use std::sync::OnceLock;

use anyhow::Result;
use tree_sitter::Node;

use super::{Compiled, Frontend, text};
use crate::ingestion::model::{ImplRel, ImportDecl};
use crate::ingestion::walker::SourceFile;

pub struct PythonFrontend {
    compiled: OnceLock<Result<Compiled, String>>,
}

pub static PYTHON: PythonFrontend = PythonFrontend { compiled: OnceLock::new() };

/// Decorator names (last dotted segment) that expose a function to the outside world.
const ENTRYPOINT_DECORATORS: &[&str] = &[
    "route",
    "get",
    "post",
    "put",
    "delete",
    "patch",
    "head",
    "options",
    "websocket",
    "api_view",
    "command",
    "task",
    "shared_task",
    "errorhandler",
    "before_request",
    "on_event",
    "listener",
    "tool",
    "callback",
];

impl Frontend for PythonFrontend {
    fn compiled(&self, _file: &SourceFile) -> Result<&Compiled> {
        self.compiled
            .get_or_init(|| {
                Compiled::build(
                    tree_sitter_python::LANGUAGE.into(),
                    include_str!("../../../queries/python/symbols.scm"),
                    include_str!("../../../queries/python/calls.scm"),
                    include_str!("../../../queries/python/risks.scm"),
                    "python",
                )
                .map_err(|e| format!("{e:#}"))
            })
            .as_ref()
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    fn symbol_kind(&self, def: Node, src: &[u8]) -> Option<&'static str> {
        Some(match def.kind() {
            "function_definition" if enclosing_class(def).is_some() => "method",
            "function_definition" => "fn",
            "class_definition" => "class",
            "expression_statement" => {
                let name = def.named_child(0).and_then(|a| a.child_by_field_name("left")).map(|n| text(n, src))?;
                if name.chars().all(|c| c.is_uppercase() || c.is_ascii_digit() || c == '_') {
                    "const"
                } else {
                    "global"
                }
            }
            _ => return None,
        })
    }

    fn scope_chain(&self, def: Node, src: &[u8]) -> Vec<String> {
        let mut chain = Vec::new();
        let mut cur = def.parent();
        while let Some(n) = cur {
            if matches!(n.kind(), "class_definition" | "function_definition")
                && let Some(name) = n.child_by_field_name("name")
            {
                chain.push(text(name, src).to_string());
            }
            cur = n.parent();
        }
        chain.reverse();
        chain
    }

    fn param_types(&self, def: Node, src: &[u8]) -> Vec<String> {
        let Some(params) = def.child_by_field_name("parameters") else {
            return Vec::new();
        };
        let mut cursor = params.walk();
        params
            .named_children(&mut cursor)
            .filter(|p| p.kind() != "comment")
            .enumerate()
            .map(|(i, p)| match p.kind() {
                "identifier" if i == 0 && matches!(text(p, src), "self" | "cls") => "self".to_string(),
                "typed_parameter" | "typed_default_parameter" => {
                    p.child_by_field_name("type").map_or("_".to_string(), |t| text(t, src).to_string())
                }
                "list_splat_pattern" => "*".to_string(),
                "dictionary_splat_pattern" => "**".to_string(),
                _ => "_".to_string(),
            })
            .collect()
    }

    fn visibility(&self, def: Node, src: &[u8]) -> &'static str {
        let name = match def.kind() {
            "expression_statement" => def.named_child(0).and_then(|a| a.child_by_field_name("left")),
            _ => def.child_by_field_name("name"),
        }
        .map_or("", |n| text(n, src));
        let dunder = name.starts_with("__") && name.ends_with("__");
        if name.starts_with('_') && !dunder { "private" } else { "pub" }
    }

    fn doc_comment(&self, def: Node, src: &[u8]) -> String {
        let Some(body) = def.child_by_field_name("body") else {
            return String::new();
        };
        let Some(first) = body.named_child(0).filter(|n| n.kind() == "expression_statement") else {
            return String::new();
        };
        let Some(s) = first.named_child(0).filter(|n| n.kind() == "string") else {
            return String::new();
        };
        let raw = text(s, src);
        let inner = raw.trim_start_matches(|c: char| c.is_ascii_alphabetic()).trim_matches(|c| c == '"' || c == '\'');
        inner.lines().map(str::trim).collect::<Vec<_>>().join("\n").trim().to_string()
    }

    fn is_entrypoint(&self, def: Node, src: &[u8]) -> bool {
        if def.kind() != "function_definition" {
            return false;
        }
        let name = def.child_by_field_name("name").map_or("", |n| text(n, src));
        let top_level = def.parent().is_some_and(|p| p.kind() == "module");
        if top_level && matches!(name, "main" | "lambda_handler" | "handler") {
            return true;
        }
        decorator_names(def, src).iter().any(|d| ENTRYPOINT_DECORATORS.contains(&d.as_str()))
    }

    fn is_branch(&self, node: Node, src: &[u8]) -> bool {
        match node.kind() {
            "if_statement"
            | "elif_clause"
            | "for_statement"
            | "while_statement"
            | "except_clause"
            | "except_group_clause"
            | "case_clause"
            | "conditional_expression" => true,
            "boolean_operator" => {
                node.child_by_field_name("operator").is_some_and(|op| matches!(text(op, src), "and" | "or"))
            }
            _ => false,
        }
    }

    fn is_nested_def(&self, node: Node) -> bool {
        matches!(node.kind(), "function_definition" | "class_definition")
    }

    fn implements(&self, root: Node, src: &[u8]) -> Vec<ImplRel> {
        let mut out = Vec::new();
        crate::ingestion::parser::walk(root, |n| {
            if n.kind() == "class_definition"
                && let (Some(name), Some(bases)) =
                    (n.child_by_field_name("name"), n.child_by_field_name("superclasses"))
            {
                let mut cursor = bases.walk();
                for b in bases.named_children(&mut cursor).filter(|b| matches!(b.kind(), "identifier" | "attribute")) {
                    let base = text(b, src).rsplit('.').next().unwrap_or("").to_string();
                    if !base.is_empty() && base != "object" {
                        out.push(ImplRel {
                            type_name: text(name, src).to_string(),
                            trait_name: base,
                            line: n.start_position().row + 1,
                        });
                    }
                }
            }
            true
        });
        out
    }

    fn flatten_import(&self, node: Node, src: &[u8], out: &mut Vec<ImportDecl>) {
        match node.kind() {
            "import_statement" => {
                let mut c = node.walk();
                for n in node.children_by_field_name("name", &mut c) {
                    let (path, alias) = dotted_and_alias(n, src);
                    let (module, name) = split_dotted(&path);
                    out.push(ImportDecl { module_path: module, name, alias });
                }
            }
            "import_from_statement" => {
                let module =
                    node.child_by_field_name("module_name").map_or(String::new(), |m| text(m, src).to_string());
                let mut c = node.walk();
                let mut any = false;
                for n in node.children_by_field_name("name", &mut c) {
                    any = true;
                    let (name, alias) = dotted_and_alias(n, src);
                    out.push(ImportDecl { module_path: module.clone(), name, alias });
                }
                if !any && node.children(&mut node.walk()).any(|c| c.kind() == "wildcard_import") {
                    out.push(ImportDecl { module_path: module, name: "*".to_string(), alias: None });
                }
            }
            _ => {}
        }
    }
}

fn enclosing_class(def: Node) -> Option<Node> {
    let mut p = def.parent()?;
    if p.kind() == "decorated_definition" {
        p = p.parent()?;
    }
    (p.kind() == "block").then(|| p.parent()).flatten().filter(|g| g.kind() == "class_definition")
}

/// `a.b as c` -> (`a.b`, Some(`c`))
fn dotted_and_alias(n: Node, src: &[u8]) -> (String, Option<String>) {
    if n.kind() == "aliased_import" {
        let path = n.child_by_field_name("name").map_or(String::new(), |x| text(x, src).to_string());
        let alias = n.child_by_field_name("alias").map(|x| text(x, src).to_string());
        (path, alias)
    } else {
        (text(n, src).to_string(), None)
    }
}

fn split_dotted(path: &str) -> (String, String) {
    match path.rsplit_once('.') {
        Some((m, n)) => (m.to_string(), n.to_string()),
        None => (String::new(), path.to_string()),
    }
}

/// `@app.route("/x")` -> `route`
fn decorator_names(def: Node, src: &[u8]) -> Vec<String> {
    let Some(parent) = def.parent().filter(|p| p.kind() == "decorated_definition") else {
        return Vec::new();
    };
    let mut cursor = parent.walk();
    parent
        .children(&mut cursor)
        .filter(|c| c.kind() == "decorator")
        .map(|d| {
            let t = text(d, src).trim_start_matches('@');
            let head = t.split('(').next().unwrap_or(t).trim();
            head.rsplit('.').next().unwrap_or(head).to_string()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::ingestion::model::*;
    use crate::ingestion::parser::parse_file;
    use crate::ingestion::walker::{Language, SourceFile};

    fn parse(src: &str) -> ParsedFile {
        let f = SourceFile {
            rel_path: "app/billing/stripe.py".into(),
            language: Language::Python,
            blake3: String::new(),
            line_count: 0,
            source: src.into(),
        };
        parse_file(&f).expect("python queries must compile").unwrap()
    }

    fn sym<'a>(p: &'a ParsedFile, name: &str) -> &'a ParsedSymbol {
        p.symbols
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("no symbol {name}: {:?}", p.symbols.iter().map(|s| &s.name).collect::<Vec<_>>()))
    }

    #[test]
    fn symbols_kinds_ids_visibility_and_docstrings() {
        let p = parse(
            r#"
MAX_RETRIES = 3
counter = 0

def charge_card(card: Card, amount: int = 1) -> Receipt:
    """Charges a card.

    Raises on decline."""
    return gateway.send(card, amount)

def _helper():
    pass

class Gateway:
    def send(self, card, amount):
        return 1

    def _retry(self):
        pass

    def __init__(self):
        pass
"#,
        );
        let c = sym(&p, "charge_card");
        assert_eq!((c.kind, c.visibility), ("fn", "pub"));
        assert!(c.symbol_id.starts_with("app/billing/stripe.py::::charge_card#fn:"), "{}", c.symbol_id);
        assert_eq!(c.signature, "def charge_card(card: Card, amount: int = 1) -> Receipt:");
        assert_eq!(c.doc_comment, "Charges a card.\n\nRaises on decline.");
        assert_eq!(sym(&p, "_helper").visibility, "private");
        assert_eq!(sym(&p, "Gateway").kind, "class");
        let send = sym(&p, "send");
        assert_eq!(send.kind, "method");
        assert!(send.symbol_id.contains("::Gateway::send#method:"), "{}", send.symbol_id);
        assert_eq!(sym(&p, "_retry").visibility, "private");
        assert_eq!(sym(&p, "__init__").visibility, "pub");
        assert_eq!((sym(&p, "MAX_RETRIES").kind, sym(&p, "counter").kind), ("const", "global"));
    }

    #[test]
    fn complexity_counts_python_branches_and_skips_nested_defs() {
        let p = parse(
            r#"
def f(x, y):
    def inner(z):
        if z: pass
    if x and y:
        return 1
    elif x or y:
        return 2
    for i in x:
        while y:
            pass
    try:
        pass
    except ValueError:
        pass
    return 3 if x else 4
"#,
        );
        // if, and, elif, or, for, while, except, ternary
        assert_eq!(sym(&p, "f").cyclomatic_complexity, 9);
        assert_eq!(sym(&p, "inner").cyclomatic_complexity, 2);
    }

    #[test]
    fn risk_flags_follow_the_spec() {
        let p = parse(
            r#"
import pickle, subprocess, os
def a(s): return eval(s)
def b(d): return pickle.loads(d)
def c(cmd): subprocess.run(cmd, shell=True)
def d():
    try:
        pass
    except:
        pass
def e(cur, name): cur.execute("SELECT * FROM t WHERE n = '%s'" % name)
def f(cur, name): cur.execute(f"SELECT * FROM t WHERE n = '{name}'")
def g(cur, name): cur.execute("SELECT 1 WHERE n = ?", (name,))
def h(p): return open(p).read()
def i(): return os.system("ls")
"#,
        );
        for n in ["a", "b", "c", "i"] {
            assert!(sym(&p, n).risk_flags.contains(&"command_exec".to_string()), "{n}: {:?}", sym(&p, n).risk_flags);
        }
        assert_eq!(sym(&p, "d").risk_flags, ["error_swallow"]);
        assert_eq!(sym(&p, "e").risk_flags, ["raw_sql"]);
        assert_eq!(sym(&p, "f").risk_flags, ["raw_sql"]);
        assert!(sym(&p, "g").risk_flags.is_empty(), "parameterised query is fine: {:?}", sym(&p, "g").risk_flags);
        assert_eq!(sym(&p, "h").risk_flags, ["external_io"]);
        assert!(sym(&p, "a").requires_strict_contract);
    }

    #[test]
    fn entrypoints_calls_callbacks_types_and_imports() {
        let p = parse(
            r#"
import os.path as osp
import json
from app.auth import verify, check as chk
from . import sibling
from .models import *

@app.route("/charge", methods=["POST"])
def charge(req: Request) -> Response:
    verify(req)
    req.validate()
    models.save(req)
    threading.Thread(target=worker).start()
    return Response()

def worker(): pass
def main(): charge(None)
def plain(): pass
"#,
        );
        for n in ["charge", "main"] {
            assert!(sym(&p, n).is_entrypoint, "{n}");
        }
        assert!(!sym(&p, "plain").is_entrypoint);

        let charge = sym(&p, "charge").symbol_id.clone();
        let find = |name: &str| p.calls.iter().find(|c| c.from_symbol_id == charge && c.target_name == name);
        assert_eq!(find("verify").unwrap().kind, CallKind::Direct);
        assert_eq!(find("validate").unwrap().receiver.as_deref(), Some("req"));
        assert_eq!(find("save").unwrap().receiver.as_deref(), Some("models"));
        let cb = find("worker").unwrap();
        assert_eq!((cb.kind.clone(), cb.via.as_deref()), (CallKind::Callback, Some("Thread")));

        assert!(p.type_refs.iter().any(|t| t.type_name == "Request" && t.kind == TypeRefKind::Param));
        assert!(p.type_refs.iter().any(|t| t.type_name == "Response" && t.kind == TypeRefKind::Return));

        let imp = |m: &str, n: &str| p.imports.iter().find(|i| i.module_path == m && i.name == n);
        assert_eq!(imp("os", "path").unwrap().alias.as_deref(), Some("osp"));
        assert!(imp("", "json").is_some());
        assert!(imp("app.auth", "verify").is_some());
        assert_eq!(imp("app.auth", "check").unwrap().alias.as_deref(), Some("chk"));
        assert!(imp(".", "sibling").is_some());
        assert!(imp(".models", "*").is_some());
    }

    #[test]
    fn ast_hash_ignores_comments_and_formatting() {
        let a = parse("def f(x):\n    return x + 1\n");
        let b = parse("# note\ndef f( x ):\n    # inner\n    return x +  1\n");
        let c = parse("def f(x):\n    return x + 2\n");
        assert_eq!(sym(&a, "f").ast_blake3, sym(&b, "f").ast_blake3);
        assert_ne!(sym(&a, "f").ast_blake3, sym(&c, "f").ast_blake3);
    }
}
