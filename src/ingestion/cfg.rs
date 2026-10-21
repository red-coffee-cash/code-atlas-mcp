//! On-demand sub-function Control Flow Graph (spec §6). Nothing here is persisted: the database
//! stays at symbol level and a CFG is rebuilt from source whenever `expand_function_cfg` asks.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result, bail};
use cozo::DbInstance;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, QueryCursor};

use super::lang::{frontend_for, risk_flag_for_capture, text};
use super::walker::{Language, SourceFile};
use crate::db::queries;
use crate::mcp::batch::read_checked;

const COND_MAX: usize = 60;

/// How a statement-level node participates in control flow. Everything language-specific about
/// a CFG lives behind `CfgLang::classify`.
pub enum Stmt<'t> {
    Simple,
    Block(Node<'t>),
    If {
        cond: Node<'t>,
        then: Node<'t>,
        otherwise: Option<Node<'t>>,
    },
    /// `cond` is the loop condition or iterator; `None` for an unconditional loop.
    Loop {
        cond: Option<Node<'t>>,
        body: Node<'t>,
    },
    /// `breakable`: a `break` inside an arm leaves the match (C-style switch).
    Match {
        scrutinee: Option<Node<'t>>,
        arms: Vec<(String, Node<'t>)>,
        breakable: bool,
    },
    /// try/except/finally. Handlers are reachable from the start of the try body.
    Try {
        body: Node<'t>,
        handlers: Vec<(String, Node<'t>)>,
        orelse: Option<Node<'t>>,
        finalizer: Option<Node<'t>>,
    },
    Return,
    Break,
    Continue,
    /// panic!/throw: leaves the function abnormally.
    Panic,
}

pub trait CfgLang {
    fn body<'t>(&self, def: Node<'t>) -> Option<Node<'t>>;
    fn statements<'t>(&self, block: Node<'t>) -> Vec<Node<'t>>;
    fn classify<'t>(&self, node: Node<'t>, src: &[u8]) -> Stmt<'t>;
    /// Names assigned to or declared mutable inside `node`.
    fn mutated(&self, node: Node, src: &[u8], out: &mut Vec<String>);
    /// Whether `node` is a postfix-propagation (`?`) expression.
    fn is_try(&self, node: Node) -> bool;
    /// Whether `node` is a call that can panic on failure (unwrap/expect).
    fn is_panicking_call(&self, node: Node, src: &[u8]) -> bool;
}

pub fn cfg_lang_for(language: Language) -> Option<&'static dyn CfgLang> {
    match language {
        Language::Rust => Some(&RustCfg),
        Language::Python => Some(&PythonCfg),
        Language::TypeScript => Some(&TypeScriptCfg),
        // CFG support for these lands with their frontends; files degrade to a single opaque block.
        Language::Go | Language::Ruby => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Entry,
    Normal,
    Join,
    LoopHeader,
    Exit,
}

#[derive(Debug, Clone)]
pub struct Block {
    pub id: usize,
    pub kind: BlockKind,
    /// 1-based inclusive; `None` for blocks that hold no source (joins, exit).
    pub lines: Option<(usize, usize)>,
    pub mutated: Vec<String>,
    pub calls: Vec<String>,
    pub risks: Vec<(usize, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CfgEdge {
    pub from: usize,
    pub to: usize,
    pub label: String,
}

#[derive(Debug)]
pub struct Cfg {
    pub blocks: Vec<Block>,
    pub edges: Vec<CfgEdge>,
    pub exit: usize,
}

impl Cfg {
    /// McCabe complexity of the graph: E − N + 2.
    pub fn graph_complexity(&self) -> i64 {
        self.edges.len() as i64 - self.blocks.len() as i64 + 2
    }
}

struct Builder<'a> {
    lang: &'a dyn CfgLang,
    src: &'a [u8],
    risks: &'a [(usize, usize, &'static str)],
    blocks: Vec<Block>,
    edges: Vec<CfgEdge>,
    exit: usize,
    /// (header, after, is_switch); `continue` skips switch entries.
    loops: Vec<(usize, usize, bool)>,
}

fn clip(s: &str) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() > COND_MAX {
        format!("{}…", one_line.chars().take(COND_MAX).collect::<String>())
    } else {
        one_line
    }
}

impl<'a> Builder<'a> {
    fn new_block(&mut self, kind: BlockKind) -> usize {
        let id = self.blocks.len();
        self.blocks.push(Block { id, kind, lines: None, mutated: vec![], calls: vec![], risks: vec![] });
        id
    }

    fn edge(&mut self, from: usize, to: usize, label: impl Into<String>) {
        self.edges.push(CfgEdge { from, to, label: label.into() });
    }

    fn has_incoming(&self, id: usize) -> bool {
        self.edges.iter().any(|e| e.to == id)
    }

    /// Folds `node` into block `blk`: line range, calls, mutations, risks and the implicit
    /// abnormal exits (`?`, unwrap).
    fn absorb(&mut self, blk: usize, node: Node) {
        let (s, e) = (node.start_position().row + 1, node.end_position().row + 1);
        let b = &mut self.blocks[blk];
        b.lines = Some(b.lines.map_or((s, e), |(a, z)| (a.min(s), z.max(e))));

        let mut mutated = Vec::new();
        self.lang.mutated(node, self.src, &mut mutated);
        let (mut try_at, mut panic_at) = (false, false);
        let mut calls = Vec::new();
        crate::ingestion::parser::walk(node, |n| {
            if (n.kind() == "call_expression" || n.kind() == "call")
                && let Some(f) = n.child_by_field_name("function")
            {
                let name = text(f, self.src).rsplit(['.', ':']).next().unwrap_or("").trim().to_string();
                if !name.is_empty() {
                    calls.push(name);
                }
            }
            try_at |= self.lang.is_try(n);
            panic_at |= self.lang.is_panicking_call(n, self.src);
            true
        });
        let b = &mut self.blocks[blk];
        for m in mutated {
            if !b.mutated.contains(&m) {
                b.mutated.push(m);
            }
        }
        for c in calls {
            if !b.calls.contains(&c) {
                b.calls.push(c);
            }
        }
        for &(rs, re, flag) in self.risks {
            if rs >= node.start_byte() && re <= node.end_byte() {
                let line = self.line_of(rs);
                if !self.blocks[blk].risks.iter().any(|(l, f)| *l == line && f == flag) {
                    self.blocks[blk].risks.push((line, flag.to_string()));
                }
            }
        }
        if try_at && !self.has_edge(blk, "UNHANDLED_ERROR_PROPAGATION (?)") {
            self.edge(blk, self.exit, "UNHANDLED_ERROR_PROPAGATION (?)");
        }
        if panic_at && !self.has_edge(blk, "PANIC_OR_THROW (unwrap/expect)") {
            self.edge(blk, self.exit, "PANIC_OR_THROW (unwrap/expect)");
        }
    }

    /// Risks anchored on a wrapper node we descend through (e.g. `unsafe { .. }`) belong to the
    /// block that enters it; the inner statements never see the wrapper's range.
    fn attach_wrapper_risks(&mut self, blk: usize, wrapper: Node) {
        for &(rs, _, flag) in self.risks {
            if rs == wrapper.start_byte() {
                let line = self.line_of(rs);
                if !self.blocks[blk].risks.iter().any(|(l, f)| *l == line && f == flag) {
                    self.blocks[blk].risks.push((line, flag.to_string()));
                }
            }
        }
    }

    fn has_edge(&self, from: usize, label: &str) -> bool {
        self.edges.iter().any(|e| e.from == from && e.to == self.exit && e.label == label)
    }

    fn line_of(&self, byte: usize) -> usize {
        self.src[..byte.min(self.src.len())].iter().filter(|&&b| b == b'\n').count() + 1
    }

    fn seq(&mut self, stmts: Vec<Node>, mut cur: Option<usize>) -> Option<usize> {
        for s in stmts {
            let Some(c) = cur else { break }; // everything after return/break is unreachable
            cur = self.stmt(s, c);
        }
        cur
    }

    fn lower(&mut self, node: Node, cur: usize) -> Option<usize> {
        match self.lang.classify(node, self.src) {
            Stmt::Block(b) => {
                self.attach_wrapper_risks(cur, b);
                let children = self.lang.statements(b);
                self.seq(children, Some(cur))
            }
            _ => self.stmt(node, cur),
        }
    }

    fn stmt(&mut self, node: Node, cur: usize) -> Option<usize> {
        match self.lang.classify(node, self.src) {
            Stmt::Simple => {
                self.absorb(cur, node);
                Some(cur)
            }
            Stmt::Block(b) => {
                self.attach_wrapper_risks(cur, b);
                let children = self.lang.statements(b);
                self.seq(children, Some(cur))
            }
            Stmt::If { cond, then, otherwise } => {
                self.absorb(cur, cond);
                let label = clip(text(cond, self.src));
                let join = self.new_block(BlockKind::Join);
                let t = self.new_block(BlockKind::Normal);
                self.edge(cur, t, format!("TRUE_BRANCH({label})"));
                if let Some(end) = self.lower(then, t) {
                    self.edge(end, join, "NEXT");
                }
                match otherwise {
                    Some(o) => {
                        let f = self.new_block(BlockKind::Normal);
                        self.edge(cur, f, format!("FALSE_BRANCH({label})"));
                        if let Some(end) = self.lower(o, f) {
                            self.edge(end, join, "NEXT");
                        }
                    }
                    None => self.edge(cur, join, format!("FALSE_BRANCH({label})")),
                }
                self.has_incoming(join).then_some(join)
            }
            Stmt::Loop { cond, body } => {
                let header = self.new_block(BlockKind::LoopHeader);
                self.edge(cur, header, "NEXT");
                if let Some(c) = cond {
                    self.absorb(header, c);
                }
                let after = self.new_block(BlockKind::Join);
                let body_block = self.new_block(BlockKind::Normal);
                match cond {
                    Some(c) => {
                        let label = clip(text(c, self.src));
                        self.edge(header, body_block, format!("TRUE_BRANCH({label})"));
                        self.edge(header, after, "LOOP_EXIT");
                    }
                    None => self.edge(header, body_block, "NEXT"),
                }
                self.loops.push((header, after, false));
                let end = self.lower(body, body_block);
                self.loops.pop();
                if let Some(e) = end {
                    self.edge(e, header, "LOOP_BACKEDGE");
                }
                self.has_incoming(after).then_some(after)
            }
            Stmt::Match { scrutinee, arms, breakable } => {
                if let Some(s) = scrutinee {
                    self.absorb(cur, s);
                }
                let join = self.new_block(BlockKind::Join);
                if breakable {
                    self.loops.push((usize::MAX, join, true));
                }
                for (pattern, body) in arms {
                    let arm = self.new_block(BlockKind::Normal);
                    self.edge(cur, arm, format!("MATCH_ARM({})", clip(&pattern)));
                    if let Some(end) = self.lower(body, arm) {
                        self.edge(end, join, "NEXT");
                    }
                }
                if breakable {
                    self.loops.pop();
                }
                self.has_incoming(join).then_some(join)
            }
            Stmt::Try { body, handlers, orelse, finalizer } => {
                let join = self.new_block(BlockKind::Join);
                let try_block = self.new_block(BlockKind::Normal);
                self.edge(cur, try_block, "TRY");
                let mut normal_end = self.lower(body, try_block);
                if let (Some(e), Some(o)) = (normal_end, orelse) {
                    let else_block = self.new_block(BlockKind::Normal);
                    self.edge(e, else_block, "NEXT");
                    normal_end = self.lower(o, else_block);
                }
                if let Some(e) = normal_end {
                    self.edge(e, join, "NEXT");
                }
                for (pattern, handler) in handlers {
                    let hb = self.new_block(BlockKind::Normal);
                    self.edge(try_block, hb, format!("EXCEPTION({})", clip(&pattern)));
                    if let Some(end) = self.lower(handler, hb) {
                        self.edge(end, join, "NEXT");
                    }
                }
                if !self.has_incoming(join) {
                    return None;
                }
                match finalizer {
                    Some(f) => self.lower(f, join),
                    None => Some(join),
                }
            }
            Stmt::Return => {
                self.absorb(cur, node);
                self.edge(cur, self.exit, "EARLY_RETURN");
                None
            }
            Stmt::Panic => {
                self.absorb(cur, node);
                self.edge(cur, self.exit, "PANIC_OR_THROW");
                None
            }
            Stmt::Break => {
                self.absorb(cur, node);
                match self.loops.last().copied() {
                    Some((_, after, _)) => self.edge(cur, after, "LOOP_EXIT"),
                    None => self.edge(cur, self.exit, "EARLY_RETURN"),
                }
                None
            }
            Stmt::Continue => {
                self.absorb(cur, node);
                match self.loops.iter().rev().find(|(_, _, is_switch)| !is_switch).copied() {
                    Some((header, _, _)) => self.edge(cur, header, "LOOP_BACKEDGE"),
                    None => self.edge(cur, self.exit, "EARLY_RETURN"),
                }
                None
            }
        }
    }
}

/// Builds the CFG of `def` (a function-like node).
pub fn build(lang: &dyn CfgLang, def: Node, src: &[u8], risks: &[(usize, usize, &'static str)]) -> Result<Cfg> {
    let body = lang.body(def).context("function has no body to build a CFG from")?;
    let mut b = Builder { lang, src, risks, blocks: vec![], edges: vec![], exit: 1, loops: vec![] };
    let entry = b.new_block(BlockKind::Entry);
    let exit = b.new_block(BlockKind::Exit);
    debug_assert_eq!((entry, exit), (0, 1));
    let end = b.lower(body, entry);
    if let Some(e) = end {
        b.edge(e, exit, "RETURN");
    }
    Ok(Cfg { blocks: b.blocks, edges: b.edges, exit })
}

/// Rebuilds the symbol's CFG from current source and renders it (spec §6.1).
pub fn expand_function_cfg(db: &DbInstance, repo_root: &Path, symbol_id: &str) -> Result<String> {
    let sym = queries::symbols_by_ids(db, &[symbol_id.to_string()])?
        .remove(symbol_id)
        .with_context(|| format!("unknown symbol_id `{symbol_id}`"))?;
    if !sym.is_callable() {
        bail!("`{symbol_id}` is a {}, not a function or method; a CFG only exists for code with a body", sym.kind);
    }
    let hashes = queries::file_hashes(db)?;
    let source = read_checked(repo_root, &sym.file_path, hashes.get(&sym.file_path)).with_context(|| {
        format!("`{}` is unreadable or changed since the last sync; call sync_repository_skeleton first", sym.file_path)
    })?;

    let file = SourceFile {
        rel_path: sym.file_path.clone(),
        language: language_of(&sym.file_path),
        blake3: String::new(),
        line_count: 0,
        source,
    };
    let lang = cfg_lang_for(file.language)
        .with_context(|| format!("CFG expansion is not implemented for {} yet", file.language.as_str()))?;
    let fe = frontend_for(&file).context("no parser frontend for this language")?;
    let compiled = fe.compiled(&file)?;

    let mut parser = Parser::new();
    parser.set_language(&compiled.language)?;
    let tree = parser.parse(&file.source, None).context("tree-sitter failed to parse the file")?;
    let src = file.source.as_bytes();
    let mut def = tree
        .root_node()
        .descendant_for_byte_range(sym.start_byte, sym.end_byte)
        .context("symbol range not found in the parsed tree")?;
    while def.start_byte() != sym.start_byte || def.end_byte() != sym.end_byte {
        def = def.parent().context("symbol range does not match a syntax node; re-sync")?;
    }
    while let Some(p) = def.parent() {
        if p.start_byte() == def.start_byte() && p.end_byte() == def.end_byte() {
            def = p;
        } else {
            break;
        }
    }

    let mut risks: Vec<(usize, usize, &'static str)> = Vec::new();
    let names = compiled.risks.capture_names();
    let mut cursor = QueryCursor::new();
    cursor.set_byte_range(sym.start_byte..sym.end_byte);
    let mut matches = cursor.matches(&compiled.risks, def, src);
    while let Some(m) = matches.next() {
        for c in m.captures() {
            if let Some(flag) = names[c.index as usize].strip_prefix("risk.").and_then(risk_flag_for_capture) {
                risks.push((c.node.start_byte(), c.node.end_byte(), flag));
            }
        }
    }

    let cfg = build(lang, def, src, &risks)?;
    let internal: HashSet<String> = queries::edges_from(db, &[symbol_id.to_string()])?
        .iter()
        .filter(|e| e.kind == "CALLS" && e.confidence != "LLM_PRUNED")
        .filter_map(|e| e.to.rsplit("::").next().and_then(|t| t.split('#').next()).map(str::to_owned))
        .collect();
    Ok(render(&sym.symbol_id, sym.complexity, &cfg, &internal))
}

fn language_of(path: &str) -> Language {
    Language::from_path(Path::new(path)).unwrap_or(Language::Rust)
}

pub fn render(symbol_id: &str, ast_complexity: i64, cfg: &Cfg, internal_calls: &HashSet<String>) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# CFG: `{symbol_id}`\n{} basic blocks · {} edges · graph complexity {} (AST-counted cyclomatic: {ast_complexity})\n\
         Abnormal exits (`?`, unwrap/expect, panic) are drawn as edges to EXIT, so graph complexity can exceed the AST count.\n",
        cfg.blocks.len(),
        cfg.edges.len(),
        cfg.graph_complexity()
    );
    for b in &cfg.blocks {
        let range = match (b.kind, b.lines) {
            (BlockKind::Exit, _) => "EXIT".to_string(),
            (_, Some((s, e))) if s == e => format!("L{s}"),
            (_, Some((s, e))) => format!("L{s}-L{e}"),
            (_, None) => "(join)".to_string(),
        };
        let tag = match b.kind {
            BlockKind::Entry => " [entry]",
            BlockKind::LoopHeader => " [loop header]",
            _ => "",
        };
        let _ = writeln!(out, "## B{} {range}{tag}", b.id);
        if !b.mutated.is_empty() {
            let _ = writeln!(out, "* mutates: {}", b.mutated.join(", "));
        }
        if !b.calls.is_empty() {
            let calls: Vec<String> = b
                .calls
                .iter()
                .map(|c| format!("{c} ({})", if internal_calls.contains(c) { "repo" } else { "external/unresolved" }))
                .collect();
            let _ = writeln!(out, "* calls: {}", calls.join(", "));
        }
        for (line, flag) in &b.risks {
            let _ = writeln!(out, "* ⚠ L{line}: {flag}");
        }
        for e in cfg.edges.iter().filter(|e| e.from == b.id) {
            let to = if e.to == cfg.exit { "EXIT".to_string() } else { format!("B{}", e.to) };
            let _ = writeln!(out, "  → {to} `{}`", e.label);
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------

struct RustCfg;

fn named_children<'t>(n: Node<'t>) -> Vec<Node<'t>> {
    let mut c = n.walk();
    n.named_children(&mut c).collect()
}

impl CfgLang for RustCfg {
    fn body<'t>(&self, def: Node<'t>) -> Option<Node<'t>> {
        def.child_by_field_name("body")
    }

    fn statements<'t>(&self, block: Node<'t>) -> Vec<Node<'t>> {
        let block = if block.kind() == "unsafe_block" {
            named_children(block).into_iter().find(|c| c.kind() == "block").unwrap_or(block)
        } else {
            block
        };
        named_children(block).into_iter().filter(|c| !matches!(c.kind(), "line_comment" | "block_comment")).collect()
    }

    fn classify<'t>(&self, node: Node<'t>, src: &[u8]) -> Stmt<'t> {
        match node.kind() {
            "expression_statement" => match named_children(node).first() {
                Some(&inner) => match self.classify(inner, src) {
                    Stmt::Simple => Stmt::Simple,
                    other => other,
                },
                None => Stmt::Simple,
            },
            "let_declaration" => match node.child_by_field_name("value") {
                Some(v) if matches!(v.kind(), "if_expression" | "match_expression") => self.classify(v, src),
                _ => Stmt::Simple,
            },
            "block" | "unsafe_block" => Stmt::Block(node),
            "if_expression" => {
                let (Some(cond), Some(then)) =
                    (node.child_by_field_name("condition"), node.child_by_field_name("consequence"))
                else {
                    return Stmt::Simple;
                };
                let otherwise =
                    node.child_by_field_name("alternative").and_then(|alt| named_children(alt).into_iter().next());
                Stmt::If { cond, then, otherwise }
            }
            "while_expression" => match (node.child_by_field_name("condition"), node.child_by_field_name("body")) {
                (cond, Some(body)) => Stmt::Loop { cond, body },
                _ => Stmt::Simple,
            },
            "for_expression" => match node.child_by_field_name("body") {
                Some(body) => Stmt::Loop { cond: node.child_by_field_name("value"), body },
                None => Stmt::Simple,
            },
            "loop_expression" => match node.child_by_field_name("body") {
                Some(body) => Stmt::Loop { cond: None, body },
                None => Stmt::Simple,
            },
            "match_expression" => {
                let arms = node
                    .child_by_field_name("body")
                    .map(|b| {
                        named_children(b)
                            .into_iter()
                            .filter(|a| a.kind() == "match_arm")
                            .filter_map(|a| {
                                let pat = a
                                    .child_by_field_name("pattern")
                                    .map_or(String::new(), |p| text(p, src).to_string());
                                a.child_by_field_name("value").map(|v| (pat, v))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Stmt::Match { scrutinee: node.child_by_field_name("value"), arms, breakable: false }
            }
            "return_expression" => Stmt::Return,
            "break_expression" => Stmt::Break,
            "continue_expression" => Stmt::Continue,
            "macro_invocation" => {
                let name = node.child_by_field_name("macro").map_or("", |m| text(m, src));
                let last = name.rsplit("::").next().unwrap_or(name);
                if matches!(last, "panic" | "unreachable" | "todo" | "unimplemented") {
                    Stmt::Panic
                } else {
                    Stmt::Simple
                }
            }
            _ => Stmt::Simple,
        }
    }

    fn mutated(&self, node: Node, src: &[u8], out: &mut Vec<String>) {
        crate::ingestion::parser::walk(node, |n| {
            match n.kind() {
                "assignment_expression" | "compound_assignment_expr" => {
                    if let Some(l) = n.child_by_field_name("left") {
                        // `a.b[i] = x` mutates `a`
                        let mut root = l;
                        while let Some(inner) =
                            root.child_by_field_name("value").or_else(|| root.child_by_field_name("argument"))
                        {
                            root = inner;
                        }
                        out.push(text(root, src).to_string());
                    }
                }
                "let_declaration" => {
                    if let Some(p) = n.child_by_field_name("pattern")
                        && (p.kind() == "mut_pattern" || text(p, src).starts_with("mut "))
                    {
                        out.push(text(p, src).trim_start_matches("mut ").trim().to_string());
                    }
                }
                _ => {}
            }
            true
        });
    }

    fn is_try(&self, node: Node) -> bool {
        node.kind() == "try_expression"
    }

    fn is_panicking_call(&self, node: Node, src: &[u8]) -> bool {
        node.kind() == "call_expression"
            && node
                .child_by_field_name("function")
                .filter(|f| f.kind() == "field_expression")
                .and_then(|f| f.child_by_field_name("field"))
                .is_some_and(|m| matches!(text(m, src), "unwrap" | "expect"))
    }
}

// ---------------------------------------------------------------------------------------------

struct PythonCfg;

fn field_children<'t>(n: Node<'t>, field: &str) -> Vec<Node<'t>> {
    let mut c = n.walk();
    n.children_by_field_name(field, &mut c).collect()
}

fn root_of_target<'t>(mut n: Node<'t>) -> Node<'t> {
    while let Some(inner) = n.child_by_field_name("object").or_else(|| n.child_by_field_name("value")) {
        n = inner;
    }
    n
}

impl CfgLang for PythonCfg {
    fn body<'t>(&self, def: Node<'t>) -> Option<Node<'t>> {
        def.child_by_field_name("body")
    }

    fn statements<'t>(&self, block: Node<'t>) -> Vec<Node<'t>> {
        named_children(block).into_iter().filter(|c| c.kind() != "comment").collect()
    }

    fn classify<'t>(&self, node: Node<'t>, src: &[u8]) -> Stmt<'t> {
        match node.kind() {
            "block" => Stmt::Block(node),
            "if_statement" => match (node.child_by_field_name("condition"), node.child_by_field_name("consequence")) {
                (Some(cond), Some(then)) => {
                    Stmt::If { cond, then, otherwise: field_children(node, "alternative").into_iter().next() }
                }
                _ => Stmt::Simple,
            },
            "elif_clause" => match (node.child_by_field_name("condition"), node.child_by_field_name("consequence")) {
                (Some(cond), Some(then)) => {
                    let mut next = node.next_named_sibling();
                    while next.is_some_and(|n| n.kind() == "comment") {
                        next = next.and_then(|n| n.next_named_sibling());
                    }
                    Stmt::If {
                        cond,
                        then,
                        otherwise: next.filter(|n| matches!(n.kind(), "elif_clause" | "else_clause")),
                    }
                }
                _ => Stmt::Simple,
            },
            "else_clause" | "finally_clause" => match node
                .child_by_field_name("body")
                .or_else(|| named_children(node).into_iter().find(|c| c.kind() == "block"))
            {
                Some(b) => Stmt::Block(b),
                None => Stmt::Simple,
            },
            "while_statement" => match node.child_by_field_name("body") {
                Some(body) => Stmt::Loop { cond: node.child_by_field_name("condition"), body },
                None => Stmt::Simple,
            },
            "for_statement" => match node.child_by_field_name("body") {
                Some(body) => Stmt::Loop { cond: node.child_by_field_name("right"), body },
                None => Stmt::Simple,
            },
            "try_statement" => {
                let Some(body) = node.child_by_field_name("body") else { return Stmt::Simple };
                let mut handlers = Vec::new();
                let (mut orelse, mut finalizer) = (None, None);
                for c in named_children(node) {
                    match c.kind() {
                        "except_clause" | "except_group_clause" => {
                            let pattern = text(c, src).lines().next().unwrap_or("").trim_end_matches(':').to_string();
                            if let Some(b) = named_children(c).into_iter().rev().find(|x| x.kind() == "block") {
                                handlers.push((pattern, b));
                            }
                        }
                        "else_clause" => {
                            orelse = c
                                .child_by_field_name("body")
                                .or_else(|| named_children(c).into_iter().find(|x| x.kind() == "block"))
                        }
                        "finally_clause" => finalizer = named_children(c).into_iter().find(|x| x.kind() == "block"),
                        _ => {}
                    }
                }
                Stmt::Try { body, handlers, orelse, finalizer }
            }
            "match_statement" => {
                let arms = node
                    .child_by_field_name("body")
                    .map(|b| {
                        named_children(b)
                            .into_iter()
                            .filter(|c| c.kind() == "case_clause")
                            .filter_map(|c| {
                                let pat = named_children(c)
                                    .into_iter()
                                    .filter(|x| x.kind() == "case_pattern")
                                    .map(|x| text(x, src))
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                c.child_by_field_name("consequence").map(|body| (pat, body))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Stmt::Match { scrutinee: node.child_by_field_name("subject"), arms, breakable: false }
            }
            "with_statement" => match node.child_by_field_name("body") {
                Some(b) => Stmt::Block(b),
                None => Stmt::Simple,
            },
            "return_statement" => Stmt::Return,
            "break_statement" => Stmt::Break,
            "continue_statement" => Stmt::Continue,
            "raise_statement" => Stmt::Panic,
            _ => Stmt::Simple,
        }
    }

    fn mutated(&self, node: Node, src: &[u8], out: &mut Vec<String>) {
        crate::ingestion::parser::walk(node, |n| {
            if matches!(n.kind(), "assignment" | "augmented_assignment" | "named_expression")
                && let Some(l) = n.child_by_field_name("left").or_else(|| n.child_by_field_name("name"))
            {
                out.push(text(root_of_target(l), src).to_string());
            }
            true
        });
    }

    fn is_try(&self, _node: Node) -> bool {
        false
    }

    fn is_panicking_call(&self, _node: Node, _src: &[u8]) -> bool {
        false
    }
}

struct TypeScriptCfg;

impl CfgLang for TypeScriptCfg {
    fn body<'t>(&self, def: Node<'t>) -> Option<Node<'t>> {
        let f = match def.kind() {
            "lexical_declaration" | "variable_declaration" => {
                crate::ingestion::lang::typescript::declared_function(def)?
            }
            "public_field_definition" => def.child_by_field_name("value")?,
            _ => def,
        };
        f.child_by_field_name("body")
    }

    fn statements<'t>(&self, block: Node<'t>) -> Vec<Node<'t>> {
        let skip =
            matches!(block.kind(), "switch_case").then(|| block.child_by_field_name("value")).flatten().map(|v| v.id());
        named_children(block).into_iter().filter(|c| c.kind() != "comment" && Some(c.id()) != skip).collect()
    }

    fn classify<'t>(&self, node: Node<'t>, src: &[u8]) -> Stmt<'t> {
        match node.kind() {
            "statement_block" | "switch_case" | "switch_default" => Stmt::Block(node),
            "else_clause" => {
                named_children(node).into_iter().next().map_or(Stmt::Simple, |n| match self.classify(n, src) {
                    Stmt::Simple => Stmt::Block(node),
                    other => other,
                })
            }
            "if_statement" => match (node.child_by_field_name("condition"), node.child_by_field_name("consequence")) {
                (Some(cond), Some(then)) => Stmt::If {
                    cond,
                    then,
                    otherwise: node
                        .child_by_field_name("alternative")
                        .and_then(|alt| named_children(alt).into_iter().next()),
                },
                _ => Stmt::Simple,
            },
            "while_statement" | "do_statement" => match node.child_by_field_name("body") {
                Some(body) => Stmt::Loop { cond: node.child_by_field_name("condition"), body },
                None => Stmt::Simple,
            },
            "for_in_statement" => match node.child_by_field_name("body") {
                Some(body) => Stmt::Loop { cond: node.child_by_field_name("right"), body },
                None => Stmt::Simple,
            },
            "for_statement" => match node.child_by_field_name("body") {
                Some(body) => Stmt::Loop { cond: node.child_by_field_name("condition"), body },
                None => Stmt::Simple,
            },
            "switch_statement" => {
                let arms = node
                    .child_by_field_name("body")
                    .map(|b| {
                        named_children(b)
                            .into_iter()
                            .filter(|c| matches!(c.kind(), "switch_case" | "switch_default"))
                            .map(|c| {
                                let pat = c
                                    .child_by_field_name("value")
                                    .map_or("default".to_string(), |v| text(v, src).to_string());
                                (pat, c)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Stmt::Match { scrutinee: node.child_by_field_name("value"), arms, breakable: true }
            }
            "try_statement" => {
                let Some(body) = node.child_by_field_name("body") else { return Stmt::Simple };
                let handlers = node
                    .child_by_field_name("handler")
                    .and_then(|h| {
                        let param =
                            h.child_by_field_name("parameter").map_or(String::new(), |p| format!("({})", text(p, src)));
                        h.child_by_field_name("body").map(|b| (format!("catch {param}"), b))
                    })
                    .into_iter()
                    .collect();
                let finalizer = node.child_by_field_name("finalizer").and_then(|f| f.child_by_field_name("body"));
                Stmt::Try { body, handlers, orelse: None, finalizer }
            }
            "return_statement" => Stmt::Return,
            "break_statement" => Stmt::Break,
            "continue_statement" => Stmt::Continue,
            "throw_statement" => Stmt::Panic,
            _ => Stmt::Simple,
        }
    }

    fn mutated(&self, node: Node, src: &[u8], out: &mut Vec<String>) {
        crate::ingestion::parser::walk(node, |n| {
            match n.kind() {
                "assignment_expression" | "augmented_assignment_expression" => {
                    if let Some(l) = n.child_by_field_name("left") {
                        out.push(text(root_of_target(l), src).to_string());
                    }
                }
                "lexical_declaration" | "variable_declaration" if !text(n, src).starts_with("const") => {
                    for d in named_children(n).into_iter().filter(|c| c.kind() == "variable_declarator") {
                        if let Some(name) = d.child_by_field_name("name") {
                            out.push(text(name, src).to_string());
                        }
                    }
                }
                _ => {}
            }
            true
        });
    }

    fn is_try(&self, _node: Node) -> bool {
        false
    }

    fn is_panicking_call(&self, _node: Node, _src: &[u8]) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingestion::lang::Frontend;
    use crate::ingestion::lang::rust::RUST;

    /// Parses `src`, finds `fn name`, builds its CFG.
    fn cfg_of(src: &str, name: &str) -> Cfg {
        let file = SourceFile {
            rel_path: "t.rs".into(),
            language: Language::Rust,
            blake3: String::new(),
            line_count: 0,
            source: src.into(),
        };
        let compiled = RUST.compiled(&file).unwrap();
        let mut p = Parser::new();
        p.set_language(&compiled.language).unwrap();
        let tree = p.parse(src, None).unwrap();
        let mut def = None;
        crate::ingestion::parser::walk(tree.root_node(), |n| {
            if n.kind() == "function_item"
                && n.child_by_field_name("name").is_some_and(|x| text(x, src.as_bytes()) == name)
            {
                def = Some(n);
            }
            true
        });
        build(&RustCfg, def.expect("fn not found"), src.as_bytes(), &[]).unwrap()
    }

    fn labels(cfg: &Cfg) -> Vec<&str> {
        let mut v: Vec<&str> = cfg.edges.iter().map(|e| e.label.as_str()).collect();
        v.sort_unstable();
        v
    }

    fn has(cfg: &Cfg, prefix: &str) -> bool {
        cfg.edges.iter().any(|e| e.label.starts_with(prefix))
    }

    #[test]
    fn straight_line_code_is_one_block() {
        let c = cfg_of("fn f() {\n let mut a = 1;\n a += 2;\n g(a);\n}", "f");
        assert_eq!(c.blocks.len(), 2); // entry + exit
        assert_eq!(c.blocks[0].lines, Some((2, 4)));
        assert_eq!(c.blocks[0].mutated, ["a"]);
        assert_eq!(c.blocks[0].calls, ["g"]);
        assert_eq!(labels(&c), ["RETURN"]);
        assert_eq!(c.graph_complexity(), 1);
    }

    #[test]
    fn if_else_has_true_false_branches_and_joins() {
        let c =
            cfg_of("fn f(x: i32) -> i32 {\n let mut y = 0;\n if x > 0 {\n y = 1;\n } else {\n y = 2;\n }\n y\n}", "f");
        assert!(has(&c, "TRUE_BRANCH(x > 0)") && has(&c, "FALSE_BRANCH(x > 0)"));
        assert_eq!(c.graph_complexity(), 2);
        let then_block = c.blocks.iter().find(|b| b.lines == Some((4, 4))).unwrap();
        assert_eq!(then_block.mutated, ["y"]);
        // the join carries the tail expression `y`... which lands in the join block
        assert!(c.blocks.iter().any(|b| b.kind == BlockKind::Join && b.lines == Some((8, 8))));
    }

    #[test]
    fn else_if_chain_and_if_without_else() {
        let c = cfg_of("fn f(x: i32) {\n if x > 0 { a(); } else if x < 0 { b(); }\n c();\n}", "f");
        assert_eq!(c.edges.iter().filter(|e| e.label.starts_with("TRUE_BRANCH")).count(), 2);
        assert_eq!(c.edges.iter().filter(|e| e.label.starts_with("FALSE_BRANCH")).count(), 2);
        assert_eq!(c.graph_complexity(), 3);
    }

    #[test]
    fn loops_have_backedges_exits_break_and_continue() {
        let c = cfg_of(
            "fn f(v: Vec<i32>) -> i32 {\n let mut n = 0;\n for x in &v {\n  if *x == 0 { continue; }\n  if *x < 0 { break; }\n  n += x;\n }\n n\n}",
            "f",
        );
        assert!(c.blocks.iter().any(|b| b.kind == BlockKind::LoopHeader));
        assert!(has(&c, "LOOP_EXIT"));
        let backedges = c.edges.iter().filter(|e| e.label == "LOOP_BACKEDGE").count();
        assert!(backedges >= 2, "continue + normal body end -> header: {backedges}");
        assert_eq!(c.graph_complexity(), 4); // for + 2 ifs + 1
    }

    #[test]
    fn infinite_loop_exits_only_through_break_or_return() {
        let c = cfg_of("fn f() {\n loop {\n  if done() { break; }\n }\n after();\n}", "f");
        assert!(c.blocks.iter().any(|b| b.calls == ["after"]));
        let no_exit = cfg_of("fn g() -> ! {\n loop { spin(); }\n}", "g");
        assert!(!no_exit.edges.iter().any(|e| e.to == no_exit.exit), "never returns");
    }

    #[test]
    fn match_arms_and_early_return() {
        let c = cfg_of(
            "fn f(o: Option<i32>) -> i32 {\n match o {\n  Some(v) if v > 1 => v,\n  Some(_) => return 0,\n  None => 5,\n }\n}",
            "f",
        );
        assert_eq!(c.edges.iter().filter(|e| e.label.starts_with("MATCH_ARM")).count(), 3);
        assert!(has(&c, "MATCH_ARM(Some(v) if v > 1)"));
        assert!(has(&c, "EARLY_RETURN"));
        assert!(has(&c, "RETURN"));
    }

    #[test]
    fn question_mark_unwrap_and_panic_edges_reach_exit() {
        let c = cfg_of(
            "fn f(s: &str) -> Result<i32, E> {\n let n = s.parse::<i32>()?;\n let m = g(n).unwrap();\n if m == 0 { panic!(\"zero\"); }\n Ok(m)\n}",
            "f",
        );
        assert!(has(&c, "UNHANDLED_ERROR_PROPAGATION (?)"));
        assert!(has(&c, "PANIC_OR_THROW (unwrap/expect)"));
        assert!(c.edges.iter().any(|e| e.label == "PANIC_OR_THROW" && e.to == c.exit));
    }

    #[test]
    fn code_after_return_is_unreachable_and_skipped() {
        let c = cfg_of("fn f() -> i32 {\n return 1;\n dead();\n}", "f");
        assert!(c.blocks.iter().all(|b| !b.calls.contains(&"dead".to_string())));
    }

    #[test]
    fn let_bound_if_and_match_expressions_branch() {
        let c = cfg_of("fn f(x: i32) -> i32 {\n let y = if x > 0 { 1 } else { 2 };\n y\n}", "f");
        assert!(has(&c, "TRUE_BRANCH(x > 0)") && has(&c, "FALSE_BRANCH(x > 0)"));
    }

    #[test]
    fn render_marks_repo_vs_external_calls_and_risk_lines() {
        let src = "fn f(p: *const u8) -> u8 {\n helper();\n other();\n unsafe { *p }\n}";
        let file = SourceFile {
            rel_path: "t.rs".into(),
            language: Language::Rust,
            blake3: String::new(),
            line_count: 0,
            source: src.into(),
        };
        let compiled = RUST.compiled(&file).unwrap();
        let mut p = Parser::new();
        p.set_language(&compiled.language).unwrap();
        let tree = p.parse(src, None).unwrap();
        let def = tree.root_node().child(0).unwrap();
        let unsafe_at = src.find("unsafe {").unwrap();
        let cfg = build(&RustCfg, def, src.as_bytes(), &[(unsafe_at, src.len() - 2, "unsafe_usage")]).unwrap();
        let md = render("t.rs::::f#fn:0000", 1, &cfg, &HashSet::from(["helper".to_string()]));
        assert!(md.contains("calls: helper (repo), other (external/unresolved)"), "{md}");
        assert!(md.contains("⚠ L4: unsafe_usage"), "{md}");
        assert!(md.contains("## B0 L2-L4 [entry]") && md.contains("## B1 EXIT"));
    }

    // ---- Python and TypeScript ------------------------------------------------------------

    fn cfg_in(path: &str, language: Language, src: &str, kind_names: &[&str], name: &str) -> Cfg {
        let file =
            SourceFile { rel_path: path.into(), language, blake3: String::new(), line_count: 0, source: src.into() };
        let fe = frontend_for(&file).unwrap();
        let compiled = fe.compiled(&file).unwrap();
        let mut p = Parser::new();
        p.set_language(&compiled.language).unwrap();
        let tree = p.parse(src, None).unwrap();
        let mut def = None;
        crate::ingestion::parser::walk(tree.root_node(), |n| {
            let named = n.child_by_field_name("name").is_some_and(|x| text(x, src.as_bytes()) == name)
                || (matches!(n.kind(), "lexical_declaration" | "variable_declaration")
                    && n.named_child(0)
                        .and_then(|d| d.child_by_field_name("name"))
                        .is_some_and(|x| text(x, src.as_bytes()) == name));
            if kind_names.contains(&n.kind()) && named {
                def = Some(n);
            }
            true
        });
        build(cfg_lang_for(language).unwrap(), def.expect("def not found"), src.as_bytes(), &[]).unwrap()
    }

    fn py(src: &str, name: &str) -> Cfg {
        cfg_in("t.py", Language::Python, src, &["function_definition"], name)
    }

    fn ts(src: &str, name: &str) -> Cfg {
        cfg_in("t.ts", Language::TypeScript, src, &["function_declaration", "lexical_declaration"], name)
    }

    #[test]
    fn python_if_elif_else_chain() {
        let c = py(
            "def f(a, b):\n    if a:\n        x()\n    elif b:\n        y()\n    else:\n        z()\n    return 1\n",
            "f",
        );
        assert_eq!(c.edges.iter().filter(|e| e.label.starts_with("TRUE_BRANCH")).count(), 2, "{:?}", labels(&c));
        assert_eq!(c.edges.iter().filter(|e| e.label.starts_with("FALSE_BRANCH")).count(), 2);
        for call in ["x", "y", "z"] {
            assert!(c.blocks.iter().any(|b| b.calls == [call]), "{call}");
        }
        assert_eq!(c.graph_complexity(), 3);
    }

    #[test]
    fn python_loops_break_continue_and_raise() {
        let c = py(
            "def f(xs):\n    for x in xs:\n        if x < 0:\n            continue\n        if x == 0:\n            break\n        if x > 9:\n            raise ValueError()\n    while pending():\n        step()\n    return 0\n",
            "f",
        );
        assert!(c.blocks.iter().filter(|b| b.kind == BlockKind::LoopHeader).count() == 2);
        assert!(has(&c, "LOOP_EXIT") && has(&c, "LOOP_BACKEDGE") && has(&c, "PANIC_OR_THROW"));
        assert!(has(&c, "EARLY_RETURN"));
    }

    #[test]
    fn python_try_except_else_finally() {
        let c = py(
            "def f():\n    try:\n        a()\n    except ValueError as e:\n        b()\n        return 1\n    except KeyError:\n        c()\n    else:\n        d()\n    finally:\n        cleanup()\n    done()\n",
            "f",
        );
        assert_eq!(c.edges.iter().filter(|e| e.label.starts_with("EXCEPTION(")).count(), 2, "{:?}", labels(&c));
        assert!(has(&c, "TRY"));
        assert!(has(&c, "EARLY_RETURN"));
        // finally and the code after it are on a path reachable from the normal and the KeyError exits
        let cleanup = c.blocks.iter().find(|b| b.calls.contains(&"cleanup".to_string())).expect("finalizer block");
        assert!(c.edges.iter().any(|e| e.to == cleanup.id));
        assert!(c.blocks.iter().any(|b| b.calls.contains(&"done".to_string())));
    }

    #[test]
    fn python_match_with_and_mutation() {
        let c = py(
            "def f(cmd):\n    total = 0\n    total += 1\n    self.n = total\n    with open(cmd) as fh:\n        read(fh)\n    match cmd:\n        case 'a':\n            go()\n        case _:\n            stop()\n",
            "f",
        );
        assert_eq!(c.edges.iter().filter(|e| e.label.starts_with("MATCH_ARM")).count(), 2, "{:?}", labels(&c));
        assert_eq!(c.blocks[0].mutated, ["total", "self"]);
        assert!(c.blocks.iter().any(|b| b.calls.contains(&"read".to_string())));
    }

    #[test]
    fn typescript_break_in_switch_stays_in_the_switch_but_continue_reaches_the_loop() {
        let c = ts(
            "function f(xs: number[]) {\n  for (const x of xs) {\n    switch (x) {\n      case 1: a(); break;\n      case 2: continue;\n      default: b();\n    }\n    after();\n  }\n}\n",
            "f",
        );
        assert_eq!(c.edges.iter().filter(|e| e.label.starts_with("MATCH_ARM")).count(), 3, "{:?}", labels(&c));
        // `after()` is reached from the break arm and from default fallthrough
        let after = c.blocks.iter().find(|b| b.calls.contains(&"after".to_string())).expect("after block");
        let into_after_region = c.edges.iter().filter(|e| e.label == "LOOP_EXIT").count();
        assert!(into_after_region >= 2, "break (switch exit) + loop exit: {:?}", labels(&c));
        assert!(
            c.edges.iter().any(|e| e.label == "LOOP_BACKEDGE" && e.from != after.id),
            "continue reaches the loop header"
        );
    }

    #[test]
    fn typescript_try_catch_finally_and_throw() {
        let c = ts(
            "function f() {\n  try {\n    risky();\n    if (bad()) { throw new Error('x'); }\n  } catch (e) {\n    log(e);\n  } finally {\n    release();\n  }\n  done();\n}\n",
            "f",
        );
        assert!(has(&c, "TRY") && has(&c, "EXCEPTION(catch (e))") && has(&c, "PANIC_OR_THROW"));
        assert!(c.blocks.iter().any(|b| b.calls.contains(&"release".to_string())));
        assert!(c.blocks.iter().any(|b| b.calls.contains(&"done".to_string())));
    }

    #[test]
    fn typescript_const_arrow_functions_with_block_and_expression_bodies() {
        let c = ts("const f = (a: number) => {\n  let n = 0;\n  if (a) { n = 1; }\n  return n;\n};\n", "f");
        assert!(has(&c, "TRUE_BRANCH(") && has(&c, "EARLY_RETURN"));
        assert_eq!(c.blocks[0].mutated, ["n"]);
        let e = ts("const g = (a: number) => helper(a);\n", "g");
        assert!(e.blocks[0].calls == ["helper"]);
    }
}
