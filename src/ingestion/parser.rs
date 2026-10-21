//! Generic tree-sitter extraction engine (spec §4). Language specifics live in `lang/`.

use std::collections::HashSet;

use anyhow::{Context, Result};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor};

use super::lang::{Frontend, frontend_for, risk_flag_for_capture, text};
use super::model::*;
use super::symbol_id;
use super::walker::SourceFile;

/// Strict-contract auto-promotion threshold (spec §4.3).
const STRICT_COMPLEXITY: u32 = 5;
const SNIPPET_MAX: usize = 120;

/// Pre-order walk. `visit` returns false to skip a node's children. Iterative so pathological
/// nesting can't overflow the stack.
pub fn walk<'t>(root: Node<'t>, mut visit: impl FnMut(Node<'t>) -> bool) {
    let mut cur = root.walk();
    loop {
        if visit(cur.node()) && cur.goto_first_child() {
            continue;
        }
        loop {
            if cur.goto_next_sibling() {
                break;
            }
            if !cur.goto_parent() {
                return;
            }
        }
    }
}

/// `None` when the file's language has no frontend yet.
pub fn parse_file(file: &SourceFile) -> Result<Option<ParsedFile>> {
    let Some(fe) = frontend_for(file) else {
        return Ok(None);
    };
    let compiled = fe.compiled(file)?;
    let mut parser = Parser::new();
    parser.set_language(&compiled.language)?;
    let tree =
        parser.parse(&file.source, None).with_context(|| format!("tree-sitter failed to parse {}", file.rel_path))?;
    let src = file.source.as_bytes();
    let root = tree.root_node();

    let mut symbols = extract_symbols(fe, &compiled.symbols, root, src, file);
    let spans: Vec<Span> = symbols.iter().map(|s| s.span).collect();

    let mut out = ParsedFile {
        rel_path: file.rel_path.clone(),
        language: file.language,
        symbols: Vec::new(),
        calls: Vec::new(),
        type_refs: Vec::new(),
        imports: Vec::new(),
        impls: fe.implements(root, src),
        package_path: None,
        facts: fe.type_facts(root, src),
    };

    apply_risks(&compiled.risks, root, src, &mut symbols, &spans);
    extract_calls(fe, &compiled.calls, root, src, &mut symbols, &spans, &mut out);

    for s in &mut symbols {
        s.sym.risk_flags.sort();
        s.sym.risk_flags.dedup();
        s.sym.requires_strict_contract =
            s.sym.cyclomatic_complexity >= STRICT_COMPLEXITY || !s.sym.risk_flags.is_empty() || s.sym.is_entrypoint;
    }
    out.symbols = symbols.into_iter().map(|s| s.sym).collect();
    Ok(Some(out))
}

#[derive(Clone, Copy)]
struct Span {
    start: usize,
    end: usize,
    params: Option<(usize, usize)>,
    ret: Option<(usize, usize)>,
}

struct Work {
    sym: ParsedSymbol,
    span: Span,
}

fn range(n: Option<Node>) -> Option<(usize, usize)> {
    n.map(|n| (n.start_byte(), n.end_byte()))
}

fn extract_symbols(fe: &dyn Frontend, query: &Query, root: Node, src: &[u8], file: &SourceFile) -> Vec<Work> {
    let names = query.capture_names();
    let mut defs: Vec<(Node, Node)> = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, root, src);
    while let Some(m) = matches.next() {
        let mut def = None;
        let mut name = None;
        for c in m.captures() {
            match names[c.index as usize] {
                "symbol.def" => def = Some(c.node),
                "symbol.name" => name = Some(c.node),
                _ => {}
            }
        }
        if let (Some(d), Some(n)) = (def, name) {
            defs.push((d, n));
        }
    }
    defs.sort_by_key(|(d, _)| (d.start_byte(), std::cmp::Reverse(d.end_byte())));
    defs.dedup_by_key(|(d, _)| d.id());

    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (def, name_node) in defs {
        let Some(kind) = fe.symbol_kind(def, src) else {
            continue;
        };
        let name = text(name_node, src).to_string();
        let scope = fe.scope_chain(def, src);
        let scope_refs: Vec<&str> = scope.iter().map(String::as_str).collect();
        let params = fe.param_types(def, src);
        let mut param_refs: Vec<&str> = params.iter().map(String::as_str).collect();

        let mut symbol_id = symbol_id::build(&file.rel_path, &scope_refs, &name, kind, &param_refs);
        if !seen.insert(symbol_id.clone()) {
            // Same name and arity in one scope (e.g. cfg-gated twins): disambiguate by position.
            let marker = format!("@L{}", def.start_position().row + 1);
            param_refs.push(&marker);
            symbol_id = symbol_id::build(&file.rel_path, &scope_refs, &name, kind, &param_refs);
            seen.insert(symbol_id.clone());
        }

        let is_entrypoint = fe.is_entrypoint(def, src);
        let qualified_name = if scope.is_empty() { name.clone() } else { format!("{}::{name}", scope.join("::")) };

        out.push(Work {
            span: Span {
                start: def.start_byte(),
                end: def.end_byte(),
                params: range(fe.function_node(def).child_by_field_name("parameters")),
                ret: range(
                    fe.function_node(def)
                        .child_by_field_name("return_type")
                        .or_else(|| fe.function_node(def).child_by_field_name("result")),
                ),
            },
            sym: ParsedSymbol {
                symbol_id,
                qualified_name,
                name,
                kind,
                visibility: fe.visibility(def, src),
                file_path: file.rel_path.clone(),
                start_byte: def.start_byte(),
                end_byte: def.end_byte(),
                start_line: def.start_position().row + 1,
                end_line: def.end_position().row + 1,
                signature: signature(fe, def, src, kind),
                doc_comment: fe.doc_comment(def, src),
                ast_blake3: ast_hash(def, src),
                cyclomatic_complexity: complexity(fe, def, src),
                risk_flags: Vec::new(),
                requires_strict_contract: false,
                is_entrypoint,
                arity: fe.arity(def, src),
            },
        });
    }
    out
}

/// Declaration header without the body.
fn signature(fe: &dyn Frontend, def: Node, src: &[u8], kind: &str) -> String {
    let end = fe.function_node(def).child_by_field_name("body").map_or(def.end_byte(), |b| b.start_byte());
    let raw = String::from_utf8_lossy(&src[def.start_byte()..end]);
    // A const/static initialiser isn't part of the declaration header; a function's default
    // parameters (`x: int = 1`) are.
    let header = if matches!(kind, "const" | "global") { raw.split('=').next().unwrap_or(&raw) } else { &raw };
    header.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// BLAKE3 over comment- and whitespace-free leaf tokens (spec §4.2). Hashes the node kind
/// *name* rather than `kind_id()` so hashes survive grammar upgrades.
fn ast_hash(def: Node, src: &[u8]) -> String {
    let mut hasher = blake3::Hasher::new();
    walk(def, |n| {
        if n.is_extra() {
            return false;
        }
        if n.child_count() == 0 {
            hasher.update(n.kind().as_bytes());
            hasher.update(&[0]);
            hasher.update(&src[n.byte_range()]);
            hasher.update(&[0]);
        }
        true
    });
    hasher.finalize().to_hex().to_string()
}

fn complexity(fe: &dyn Frontend, def: Node, src: &[u8]) -> u32 {
    let scope = fe.function_node(def).child_by_field_name("body").unwrap_or(def);
    let mut branches = 0;
    walk(scope, |n| {
        if n.id() != scope.id() && fe.is_nested_def(n) {
            return false;
        }
        if fe.is_branch(n, src) {
            branches += 1;
        }
        true
    });
    1 + branches
}

/// Index of the smallest symbol containing `byte`.
fn innermost(spans: &[Span], byte: usize) -> Option<usize> {
    spans
        .iter()
        .enumerate()
        .filter(|(_, s)| s.start <= byte && byte < s.end)
        .min_by_key(|(_, s)| s.end - s.start)
        .map(|(i, _)| i)
}

fn apply_risks(query: &Query, root: Node, src: &[u8], symbols: &mut [Work], spans: &[Span]) {
    let names = query.capture_names();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, root, src);
    while let Some(m) = matches.next() {
        for c in m.captures() {
            let Some(suffix) = names[c.index as usize].strip_prefix("risk.") else {
                continue;
            };
            let (Some(flag), Some(i)) = (risk_flag_for_capture(suffix), innermost(spans, c.node.start_byte())) else {
                continue;
            };
            symbols[i].sym.risk_flags.push(flag.to_string());
        }
    }
}

fn snippet(node: Node, src: &[u8]) -> String {
    let t = text(node, src).split_whitespace().collect::<Vec<_>>().join(" ");
    t.chars().take(SNIPPET_MAX).collect()
}

fn last_segment(t: &str) -> &str {
    t.rsplit([':', '.']).next().unwrap_or(t).trim_end_matches('!')
}

/// The object a Ruby symbol ref names a method of. `obj.send(:x)` / `obj.try(:x)` -> `obj`;
/// `send(:x)` and `before_action :x` -> self (`None`); `xs.map(&:x)` calls `x` on each element of
/// `xs`, never on self, so it gets the placeholder `_` unless the collection has a name.
fn symbol_ref_receiver(sym: Node, src: &[u8]) -> Option<String> {
    let parent = sym.parent()?;
    let to_proc = parent.kind() == "block_argument";
    let call = if to_proc { parent.parent()?.parent()? } else { parent.parent()? };
    let named = call.child_by_field_name("receiver").map(|r| snippet(r, src));
    if to_proc { Some(named.filter(|r| r != "self").unwrap_or_else(|| "_".to_string())) } else { named }
}

fn extract_calls(
    fe: &dyn Frontend,
    query: &Query,
    root: Node,
    src: &[u8],
    symbols: &mut [Work],
    spans: &[Span],
    out: &mut ParsedFile,
) {
    let names = query.capture_names();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, root, src);
    let mut macro_seen: HashSet<(usize, String)> = HashSet::new();

    while let Some(m) = matches.next() {
        let cap = |name: &str| m.captures().iter().find(|c| names[c.index as usize] == name).map(|c| c.node);

        if let Some(path) = cap("import.path") {
            fe.flatten_import(path, src, &mut out.imports);
            continue;
        }
        if let Some(t) = cap("type.reference") {
            let Some(i) = innermost(spans, t.start_byte()) else {
                continue;
            };
            let (b, s) = (t.start_byte(), &spans[i]);
            let within = |r: Option<(usize, usize)>| r.is_some_and(|(a, z)| a <= b && b < z);
            let kind = if within(s.ret) {
                TypeRefKind::Return
            } else if within(s.params) {
                TypeRefKind::Param
            } else {
                continue;
            };
            out.type_refs.push(TypeRef {
                from_symbol_id: symbols[i].sym.symbol_id.clone(),
                type_name: text(t, src).to_string(),
                kind,
                qualifier: t
                    .parent()
                    .filter(|p| p.kind() == "qualified_type")
                    .and_then(|p| p.child_by_field_name("package"))
                    .map(|q| text(q, src).to_string()),
            });
            continue;
        }

        let Some(anchor) = cap("call.site").or_else(|| cap("call.callback_ref")).or_else(|| cap("macro.site")) else {
            continue;
        };
        let Some(i) = innermost(spans, anchor.start_byte()) else {
            continue;
        };
        let from = symbols[i].sym.symbol_id.clone();
        let line = anchor.start_position().row + 1;

        if let (Some(site), Some(target)) = (cap("call.site"), cap("call.target_name")) {
            out.calls.push(CallSite {
                from_symbol_id: from,
                target_name: text(target, src).to_string(),
                receiver: cap("call.receiver").map(|r| snippet(r, src)),
                line,
                snippet: snippet(site, src),
                kind: CallKind::Direct,
                via: None,
                receiver_type: cap("call.receiver").and_then(|r| fe.receiver_type(r, src)),
                args: fe.call_args(site, src),
                arg_types: fe.call_arg_types(site, src),
            });
        } else if let Some(r) = cap("call.callback_ref") {
            // `Self::handler`, `self.handler`, `mod.handler` carry a qualifier; a bare `handler`
            // can only be a free function (or a local variable).
            let qualifier = r.parent().and_then(|p| {
                matches!(p.kind(), "scoped_identifier" | "attribute" | "member_expression" | "selector_expression")
                    .then(|| {
                        p.child_by_field_name("path")
                            .or_else(|| p.child_by_field_name("object"))
                            .or_else(|| p.child_by_field_name("operand"))
                    })
                    .flatten()
            });
            let symbol_ref = r.kind() == "simple_symbol";
            let receiver = if symbol_ref { symbol_ref_receiver(r, src) } else { qualifier.map(|q| snippet(q, src)) };
            out.calls.push(CallSite {
                from_symbol_id: from,
                target_name: text(r, src).trim_start_matches(':').to_string(),
                receiver,
                line,
                snippet: snippet(r.parent().unwrap_or(r), src),
                kind: if symbol_ref { CallKind::SymbolRef } else { CallKind::Callback },
                via: cap("call.callback_via").map(|v| last_segment(text(v, src)).to_string()),
                receiver_type: if symbol_ref { None } else { qualifier.and_then(|q| fe.receiver_type(q, src)) },
                args: None,
                arg_types: Vec::new(),
            });
        } else if let (Some(name), Some(tokens)) = (cap("macro.name"), cap("macro.tokens")) {
            let macro_name = last_segment(text(name, src)).to_string();
            if fe.macro_is_risky(&macro_name, tokens, src) {
                symbols[i].sym.risk_flags.push("macro_invocation".to_string());
            }
            walk(tokens, |n| {
                // `foo(..)` inside a macro is a call; a bare `foo` is almost always a variable.
                let call_like =
                    n.next_sibling().is_some_and(|s| s.kind() == "token_tree" && text(s, src).starts_with('('));
                if n.kind() == "identifier" && call_like && macro_seen.insert((i, text(n, src).to_string())) {
                    out.calls.push(CallSite {
                        from_symbol_id: from.clone(),
                        target_name: text(n, src).to_string(),
                        receiver: None,
                        line: n.start_position().row + 1,
                        snippet: format!("{macro_name}!(…{}…)", text(n, src)),
                        kind: CallKind::MacroIdent,
                        via: Some(macro_name.clone()),
                        receiver_type: None,
                        args: None,
                        arg_types: Vec::new(),
                    });
                }
                true
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingestion::walker::Language;

    pub fn rust_file(src: &str) -> SourceFile {
        SourceFile {
            rel_path: "src/lib.rs".into(),
            language: Language::Rust,
            blake3: String::new(),
            line_count: src.lines().count(),
            source: src.into(),
        }
    }

    fn parse(src: &str) -> ParsedFile {
        parse_file(&rust_file(src)).unwrap().unwrap()
    }

    fn sym<'a>(p: &'a ParsedFile, name: &str) -> &'a ParsedSymbol {
        p.symbols.iter().find(|s| s.name == name).unwrap_or_else(|| panic!("no symbol {name}"))
    }

    #[test]
    fn extracts_symbols_kinds_ids_and_visibility() {
        let p = parse(
            r#"
/// Adds numbers.
pub fn add(a: i32, b: i32) -> i32 { a + b }
struct Point { x: i32 }
pub(crate) enum E { A }
const MAX: usize = 3;
static GLOBAL: u8 = 1;
trait Shape { fn area(&self) -> f64; }
impl Point { pub fn new(x: i32) -> Self { Point { x } } fn hidden(&self) {} }
impl Shape for Point { fn area(&self) -> f64 { 0.0 } }
"#,
        );
        assert_eq!(sym(&p, "add").kind, "fn");
        assert_eq!(sym(&p, "add").visibility, "pub");
        assert_eq!(sym(&p, "add").doc_comment, "Adds numbers.");
        assert_eq!(sym(&p, "add").signature, "pub fn add(a: i32, b: i32) -> i32");
        assert_eq!(sym(&p, "Point").kind, "struct");
        assert_eq!(sym(&p, "E").visibility, "crate");
        assert_eq!(sym(&p, "MAX").kind, "const");
        assert_eq!(sym(&p, "MAX").signature, "const MAX: usize");
        assert_eq!(sym(&p, "GLOBAL").kind, "global");
        assert_eq!(sym(&p, "Shape").kind, "interface");
        assert_eq!(sym(&p, "new").kind, "method");
        assert_eq!(sym(&p, "hidden").visibility, "private");

        let area_impl = p
            .symbols
            .iter()
            .find(|s| s.name == "area" && s.symbol_id.contains("Shape_for_Point"))
            .expect("trait impl method");
        assert!(area_impl.symbol_id.starts_with("src/lib.rs::Shape_for_Point::area#method:"));
        assert_eq!(area_impl.visibility, "pub");
        assert!(sym(&p, "add").symbol_id.starts_with("src/lib.rs::::add#fn:"));
    }

    #[test]
    fn duplicate_ids_are_disambiguated() {
        let p = parse("#[cfg(unix)]\nfn f() {}\n#[cfg(windows)]\nfn f() {}\n");
        let ids: HashSet<_> = p.symbols.iter().map(|s| &s.symbol_id).collect();
        assert_eq!(p.symbols.len(), 2);
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn complexity_counts_branches_but_not_nested_fns() {
        let p = parse(
            r#"
fn c(x: i32, y: bool) -> i32 {
    fn inner(z: i32) { if z > 0 { } }
    if x > 0 && y { 1 } else if x < 0 || y { 2 } else { 3 }
}
fn d(v: Vec<i32>) -> Option<i32> {
    for _ in &v {}
    while false {}
    match v.len() { 0 => None, _ => Some(v.first().copied()?) }
}
"#,
        );
        // if, &&, else-if, ||
        assert_eq!(sym(&p, "c").cyclomatic_complexity, 5);
        assert_eq!(sym(&p, "inner").cyclomatic_complexity, 2);
        // for, while, 2 arms, ?
        assert_eq!(sym(&p, "d").cyclomatic_complexity, 6);
    }

    #[test]
    fn ast_hash_ignores_comments_and_whitespace_but_not_logic() {
        let a = parse("fn f(x: i32) -> i32 { x + 1 }");
        let b = parse("// hi\nfn f( x : i32 ) -> i32 {\n    /* c */ x   +   1\n}");
        let c = parse("fn f(x: i32) -> i32 { x + 2 }");
        assert_eq!(sym(&a, "f").ast_blake3, sym(&b, "f").ast_blake3);
        assert_ne!(sym(&a, "f").ast_blake3, sym(&c, "f").ast_blake3);
    }

    #[test]
    fn risk_flags_and_strict_contract_promotion() {
        let p = parse(
            r#"
fn risky(p: *const u8) -> u8 { unsafe { *p } }
fn unwrapper(o: Option<u8>) -> u8 { o.unwrap() }
fn runner() { std::process::Command::new("ls"); let m = Mutex::new(1); }
fn calm() {}
unsafe fn declared_unsafe() {}
"#,
        );
        assert_eq!(sym(&p, "risky").risk_flags, ["unsafe_usage"]);
        assert_eq!(sym(&p, "unwrapper").risk_flags, ["unchecked_unwrap"]);
        assert!(sym(&p, "declared_unsafe").risk_flags.contains(&"unsafe_usage".to_string()));
        assert!(sym(&p, "risky").requires_strict_contract);
        assert!(!sym(&p, "calm").requires_strict_contract);
        let r = &sym(&p, "runner").risk_flags;
        assert!(r.contains(&"concurrency_primitive".to_string()), "{r:?}");
    }

    #[test]
    fn entrypoints_are_detected() {
        let p = parse(
            r#"
fn main() {}
#[tokio::main]
async fn boot() {}
#[get("/x")]
async fn route_handler() {}
pub extern "C" fn ffi_export() {}
fn plain() {}
"#,
        );
        for n in ["main", "boot", "route_handler", "ffi_export"] {
            assert!(sym(&p, n).is_entrypoint, "{n}");
            assert!(sym(&p, n).requires_strict_contract, "{n}");
        }
        assert!(!sym(&p, "plain").is_entrypoint);
    }

    #[test]
    fn calls_callbacks_macros_and_types() {
        let p = parse(
            r#"
use std::collections::{HashMap, hash_map::Entry as E};
use crate::auth::*;
fn handler(r: Request) -> Response { helper(1); r.validate(); auth::check(r); Response }
fn setup() { let app = route("/login", get(handler)); v.iter().map(transform); tokio::select! { x = fetch() => {} } }
"#,
        );
        let handler = sym(&p, "handler").symbol_id.clone();
        let setup = sym(&p, "setup").symbol_id.clone();
        let find = |from: &str, name: &str| p.calls.iter().find(|c| c.from_symbol_id == from && c.target_name == name);
        assert_eq!(find(&handler, "helper").unwrap().kind, CallKind::Direct);
        assert_eq!(find(&handler, "validate").unwrap().receiver.as_deref(), Some("r"));
        assert_eq!(find(&handler, "check").unwrap().receiver.as_deref(), Some("auth"));
        let cb = find(&setup, "handler").unwrap();
        assert_eq!(cb.kind, CallKind::Callback);
        assert_eq!(cb.via.as_deref(), Some("get"));
        assert_eq!(find(&setup, "transform").unwrap().via.as_deref(), Some("map"));
        assert_eq!(find(&setup, "fetch").unwrap().kind, CallKind::MacroIdent);

        assert!(p.type_refs.iter().any(|t| t.type_name == "Request" && t.kind == TypeRefKind::Param));
        assert!(p.type_refs.iter().any(|t| t.type_name == "Response" && t.kind == TypeRefKind::Return));

        let imp = |m: &str, n: &str| p.imports.iter().any(|i| i.module_path == m && i.name == n);
        assert!(imp("std::collections", "HashMap"));
        assert!(imp("std::collections::hash_map", "Entry"));
        assert!(imp("crate::auth", "*"));
        assert_eq!(p.imports.iter().find(|i| i.name == "Entry").unwrap().alias.as_deref(), Some("E"));
    }

    #[test]
    fn macro_wrapping_unsafe_is_flagged() {
        let p = parse("fn f() { my_macro!(unsafe { 1 }); }\nfn g() { println!(\"x\"); }");
        assert!(sym(&p, "f").risk_flags.contains(&"macro_invocation".to_string()));
        assert!(!sym(&p, "g").risk_flags.contains(&"macro_invocation".to_string()));
    }
}
