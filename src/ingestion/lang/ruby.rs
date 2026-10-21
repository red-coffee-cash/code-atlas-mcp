use std::sync::OnceLock;

use anyhow::Result;
use tree_sitter::Node;

use super::{Compiled, Frontend, own_line, text};
use crate::ingestion::model::{Arity, CallArgs, ImplRel, ImportDecl, TypeExpr, TypeFacts};
use crate::ingestion::walker::SourceFile;

pub struct RubyFrontend {
    compiled: OnceLock<Result<Compiled, String>>,
}

pub static RUBY: RubyFrontend = RubyFrontend { compiled: OnceLock::new() };

const CONTROLLER_BASES: &[&str] = &["ApplicationController", "ActionController::Base", "ActionController::API"];
const JOB_BASES: &[&str] = &["ApplicationJob", "ActiveJob::Base", "Sidekiq::Worker", "Sidekiq::Job"];
/// Comments that configure the interpreter or a type checker rather than document the code.
const MAGIC_COMMENTS: &[&str] = &["frozen_string_literal:", "typed:", "encoding:", "coding:", "-*-", "warn_indent:"];

impl Frontend for RubyFrontend {
    fn compiled(&self, _file: &SourceFile) -> Result<&Compiled> {
        self.compiled
            .get_or_init(|| {
                Compiled::build(
                    tree_sitter_ruby::LANGUAGE.into(),
                    include_str!("../../../queries/ruby/symbols.scm"),
                    include_str!("../../../queries/ruby/calls.scm"),
                    include_str!("../../../queries/ruby/risks.scm"),
                    "ruby",
                )
                .map_err(|e| format!("{e:#}"))
            })
            .as_ref()
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    fn symbol_kind(&self, def: Node, _src: &[u8]) -> Option<&'static str> {
        Some(match def.kind() {
            "singleton_method" => "method",
            "method" if enclosing_type(def).is_some() => "method",
            "method" => "fn",
            "class" | "module" => "class",
            "assignment" => "const",
            _ => return None,
        })
    }

    fn scope_chain(&self, def: Node, src: &[u8]) -> Vec<String> {
        let mut chain = Vec::new();
        let mut cur = def.parent();
        while let Some(n) = cur {
            if matches!(n.kind(), "class" | "module")
                && let Some(name) = n.child_by_field_name("name")
            {
                chain.push(text(name, src).to_string());
            }
            cur = n.parent();
        }
        chain.reverse();
        // `class Foo::Bar` is the symbol `Bar` scoped under `Foo`.
        if matches!(def.kind(), "class" | "module")
            && let Some(scope) = def.child_by_field_name("name").and_then(|n| n.child_by_field_name("scope"))
        {
            chain.push(text(scope, src).to_string());
        }
        chain
    }

    fn param_types(&self, def: Node, src: &[u8]) -> Vec<String> {
        // `def self.x` and an instance `x` share scope, name and kind; the marker keeps their IDs
        // apart without the line-number fallback, which would change on unrelated edits.
        let mut out = Vec::new();
        if def.kind() == "singleton_method" {
            out.push("self.".to_string());
        }
        let Some(params) = def.child_by_field_name("parameters") else {
            return out;
        };
        let mut cursor = params.walk();
        out.extend(params.named_children(&mut cursor).filter(|p| p.kind() != "comment").map(|p| match p.kind() {
            "splat_parameter" => "*".to_string(),
            "hash_splat_parameter" => "**".to_string(),
            "block_parameter" => "&".to_string(),
            "forward_parameter" => "...".to_string(),
            "keyword_parameter" => {
                format!("kw:{}", p.child_by_field_name("name").map_or("", |n| text(n, src)))
            }
            _ => "_".to_string(),
        }));
        out
    }

    fn visibility(&self, def: Node, src: &[u8]) -> &'static str {
        if !matches!(def.kind(), "method" | "singleton_method") {
            return "pub";
        }
        let name = def.child_by_field_name("name").map_or("", |n| text(n, src));
        // `private def x` / `private_class_method def self.x`
        let wrapper = def
            .parent()
            .filter(|p| p.kind() == "argument_list")
            .and_then(|a| a.parent())
            .filter(|c| c.kind() == "call" && c.child_by_field_name("receiver").is_none());
        if let Some(call) = wrapper
            && let Some(level) = call.child_by_field_name("method").and_then(|m| keyword_level(text(m, src)))
        {
            return level;
        }
        let anchor = wrapper.unwrap_or(def);
        let Some(scope) = anchor.parent() else {
            return "pub";
        };

        // `private :a, :b` / `private_class_method :x`, anywhere in the same body
        let mut cursor = scope.walk();
        for sib in scope.named_children(&mut cursor) {
            if sib.kind() != "call" || sib.child_by_field_name("receiver").is_some() {
                continue;
            }
            let Some(level) = sib.child_by_field_name("method").and_then(|m| keyword_level(text(m, src))) else {
                continue;
            };
            let Some(args) = sib.child_by_field_name("arguments") else {
                continue;
            };
            let mut ac = args.walk();
            let named = args.named_children(&mut ac).any(|a| {
                matches!(a.kind(), "simple_symbol" | "string")
                    && text(a, src).trim_start_matches(':').trim_matches(|c| c == '"' || c == '\'') == name
            });
            let class_method = text(sib.child_by_field_name("method").unwrap(), src).ends_with("_class_method");
            if named && class_method == (def.kind() == "singleton_method") {
                return level;
            }
        }

        // A bare `private` applies to the instance methods that follow it, not to `def self.x`.
        if def.kind() == "method" {
            let mut prev = anchor.prev_sibling();
            while let Some(p) = prev {
                if p.kind() == "identifier"
                    && let Some(level) = keyword_level(text(p, src))
                {
                    return level;
                }
                prev = p.prev_sibling();
            }
        }
        "pub"
    }

    fn doc_comment(&self, def: Node, src: &[u8]) -> String {
        let anchor = def
            .parent()
            .filter(|p| p.kind() == "argument_list")
            .and_then(|a| a.parent())
            .filter(|c| c.kind() == "call")
            .unwrap_or(def);
        let mut lines = Vec::new();
        let mut row = anchor.start_position().row;
        let mut prev = anchor.prev_sibling();
        while let Some(c) = prev.filter(|c| c.kind() == "comment" && c.end_position().row + 1 >= row && own_line(*c)) {
            let raw = text(c, src);
            let body = raw.trim_start_matches('#').trim();
            if !MAGIC_COMMENTS.iter().any(|m| body.starts_with(m)) {
                let stripped = raw.trim_start_matches('#');
                lines.push(stripped.strip_prefix(' ').unwrap_or(stripped).trim_end().to_string());
            }
            row = c.start_position().row;
            prev = c.prev_sibling();
        }
        lines.reverse();
        lines.join("\n")
    }

    fn is_entrypoint(&self, def: Node, src: &[u8]) -> bool {
        let name = def.child_by_field_name("name").map_or("", |n| text(n, src));
        match def.kind() {
            "method" => match enclosing_type(def) {
                None => name == "main" && def.parent().is_some_and(|p| p.kind() == "program"),
                Some(class) if class.kind() == "class" => {
                    if self.visibility(def, src) != "pub" || name == "initialize" {
                        return false;
                    }
                    let base = superclass(class, src);
                    let last = class_name(class, src).rsplit("::").next().unwrap_or("").to_string();
                    if name == "perform" {
                        return JOB_BASES.contains(&base.as_str()) || includes_sidekiq(class, src);
                    }
                    last.ends_with("Controller") || CONTROLLER_BASES.contains(&base.as_str()) || base == "Thor"
                }
                Some(_) => false,
            },
            _ => false,
        }
    }

    fn is_branch(&self, node: Node, src: &[u8]) -> bool {
        // keyword tokens (`if`, `when`, `rescue`) are anonymous nodes named like the constructs
        if !node.is_named() {
            return false;
        }
        match node.kind() {
            "if" | "elsif" | "unless" | "while" | "until" | "for" | "when" | "rescue" | "conditional"
            | "if_modifier" | "unless_modifier" | "while_modifier" | "until_modifier" | "rescue_modifier"
            | "in_clause" => true,
            "binary" => node
                .child_by_field_name("operator")
                .is_some_and(|op| matches!(text(op, src), "&&" | "||" | "and" | "or")),
            _ => false,
        }
    }

    fn is_nested_def(&self, node: Node) -> bool {
        matches!(node.kind(), "method" | "singleton_method" | "class" | "module")
    }

    fn arity(&self, def: Node, _src: &[u8]) -> Option<Arity> {
        if !matches!(def.kind(), "method" | "singleton_method") {
            return None;
        }
        let mut a = Arity { min: 0, max: Some(0), keywords: false };
        for p in def.child_by_field_name("parameters").map(named_kids).unwrap_or_default() {
            match p.kind() {
                "identifier" | "destructured_parameter" => {
                    a.min += 1;
                    a.max = a.max.map(|m| m + 1);
                }
                "optional_parameter" => a.max = a.max.map(|m| m + 1),
                "splat_parameter" | "forward_parameter" => a.max = None,
                "keyword_parameter" | "hash_splat_parameter" | "hash_splat_nil" => a.keywords = true,
                _ => {}
            }
        }
        Some(a)
    }

    fn call_args(&self, site: Node, _src: &[u8]) -> Option<CallArgs> {
        let mut args = CallArgs { positional: 0, keywords: false };
        for a in site.child_by_field_name("arguments").map(named_kids).unwrap_or_default() {
            match a.kind() {
                "pair" => args.keywords = true,
                "block_argument" => {}
                "splat_argument" | "hash_splat_argument" | "forward_argument" => return None,
                _ => args.positional += 1,
            }
        }
        Some(args)
    }

    fn receiver_type(&self, receiver: Node, src: &[u8]) -> Option<TypeExpr> {
        expr_type(receiver, src, 0)
    }

    fn call_arg_types(&self, site: Node, src: &[u8]) -> Vec<Option<TypeExpr>> {
        let mut out = Vec::new();
        for a in site.child_by_field_name("arguments").map(named_kids).unwrap_or_default() {
            match a.kind() {
                "pair" | "block_argument" => {}
                "splat_argument" | "hash_splat_argument" | "forward_argument" => break,
                _ => out.push(expr_type(a, src, 0)),
            }
        }
        out
    }

    /// `@x = Foo.new(..)` anywhere in a class types `@x` for the whole class. Also records the
    /// methods a class gets without a `def` (accessors, Struct members, aliases, delegators).
    fn type_facts(&self, root: Node, src: &[u8]) -> TypeFacts {
        let mut facts = TypeFacts::default();
        let class_qn = |class: Node| {
            let mut chain = self.scope_chain(class, src);
            chain.push(class.child_by_field_name("name").map_or("", |c| last_segment(text(c, src))).to_string());
            chain.join("::")
        };
        crate::ingestion::parser::walk(root, |n| {
            // `Point = Struct.new(:x, :y)` and `class Point < Struct.new(:x, :y)`
            let struct_members = |call: Node| -> Option<Vec<String>> {
                let r = call.child_by_field_name("receiver")?;
                let m = call.child_by_field_name("method")?;
                (call.kind() == "call" && text(r, src) == "Struct" && text(m, src) == "new")
                    .then(|| symbol_args(call, src))
            };
            if n.kind() == "assignment"
                && let (Some(l), Some(r)) = (n.child_by_field_name("left"), n.child_by_field_name("right"))
                && l.kind() == "constant"
                && let Some(members) = struct_members(r)
            {
                let qn = match enclosing_type(n) {
                    Some(c) => format!("{}::{}", class_qn(c), text(l, src)),
                    None => text(l, src).to_string(),
                };
                accessor_methods(&qn, &members, true, &mut facts.methods);
                facts.classes.push(qn);
            }
            if n.kind() == "class"
                && let Some(sup) = n.child_by_field_name("superclass").and_then(|s| s.named_child(0))
                && let Some(members) = struct_members(sup)
            {
                accessor_methods(&class_qn(n), &members, true, &mut facts.methods);
            }
            // elements put into instance-variable collections
            if let Some(class) = enclosing_type(n)
                && let Some((target, values)) = collection_puts(n, src)
                && target.kind() == "instance_variable"
            {
                let qn = class_qn(class);
                for v in values {
                    facts.elems.push((qn.clone(), text(target, src).to_string(), expr_type(v, src, 0)));
                }
            }
            if matches!(n.kind(), "assignment" | "operator_assignment")
                && let Some(class) = enclosing_type(n)
                && let (Some(l), Some(r)) = (n.child_by_field_name("left"), n.child_by_field_name("right"))
                && l.kind() == "instance_variable"
                && !fresh_collection(r, src)
            {
                facts.elems.push((class_qn(class), text(l, src).to_string(), None));
            }
            // a method's result: its last expression, when that has a known type
            if matches!(n.kind(), "method" | "singleton_method")
                && let Some(class) = enclosing_type(n)
                && let Some(name) = n.child_by_field_name("name")
                && let Some(t) = method_result(n, src)
            {
                facts.results.push((class_qn(class), text(name, src).to_string(), t));
            }
            if let Some(class) = enclosing_type(n) {
                let defined: Vec<String> = match n.kind() {
                    "alias" => n.child_by_field_name("name").map(|a| text(a, src).to_string()).into_iter().collect(),
                    "call" if n.child_by_field_name("receiver").is_none() => {
                        let method = n.child_by_field_name("method").map_or("", |m| text(m, src));
                        let args = symbol_args(n, src);
                        match method {
                            "attr_reader" | "attr" => {
                                reader_results(&class_qn(class), &args, &mut facts.results);
                                args
                            }
                            "attr_writer" => args.iter().map(|a| format!("{a}=")).collect(),
                            "attr_accessor" => {
                                reader_results(&class_qn(class), &args, &mut facts.results);
                                accessor_methods(&class_qn(class), &args, true, &mut facts.methods);
                                Vec::new()
                            }
                            "alias_method" | "define_method" => args.into_iter().take(1).collect(),
                            "def_delegators" | "delegate" => {
                                args.into_iter().skip(usize::from(method == "def_delegators")).collect()
                            }
                            "def_delegator" => args.get(2).or(args.get(1)).cloned().into_iter().collect(),
                            _ => Vec::new(),
                        }
                    }
                    _ => Vec::new(),
                };
                let qn = class_qn(class);
                facts.methods.extend(defined.into_iter().map(|m| (qn.clone(), m)));
            }
            if matches!(n.kind(), "assignment" | "operator_assignment")
                && let (Some(l), Some(r)) = (n.child_by_field_name("left"), n.child_by_field_name("right"))
                && l.kind() == "instance_variable"
                && let Some(class) = enclosing_type(n)
                && r.kind() != "nil"
            {
                // an untyped assignment (`?`) makes the variable's type unknown
                let t = expr_type(r, src, 0).unwrap_or_else(|| TypeExpr::Named("?".to_string()));
                if t != TypeExpr::Field(Box::new(TypeExpr::Named("self".to_string())), text(l, src).to_string()) {
                    facts.fields.push((class_qn(class), text(l, src).to_string(), t));
                }
            }
            true
        });
        facts
    }

    fn implements(&self, root: Node, src: &[u8]) -> Vec<ImplRel> {
        let mut out = Vec::new();
        crate::ingestion::parser::walk(root, |n| {
            if !matches!(n.kind(), "class" | "module") {
                return true;
            }
            let Some(name) = n.child_by_field_name("name") else {
                return true;
            };
            let type_name = last_segment(text(name, src)).to_string();
            let line = n.start_position().row + 1;
            if let Some(sup) = n.child_by_field_name("superclass").and_then(|s| s.named_child(0))
                && matches!(sup.kind(), "constant" | "scope_resolution")
            {
                out.push(ImplRel { type_name: type_name.clone(), trait_name: text(sup, src).to_string(), line });
            }
            if let Some(body) = n.child_by_field_name("body") {
                let mut cursor = body.walk();
                for call in body.named_children(&mut cursor).filter(|c| c.kind() == "call") {
                    let method = call.child_by_field_name("method").map_or("", |m| text(m, src));
                    if call.child_by_field_name("receiver").is_some()
                        || !matches!(method, "include" | "extend" | "prepend")
                    {
                        continue;
                    }
                    let Some(args) = call.child_by_field_name("arguments") else {
                        continue;
                    };
                    let mut ac = args.walk();
                    for a in
                        args.named_children(&mut ac).filter(|a| matches!(a.kind(), "constant" | "scope_resolution"))
                    {
                        out.push(ImplRel {
                            type_name: type_name.clone(),
                            trait_name: text(a, src).to_string(),
                            line: call.start_position().row + 1,
                        });
                    }
                }
            }
            true
        });
        out
    }

    fn flatten_import(&self, node: Node, src: &[u8], out: &mut Vec<ImportDecl>) {
        if node.kind() != "call" {
            return;
        }
        let Some(args) = node.child_by_field_name("arguments") else {
            return;
        };
        let mut cursor = args.walk();
        let Some(path) = args.named_children(&mut cursor).find(|a| a.kind() == "string").and_then(|s| literal(s, src))
        else {
            return;
        };
        let path = path.strip_suffix(".rb").unwrap_or(&path);
        let (module_path, name) = match path.rsplit_once('/') {
            Some((m, n)) => (m.to_string(), n.to_string()),
            None => (String::new(), path.to_string()),
        };
        // `require_relative` is relative to the requiring file; the linker resolves `./` paths
        // against its directory.
        let relative = node.child_by_field_name("method").is_some_and(|m| text(m, src) == "require_relative");
        let module_path = if relative { format!("./{module_path}") } else { module_path };
        out.push(ImportDecl { module_path, name, alias: None });
    }
}

/// The receiver's class as far as the enclosing method shows it: `Foo.new`, a local assigned
/// from one, `self`, `self.class`, or an instance variable (typed by the class's assignments).
fn expr_type(n: Node, src: &[u8], depth: u8) -> Option<TypeExpr> {
    if depth > 32 {
        return None;
    }
    let core = |c: &str| Some(TypeExpr::Named(c.to_string()));
    match n.kind() {
        "self" => Some(TypeExpr::Named("self".to_string())),
        // literals have core types, which have no repo methods
        "array" => core("Array"),
        "hash" => core("Hash"),
        "string" | "heredoc_beginning" | "chained_string" => core("String"),
        "simple_symbol" | "delimited_symbol" => core("Symbol"),
        "integer" => core("Integer"),
        "float" => core("Float"),
        "regex" => core("Regexp"),
        "range" => core("Range"),
        "instance_variable" => {
            Some(TypeExpr::Field(Box::new(TypeExpr::Named("self".to_string())), text(n, src).to_string()))
        }
        "parenthesized_statements" => expr_type(n.named_child(0)?, src, depth + 1),
        "constant" | "scope_resolution" => Some(TypeExpr::Ident(text(n, src).to_string())),
        "if" | "unless" | "case" | "conditional" | "begin" => common_type(&[n], src),
        "binary"
            if n.child_by_field_name("operator")
                .is_some_and(|o| matches!(text(o, src), "||" | "or" | "&&" | "and")) =>
        {
            common_type(&[n], src)
        }
        "operator_assignment" | "assignment" => expr_type(n.child_by_field_name("right")?, src, depth + 1),
        "call" => {
            let method = text(n.child_by_field_name("method")?, src);
            let Some(receiver) = n.child_by_field_name("receiver") else {
                // `helper(x)`: a self call
                return Some(self_call(method));
            };
            match (receiver.kind(), method) {
                ("constant" | "scope_resolution", "new") => Some(TypeExpr::Named(text(receiver, src).to_string())),
                ("self", "class") => Some(TypeExpr::Named("self.class".to_string())),
                _ => {
                    let base = expr_type(receiver, src, depth + 1)?;
                    Some(TypeExpr::Call(Box::new(TypeExpr::Field(Box::new(base), method.to_string()))))
                }
            }
        }
        "identifier" => match local_binding(n, src, depth) {
            Some(t) => t,
            // neither a local nor a parameter: a paren-less self call
            None => Some(self_call(text(n, src))),
        },
        _ => None,
    }
}

/// A method's result type, when its last expression and every `return` agree (`nil` results
/// don't count against it: they answer no methods anyway).
fn method_result(method: Node, src: &[u8]) -> Option<TypeExpr> {
    let body = method.child_by_field_name("body")?;
    let last = named_kids(body).into_iter().rfind(|c| !matches!(c.kind(), "rescue" | "else" | "ensure"))?;
    let mut values = vec![last];
    crate::ingestion::parser::walk(body, |n| {
        if matches!(n.kind(), "method" | "singleton_method" | "class" | "module" | "lambda") {
            return false;
        }
        if n.kind() == "return"
            && let Some(v) = n.named_child(0).and_then(|a| named_kids(a).into_iter().next())
        {
            values.push(v);
        }
        true
    });
    common_type(&values, src)
}

/// The types `values` can have (looking through branches), ignoring `nil`s: one type, or a
/// `Union` of them. `None` when any is untyped.
fn common_type(values: &[Node], src: &[u8]) -> Option<TypeExpr> {
    let mut found: Vec<TypeExpr> = Vec::new();
    for v in values {
        for leaf in branch_results(*v, src) {
            if leaf.kind() == "nil" {
                continue;
            }
            let t = expr_type(leaf, src, 0)?;
            if !found.contains(&t) {
                found.push(t);
            }
        }
    }
    match found.len() {
        0 => None,
        1 => found.pop(),
        _ => Some(TypeExpr::Union(found)),
    }
}

/// The expressions a value can come from: both branches of `if`/`unless`/ternaries and `case`,
/// both sides of `a || b`, the last statement of a `begin` or parenthesized block.
fn branch_results<'t>(n: Node<'t>, src: &[u8]) -> Vec<Node<'t>> {
    let last = |b: Node<'t>| {
        named_kids(b).into_iter().rfind(|c| !matches!(c.kind(), "rescue" | "else" | "ensure" | "comment"))
    };
    match n.kind() {
        "if" | "unless" | "elsif" => {
            let mut out = Vec::new();
            for f in ["consequence", "alternative"] {
                match n.child_by_field_name(f) {
                    Some(b) if matches!(b.kind(), "elsif" | "if") => out.extend(branch_results(b, src)),
                    Some(b) => out.extend(last(b).map(|l| branch_results(l, src)).unwrap_or_default()),
                    // a missing branch is `nil`
                    None => {}
                }
            }
            out
        }
        "conditional" => ["consequence", "alternative"]
            .into_iter()
            .filter_map(|f| n.child_by_field_name(f))
            .flat_map(|b| branch_results(b, src))
            .collect(),
        "case" => named_kids(n)
            .into_iter()
            .filter(|c| matches!(c.kind(), "when" | "else"))
            .filter_map(|c| if c.kind() == "else" { last(c) } else { c.child_by_field_name("body").and_then(last) })
            .flat_map(|l| branch_results(l, src))
            .collect(),
        "binary" if n.child_by_field_name("operator").is_some_and(|o| matches!(text(o, src), "||" | "or")) => {
            ["left", "right"]
                .into_iter()
                .filter_map(|f| n.child_by_field_name(f))
                .flat_map(|b| branch_results(b, src))
                .collect()
        }
        // `a && b` is `b` (or a falsy `a`, which answers nothing)
        "binary" if n.child_by_field_name("operator").is_some_and(|o| matches!(text(o, src), "&&" | "and")) => {
            n.child_by_field_name("right").map(|r| branch_results(r, src)).unwrap_or_default()
        }
        "begin" | "parenthesized_statements" => last(n).map(|l| branch_results(l, src)).unwrap_or_default(),
        _ => vec![n],
    }
}

/// Iterators whose block's first parameter is an element of the receiver.
const ITERATORS: &[&str] = &[
    "each",
    "map",
    "collect",
    "flat_map",
    "select",
    "filter",
    "filter_map",
    "reject",
    "find",
    "detect",
    "find_all",
    "any?",
    "all?",
    "none?",
    "one?",
    "count",
    "sort_by",
    "min_by",
    "max_by",
    "group_by",
    "partition",
    "sum",
    "take_while",
    "drop_while",
    "each_with_index",
    "each_with_object",
    "map!",
    "select!",
    "reject!",
    "sort_by!",
    "uniq",
    "each_value",
    "find_index",
    "index",
    "delete_if",
    "keep_if",
    "tally_by",
    "chunk_while",
];

/// `xs.each { |x| }` (and `sort { |a, b| }`, `inject { |acc, x| }`): a block parameter that receives
/// the receiver's elements is typed as such; any other block parameter is untyped.
fn block_param_type(block: Node, k: usize, n: usize, src: &[u8], depth: u8) -> Option<TypeExpr> {
    let call = block
        .parent()
        .filter(|c| c.kind() == "call" && c.child_by_field_name("block").is_some_and(|b| b.id() == block.id()))?;
    let method = text(call.child_by_field_name("method")?, src);
    let elem_param = match method {
        "sort" | "sort!" | "max" | "min" | "minmax" => k < 2,
        "inject" | "reduce" => k == 1 && n == 2,
        m if ITERATORS.contains(&m) => k == 0 && (n == 1 || matches!(m, "each_with_index" | "each_with_object")),
        _ => false,
    };
    if !elem_param {
        return None;
    }
    let receiver = call.child_by_field_name("receiver")?;
    // a local collection filled in this method: its elements' type, when they agree
    if receiver.kind() == "identifier"
        && let Some(t) = local_elem_type(receiver, src, depth)
    {
        return Some(t);
    }
    Some(TypeExpr::Elem(Box::new(expr_type(receiver, src, depth + 1)?)))
}

/// The common type of what a method puts into local collection `ident` (`xs << v`, `xs.push(v)`,
/// `xs = [v]`), when every such value has the same known type.
fn local_elem_type(ident: Node, src: &[u8], depth: u8) -> Option<TypeExpr> {
    let name = text(ident, src);
    let mut method = ident.parent();
    while let Some(m) = method.filter(|m| !matches!(m.kind(), "method" | "singleton_method")) {
        method = m.parent();
    }
    let mut found: Option<Option<TypeExpr>> = None;
    let mut add = |t: Option<TypeExpr>| match &found {
        None => found = Some(t),
        Some(prev) if *prev != t => found = Some(None),
        _ => {}
    };
    crate::ingestion::parser::walk(method?, |n| {
        if let Some((target, values)) = collection_puts(n, src)
            && target.kind() == "identifier"
            && text(target, src) == name
        {
            for v in values {
                add(expr_type(v, src, depth + 1));
            }
        }
        // assigned something other than a fresh collection: elements we don't see
        if matches!(n.kind(), "assignment" | "operator_assignment")
            && let (Some(l), Some(r)) = (n.child_by_field_name("left"), n.child_by_field_name("right"))
            && l.kind() == "identifier"
            && text(l, src) == name
            && !fresh_collection(r, src)
        {
            add(None);
        }
        true
    });
    found.flatten()
}

/// Whether `n` makes a fresh collection whose elements are only what is later put into it:
/// `[]`/`{}` literals (their own elements count as puts) and `Array.new`/`Hash.new`/`Set.new`.
fn fresh_collection(n: Node, src: &[u8]) -> bool {
    match n.kind() {
        "array" | "hash" => true,
        "call" => {
            n.child_by_field_name("receiver").is_some_and(|r| matches!(text(r, src), "Array" | "Hash" | "Set"))
                && n.child_by_field_name("method").is_some_and(|m| text(m, src) == "new")
                && n.child_by_field_name("arguments").is_none()
        }
        _ => false,
    }
}

/// The collection node `n` puts values into (`xs << v`, `xs.push(v)`, `xs[k] = v`, `xs = [v]`),
/// and those values.
fn collection_puts<'t>(n: Node<'t>, src: &[u8]) -> Option<(Node<'t>, Vec<Node<'t>>)> {
    match n.kind() {
        "binary" if n.child_by_field_name("operator").is_some_and(|o| text(o, src) == "<<") => {
            Some((n.child_by_field_name("left")?, vec![n.child_by_field_name("right")?]))
        }
        "call" => {
            let m = n.child_by_field_name("method").map_or("", |m| text(m, src));
            matches!(m, "push" | "unshift" | "append" | "prepend" | "add" | "<<").then_some(())?;
            Some((
                n.child_by_field_name("receiver")?,
                n.child_by_field_name("arguments").map(named_kids).unwrap_or_default(),
            ))
        }
        "assignment" => {
            let (l, r) = (n.child_by_field_name("left")?, n.child_by_field_name("right")?);
            match l.kind() {
                "element_reference" => Some((l.child_by_field_name("object")?, vec![r])),
                _ if r.kind() == "array" => Some((l, named_kids(r))),
                _ => None,
            }
        }
        _ => None,
    }
}

fn self_call(method: &str) -> TypeExpr {
    TypeExpr::Call(Box::new(TypeExpr::Field(Box::new(TypeExpr::Named("self".to_string())), method.to_string())))
}

/// `Some(type)` when `ident` is a local or parameter of its method (`Some(None)` when its type is
/// unknown: parameters, block variables, assignments from untyped expressions); `None` when it is
/// neither, i.e. a paren-less method call.
fn local_binding(ident: Node, src: &[u8], depth: u8) -> Option<Option<TypeExpr>> {
    let name = text(ident, src);
    let mut method = None;
    let mut cur = ident.parent();
    while let Some(n) = cur {
        match n.kind() {
            "block" | "do_block" | "lambda" => {
                let params = n.child_by_field_name("parameters").map(|p| param_names(p, src)).unwrap_or_default();
                if let Some(k) = params.iter().position(|p| p == name) {
                    return Some(block_param_type(n, k, params.len(), src, depth));
                }
            }
            "method" | "singleton_method" => {
                method = Some(n);
                break;
            }
            "class" | "module" | "program" | "singleton_class" => break,
            _ => {}
        }
        cur = n.parent();
    }
    let method = method?; // class bodies and scripts: leave unknown rather than guess
    if let Some(params) = method.child_by_field_name("parameters") {
        let kids = named_kids(params);
        if let Some(i) = kids.iter().position(|p| param_names_of(*p, src) == name) {
            // positional parameters are typed by the linker from their callers' arguments
            let positional =
                kids[..=i].iter().filter(|p| matches!(p.kind(), "identifier" | "optional_parameter")).count();
            let method_name = method.child_by_field_name("name").map_or("", |n| text(n, src));
            return Some(
                matches!(kids[i].kind(), "identifier" | "optional_parameter")
                    .then(|| TypeExpr::Param(method_name.to_string(), positional - 1)),
            );
        }
    }
    let mut last_before = None;
    let mut assigned_anywhere = false;
    crate::ingestion::parser::walk(method, |n| {
        if matches!(n.kind(), "assignment" | "operator_assignment")
            && let Some(l) = n.child_by_field_name("left")
        {
            let targets = if l.kind() == "left_assignment_list" { named_kids(l) } else { vec![l] };
            if targets.iter().any(|t| t.kind() == "identifier" && text(*t, src) == name) {
                assigned_anywhere = true;
                if n.end_byte() <= ident.start_byte() {
                    let single = l.kind() == "identifier";
                    last_before = Some(single.then(|| n.child_by_field_name("right")).flatten());
                }
            }
        }
        // `for x in xs`, `rescue => e`, block params of nested blocks named alike
        if matches!(n.kind(), "for" | "exception_variable")
            && named_kids(n).first().is_some_and(|v| text(*v, src) == name)
        {
            assigned_anywhere = true;
        }
        true
    });
    match last_before {
        Some(right) => Some(right.and_then(|r| expr_type(r, src, depth + 1))),
        None if assigned_anywhere => Some(None),
        None => None,
    }
}

fn param_names(params: Node, src: &[u8]) -> Vec<String> {
    named_kids(params).into_iter().map(|p| param_names_of(p, src)).collect()
}

fn param_names_of(p: Node, src: &[u8]) -> String {
    match p.kind() {
        "identifier" => text(p, src).to_string(),
        _ => p.child_by_field_name("name").map_or(String::new(), |n| text(n, src).to_string()),
    }
}

/// Literal symbol or string arguments of a call: `attr_reader :a, "b"` -> [a, b].
fn symbol_args(call: Node, src: &[u8]) -> Vec<String> {
    let Some(args) = call.child_by_field_name("arguments") else {
        return Vec::new();
    };
    let mut cursor = args.walk();
    args.named_children(&mut cursor)
        .filter(|a| matches!(a.kind(), "simple_symbol" | "string"))
        .filter_map(|a| match a.kind() {
            "string" => literal(a, src),
            _ => Some(text(a, src).trim_start_matches(':').to_string()),
        })
        .filter(|a| !a.starts_with('@'))
        .collect()
}

fn named_kids(n: Node) -> Vec<Node> {
    let mut c = n.walk();
    n.named_children(&mut c).filter(|x| x.kind() != "comment").collect()
}

/// `attr_reader :x` returns `@x`.
fn reader_results(class: &str, names: &[String], out: &mut Vec<(String, String, TypeExpr)>) {
    for n in names {
        let ivar = TypeExpr::Field(Box::new(TypeExpr::Named("self".to_string())), format!("@{n}"));
        out.push((class.to_string(), n.clone(), ivar));
    }
}

/// A reader and, if `writer`, an `x=` writer per name.
fn accessor_methods(class: &str, names: &[String], writer: bool, out: &mut Vec<(String, String)>) {
    for n in names {
        out.push((class.to_string(), n.clone()));
        if writer {
            out.push((class.to_string(), format!("{n}=")));
        }
    }
}

fn keyword_level(name: &str) -> Option<&'static str> {
    match name {
        "private" | "protected" | "private_class_method" => Some("private"),
        "public" | "public_class_method" => Some("pub"),
        _ => None,
    }
}

fn last_segment(path: &str) -> &str {
    path.rsplit("::").next().unwrap_or(path)
}

/// The nearest enclosing `class`/`module`, looking through wrapper calls and `class << self`.
fn enclosing_type(def: Node) -> Option<Node> {
    let mut cur = def.parent();
    while let Some(n) = cur {
        if matches!(n.kind(), "class" | "module") {
            return Some(n);
        }
        cur = n.parent();
    }
    None
}

fn class_name<'a>(class: Node, src: &'a [u8]) -> &'a str {
    class.child_by_field_name("name").map_or("", |n| text(n, src))
}

fn superclass(class: Node, src: &[u8]) -> String {
    class
        .child_by_field_name("superclass")
        .and_then(|s| s.named_child(0))
        .map_or(String::new(), |s| text(s, src).to_string())
}

fn includes_sidekiq(class: Node, src: &[u8]) -> bool {
    let Some(body) = class.child_by_field_name("body") else {
        return false;
    };
    let mut cursor = body.walk();
    body.named_children(&mut cursor).any(|c| {
        c.kind() == "call"
            && c.child_by_field_name("method").is_some_and(|m| text(m, src) == "include")
            && text(c, src).contains("Sidekiq::")
    })
}

/// Contents of a string literal without interpolation.
fn literal(s: Node, src: &[u8]) -> Option<String> {
    let mut cursor = s.walk();
    let mut out = String::new();
    for c in s.named_children(&mut cursor) {
        if c.kind() != "string_content" {
            return None;
        }
        out.push_str(text(c, src));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use crate::ingestion::model::*;
    use crate::ingestion::parser::parse_file;
    use crate::ingestion::walker::{Language, SourceFile};

    fn parse_at(path: &str, src: &str) -> ParsedFile {
        let f = SourceFile {
            rel_path: path.into(),
            language: Language::Ruby,
            blake3: String::new(),
            line_count: 0,
            source: src.into(),
        };
        parse_file(&f).expect("ruby queries must compile").unwrap()
    }

    fn parse(src: &str) -> ParsedFile {
        parse_at("app/models/account.rb", src)
    }

    fn sym<'a>(p: &'a ParsedFile, name: &str) -> &'a ParsedSymbol {
        p.symbols
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("no symbol {name}: {:?}", p.symbols.iter().map(|s| &s.name).collect::<Vec<_>>()))
    }

    #[test]
    fn symbols_kinds_ids_docs_and_scopes() {
        let p = parse(
            r#"# frozen_string_literal: true

MAX = 3
counter = 0

# Billing core.
# Second line.
module Billing
  RATE = 2

  class Account::Ledger < Base
    LIMIT = 10
    @state = 1

    # Posts an entry.
    def post(entry, amount = 1, *rest, key:, opt: 2, **kw, &blk)
      @x = 1
    end

    def self.build; end
    def build; end
  end
end

def helper(x); end
"#,
        );
        let billing = sym(&p, "Billing");
        assert_eq!(billing.kind, "class");
        assert_eq!(
            billing.doc_comment,
            "Billing core.
Second line."
        );
        assert_eq!(sym(&p, "MAX").kind, "const");
        assert_eq!(sym(&p, "RATE").qualified_name, "Billing::RATE");
        assert!(!p.symbols.iter().any(|s| s.name == "counter" || s.name == "@state"));

        let ledger = sym(&p, "Ledger");
        assert_eq!((ledger.kind, ledger.qualified_name.as_str()), ("class", "Billing::Account::Ledger"));
        assert_eq!(sym(&p, "LIMIT").qualified_name, "Billing::Account::Ledger::LIMIT");

        let post = sym(&p, "post");
        assert_eq!((post.kind, post.visibility), ("method", "pub"));
        assert!(post.symbol_id.contains("::Billing::Account::Ledger::post#method:"), "{}", post.symbol_id);
        assert_eq!(post.doc_comment, "Posts an entry.");
        assert_eq!(post.signature, "def post(entry, amount = 1, *rest, key:, opt: 2, **kw, &blk)");
        assert_eq!(sym(&p, "helper").kind, "fn");

        let builds: Vec<_> = p.symbols.iter().filter(|s| s.name == "build").collect();
        assert_eq!(builds.len(), 2);
        assert_ne!(builds[0].symbol_id, builds[1].symbol_id, "def self.build vs build must not collide");
        assert!(builds.iter().all(|b| !b.symbol_id.contains("@L")), "IDs must not depend on line numbers");
    }

    #[test]
    fn param_shapes_feed_the_signature_hash() {
        let a = parse(
            "def f(a, b = 1, *r, k:, **o, &blk); end
",
        );
        let b = parse(
            "def f(a, b = 2, *r, k:, **o, &blk); end
",
        );
        let c = parse(
            "def f(a, *r, j:, **o, &blk); end
",
        );
        assert_eq!(sym(&a, "f").symbol_id, sym(&b, "f").symbol_id);
        assert_ne!(sym(&a, "f").symbol_id, sym(&c, "f").symbol_id);
    }

    #[test]
    fn visibility_keywords_scope_correctly() {
        let p = parse(
            r#"class Svc
  def open_one; end

  def self.klass_pub; end

  private

  def hidden_a; end
  def self.still_public; end

  protected

  def guarded; end

  public

  def open_two; end

  private def wrapped; end
  def after_wrapper; end

  def explicit_one; end
  def explicit_two; end
  private :explicit_one, :explicit_two

  def self.secret; end
  private_class_method :secret
  private_class_method def self.secret_wrapped; end
end
"#,
        );
        let vis = |n: &str| sym(&p, n).visibility;
        for n in ["open_one", "klass_pub", "still_public", "open_two", "after_wrapper"] {
            assert_eq!(vis(n), "pub", "{n}");
        }
        for n in ["hidden_a", "guarded", "wrapped", "explicit_one", "explicit_two", "secret", "secret_wrapped"] {
            assert_eq!(vis(n), "private", "{n}");
        }
    }

    #[test]
    fn complexity_counts_ruby_branches_and_skips_nested_defs() {
        let p = parse(
            r#"def f(x, y)
  def inner(z)
    z if z
  end
  if x && y
    1
  elsif x or y
    2
  end
  puts "a" unless x
  for i in x do end
  while y do break end
  case x
  when 1 then 1
  when 2 then 2
  end
  r = x ? 1 : 2
  begin
    1
  rescue ArgumentError
    2
  end
end
"#,
        );
        // if, &&, elsif, or, unless-modifier, for, while, 2 whens, ternary, rescue
        assert_eq!(sym(&p, "f").cyclomatic_complexity, 12);
        assert_eq!(sym(&p, "inner").cyclomatic_complexity, 2);
    }

    #[test]
    fn risk_flags_have_positive_and_negative_cases() {
        let p = parse(
            r#"def a(c); system(c); end
def b(c); `ls #{c}`; end
def c(x); Open3.capture2(x); end
def d(m); send(m); end
def d2; send(:known); end
def e(s); Marshal.load(s); end
def f(s); YAML.load(s); end
def f2(s); YAML.safe_load(s); end
def g(n); User.where("name = '#{n}'"); end
def g2(n); User.where("name = ?", n); end
def g3(n); User.where(name: n); end
def h(s); raw(s); end
def h2(s); s.html_safe; end
def i(p); File.read(p); end
def i2(p); File.join(p, "x"); end
def j; Thread.new { 1 }; end
def k; begin; x; rescue; end; end
def k2; x rescue nil; end
def k3; begin; x; rescue Exception => e; log(e); end; end
def k4; begin; x; rescue ArgumentError => e; log(e); end; end
def l(v); v.instance_eval("1"); end
def m(c); c.constantize; end
def plain(a, b); a + b; end
"#,
        );
        let flags = |n: &str| sym(&p, n).risk_flags.clone();
        for n in ["a", "b", "c", "d", "l", "m"] {
            assert!(flags(n).contains(&"command_exec".to_string()), "{n}: {:?}", flags(n));
        }
        assert!(flags("d2").is_empty(), "{:?}", flags("d2"));
        assert_eq!(flags("e"), ["unsafe_deserialization"]);
        assert_eq!(flags("f"), ["unsafe_deserialization"]);
        assert!(flags("f2").is_empty(), "{:?}", flags("f2"));
        assert_eq!(flags("g"), ["raw_sql"]);
        assert!(flags("g2").is_empty() && flags("g3").is_empty());
        assert_eq!(flags("h"), ["xss_sink"]);
        assert_eq!(flags("h2"), ["xss_sink"]);
        assert_eq!(flags("i"), ["external_io"]);
        assert!(flags("i2").is_empty(), "{:?}", flags("i2"));
        assert_eq!(flags("j"), ["concurrency_primitive"]);
        for n in ["k", "k2", "k3"] {
            assert_eq!(flags(n), ["error_swallow"], "{n}");
        }
        assert!(flags("k4").is_empty(), "{:?}", flags("k4"));
        assert!(flags("plain").is_empty());
        assert!(sym(&p, "a").requires_strict_contract);
    }

    #[test]
    fn entrypoints() {
        let p = parse_at(
            "app/controllers/accounts_controller.rb",
            r#"class AccountsController < ApplicationController
  def index; end
  def show; end

  private

  def set_account; end
end

class Report
  def run; end
end

class SyncJob < ApplicationJob
  def perform(id); end
  def helper; end
end

class Legacy
  include Sidekiq::Worker
  def perform; end
end

class Cli < Thor
  def deploy; end
end

def main; end
"#,
        );
        for n in ["index", "show", "perform", "deploy", "main"] {
            assert!(p.symbols.iter().filter(|s| s.name == n).all(|s| s.is_entrypoint), "{n}");
        }
        assert!(p.symbols.iter().filter(|s| s.name == "perform").count() == 2);
        for n in ["set_account", "run", "helper"] {
            assert!(!sym(&p, n).is_entrypoint, "{n}");
        }
    }

    #[test]
    fn calls_symbol_callbacks_and_imports() {
        let p = parse(
            r#"require 'json'
require 'net/http'
require_relative '../lib/helper'
load 'tasks/seed.rb'
autoload :Foo, 'foo/bar'
require "dyn/#{name}"

class Ctl < Base
  before_action :authorize
  def go(items)
    helper_call(1)
    items.map(&:name)
    Other.build(items)
    send(:authorize)
    bare_local
    items.each { |i| i.save }
  end
  def authorize; end
end
"#,
        );
        let go = sym(&p, "go").symbol_id.clone();
        let find = |name: &str| p.calls.iter().find(|c| c.from_symbol_id == go && c.target_name == name);
        assert_eq!(find("helper_call").unwrap().kind, CallKind::Direct);
        assert_eq!(find("helper_call").unwrap().receiver, None);
        assert_eq!(find("build").unwrap().receiver.as_deref(), Some("Other"));
        assert_eq!(find("save").unwrap().receiver.as_deref(), Some("i"));
        assert!(find("bare_local").is_none(), "paren-less bare identifiers are not call sites");
        let sym_refs: Vec<_> = p.calls.iter().filter(|c| c.kind == CallKind::SymbolRef).collect();
        assert!(sym_refs.iter().any(|c| c.target_name == "authorize" && c.via.as_deref() == Some("send")));
        assert!(sym_refs.iter().any(|c| c.target_name == "authorize" && c.via.as_deref() == Some("before_action")));
        assert!(sym_refs.iter().any(|c| c.target_name == "name" && c.via.as_deref() == Some("map")));
        let recv = |name: &str, via: &str| {
            sym_refs.iter().find(|c| c.target_name == name && c.via.as_deref() == Some(via)).unwrap().receiver.clone()
        };
        assert_eq!(recv("authorize", "send"), None, "receiver-less send is a self call");
        assert_eq!(recv("name", "map").as_deref(), Some("items"), "&:name is called on the elements");

        let imp = |m: &str, n: &str| p.imports.iter().any(|i| i.module_path == m && i.name == n);
        assert!(imp("", "json") && imp("net", "http") && imp("./../lib", "helper"));
        assert!(imp("tasks", "seed") && imp("foo", "bar"));
        assert_eq!(p.imports.len(), 5, "interpolated require is skipped: {:?}", p.imports);
    }

    #[test]
    fn inheritance_and_mixins_become_impl_relations() {
        let p = parse(
            r#"module Mixin; end
class Base; end
class Foo::Bar < Base
  include Mixin
  extend Helpers::Other
  prepend Wrap
end
class Plain; end
"#,
        );
        let rel: Vec<_> = p.impls.iter().map(|r| (r.type_name.as_str(), r.trait_name.as_str())).collect();
        assert_eq!(rel, [("Bar", "Base"), ("Bar", "Mixin"), ("Bar", "Helpers::Other"), ("Bar", "Wrap")]);
    }

    #[test]
    fn ast_hash_ignores_comments_and_formatting() {
        let a = parse(
            "def f(x)
  x + 1
end
",
        );
        let b = parse(
            "# note
def f( x )
  # inner
  x +  1
end
",
        );
        let c = parse(
            "def f(x)
  x + 2
end
",
        );
        assert_eq!(sym(&a, "f").ast_blake3, sym(&b, "f").ast_blake3);
        assert_ne!(sym(&a, "f").ast_blake3, sym(&c, "f").ast_blake3);
    }

    #[test]
    fn trailing_comments_are_not_docs() {
        let p = parse("A = 1 # first\nB = 2\nx = 1 # note\ndef f; end\n# Real doc.\ndef g; end\n");
        assert_eq!(sym(&p, "B").doc_comment, "");
        assert_eq!(sym(&p, "f").doc_comment, "");
        assert_eq!(sym(&p, "g").doc_comment, "Real doc.");
    }
}
