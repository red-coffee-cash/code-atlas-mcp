use std::sync::OnceLock;

use anyhow::Result;
use tree_sitter::Node;

use super::{Compiled, Frontend, text};
use crate::ingestion::model::{ImplRel, ImportDecl};
use crate::ingestion::symbol_id::trait_impl_scope;
use crate::ingestion::walker::SourceFile;

pub struct RustFrontend {
    compiled: OnceLock<Result<Compiled, String>>,
}

pub static RUST: RustFrontend = RustFrontend { compiled: OnceLock::new() };

/// Attribute names (last path segment) that make a function externally reachable.
const ENTRYPOINT_ATTRS: &[&str] = &[
    "main",
    "get",
    "post",
    "put",
    "delete",
    "patch",
    "head",
    "options",
    "route",
    "no_mangle",
    "export_name",
    "wasm_bindgen",
    "pyfunction",
    "napi",
    "tool",
    "command",
];

/// Macros whose arguments hit a database or the filesystem.
const IO_MACROS: &[&str] = &["query", "query_as", "query_scalar", "query_file", "include_str", "include_bytes"];

impl Frontend for RustFrontend {
    fn compiled(&self, _file: &SourceFile) -> Result<&Compiled> {
        self.compiled
            .get_or_init(|| {
                Compiled::build(
                    tree_sitter_rust::LANGUAGE.into(),
                    include_str!("../../../queries/rust/symbols.scm"),
                    include_str!("../../../queries/rust/calls.scm"),
                    include_str!("../../../queries/rust/risks.scm"),
                    "rust",
                )
                .map_err(|e| format!("{e:#}"))
            })
            .as_ref()
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    fn symbol_kind(&self, def: Node, _src: &[u8]) -> Option<&'static str> {
        Some(match def.kind() {
            "function_item" if in_impl_or_trait(def) => "method",
            "function_item" => "fn",
            "struct_item" => "struct",
            "enum_item" => "enum",
            "trait_item" => "interface",
            "const_item" => "const",
            "static_item" => "global",
            _ => return None,
        })
    }

    fn scope_chain(&self, def: Node, src: &[u8]) -> Vec<String> {
        let mut chain = Vec::new();
        let mut cur = def.parent();
        while let Some(n) = cur {
            match n.kind() {
                "impl_item" => {
                    let ty = n.child_by_field_name("type").map(|t| base_type_name(text(t, src)));
                    let tr = n.child_by_field_name("trait").map(|t| base_type_name(text(t, src)));
                    match (tr, ty) {
                        (Some(tr), Some(ty)) => chain.push(trait_impl_scope(&tr, &ty)),
                        (None, Some(ty)) => chain.push(ty),
                        _ => {}
                    }
                }
                "trait_item" | "mod_item" | "function_item" => {
                    if let Some(name) = n.child_by_field_name("name") {
                        chain.push(text(name, src).to_string());
                    }
                }
                _ => {}
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
            .filter_map(|p| match p.kind() {
                "self_parameter" => Some("self".to_string()),
                "parameter" => p.child_by_field_name("type").map(|t| text(t, src).to_string()),
                _ => None,
            })
            .collect()
    }

    fn visibility(&self, def: Node, src: &[u8]) -> &'static str {
        let mut cursor = def.walk();
        let vis = def.children(&mut cursor).find(|c| c.kind() == "visibility_modifier");
        match vis {
            Some(v) if text(v, src).trim() == "pub" => "pub",
            Some(_) => "crate",
            // Trait methods and trait-impl methods are as visible as the trait.
            None if def.kind() == "function_item" && in_trait_context(def) => "pub",
            None => "private",
        }
    }

    fn doc_comment(&self, def: Node, src: &[u8]) -> String {
        let mut lines = Vec::new();
        let mut prev = def.prev_sibling();
        while let Some(n) = prev {
            match n.kind() {
                "attribute_item" => {}
                "line_comment" | "block_comment" => {
                    let t = text(n, src);
                    if let Some(rest) = t.strip_prefix("///") {
                        lines.push(rest.strip_prefix(' ').unwrap_or(rest).trim_end().to_string());
                    } else if t.starts_with("/**") {
                        let body = t.trim_start_matches("/**").trim_end_matches("*/");
                        lines.push(body.trim().to_string());
                    } else {
                        break;
                    }
                }
                _ => break,
            }
            prev = n.prev_sibling();
        }
        lines.reverse();
        lines.join("\n")
    }

    fn is_entrypoint(&self, def: Node, src: &[u8]) -> bool {
        if def.kind() != "function_item" {
            return false;
        }
        let top_level = def.parent().is_some_and(|p| p.kind() == "source_file");
        let is_main = top_level && def.child_by_field_name("name").is_some_and(|n| text(n, src) == "main");
        is_main
            || attribute_names(def, src).iter().any(|a| ENTRYPOINT_ATTRS.contains(&a.as_str()))
            || is_public_extern(def, src)
    }

    fn is_branch(&self, node: Node, src: &[u8]) -> bool {
        match node.kind() {
            "if_expression" | "match_arm" | "for_expression" | "while_expression" | "try_expression" => true,
            "binary_expression" => {
                node.child_by_field_name("operator").is_some_and(|op| matches!(text(op, src), "&&" | "||"))
            }
            _ => false,
        }
    }

    fn is_nested_def(&self, node: Node) -> bool {
        matches!(node.kind(), "function_item" | "impl_item" | "trait_item" | "mod_item")
    }

    fn flatten_import(&self, node: Node, src: &[u8], out: &mut Vec<ImportDecl>) {
        flatten_use(node, "", src, out);
    }

    fn implements(&self, root: Node, src: &[u8]) -> Vec<ImplRel> {
        let mut out = Vec::new();
        super::super::parser::walk(root, |n| {
            if n.kind() == "impl_item"
                && let (Some(tr), Some(ty)) = (n.child_by_field_name("trait"), n.child_by_field_name("type"))
            {
                out.push(ImplRel {
                    type_name: base_type_name(text(ty, src)),
                    trait_name: base_type_name(text(tr, src)),
                    line: n.start_position().row + 1,
                });
            }
            true
        });
        out
    }

    fn macro_is_risky(&self, macro_name: &str, tokens: Node, src: &[u8]) -> bool {
        if IO_MACROS.contains(&macro_name) {
            return true;
        }
        let mut risky = false;
        super::super::parser::walk(tokens, |n| {
            if n.kind() == "unsafe" || (n.kind() == "identifier" && text(n, src) == "unsafe") {
                risky = true;
            }
            !risky
        });
        risky
    }
}

fn in_impl_or_trait(def: Node) -> bool {
    def.parent()
        .filter(|p| p.kind() == "declaration_list")
        .and_then(|p| p.parent())
        .is_some_and(|g| matches!(g.kind(), "impl_item" | "trait_item"))
}

fn in_trait_context(def: Node) -> bool {
    def.parent()
        .filter(|p| p.kind() == "declaration_list")
        .and_then(|p| p.parent())
        .is_some_and(|g| g.kind() == "trait_item" || g.child_by_field_name("trait").is_some())
}

/// `&mut dyn a::Foo<T>` -> `Foo`
fn base_type_name(t: &str) -> String {
    let t = t.trim().trim_start_matches('&').trim();
    let t = t.strip_prefix("mut ").unwrap_or(t);
    let t = t.strip_prefix("dyn ").unwrap_or(t).trim();
    let t = t.split('<').next().unwrap_or(t).trim();
    t.rsplit("::").next().unwrap_or(t).to_string()
}

/// Last path segment of each attribute directly above `def`: `#[tokio::main]` -> `main`.
fn attribute_names(def: Node, src: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    let mut prev = def.prev_sibling();
    while let Some(n) = prev {
        match n.kind() {
            "attribute_item" => {
                let t = text(n, src).trim_start_matches("#[").trim_end_matches(']');
                let path = t.split(['(', '=', ' ']).next().unwrap_or(t);
                names.push(path.rsplit("::").next().unwrap_or(path).to_string());
            }
            "line_comment" | "block_comment" => {}
            _ => break,
        }
        prev = n.prev_sibling();
    }
    names
}

fn is_public_extern(def: Node, src: &[u8]) -> bool {
    let mut cursor = def.walk();
    let mut is_pub = false;
    let mut is_extern = false;
    for c in def.children(&mut cursor) {
        match c.kind() {
            "visibility_modifier" => is_pub = text(c, src).trim() == "pub",
            "function_modifiers" => is_extern = text(c, src).contains("extern"),
            _ => {}
        }
    }
    is_pub && is_extern
}

fn join(prefix: &str, rest: &str) -> String {
    match (prefix.is_empty(), rest.is_empty()) {
        (true, _) => rest.to_string(),
        (_, true) => prefix.to_string(),
        _ => format!("{prefix}::{rest}"),
    }
}

/// Splits `a::b::c` into (`a::b`, `c`).
fn split_last(path: &str) -> (String, String) {
    match path.rsplit_once("::") {
        Some((m, n)) => (m.to_string(), n.to_string()),
        None => (String::new(), path.to_string()),
    }
}

fn flatten_use(node: Node, prefix: &str, src: &[u8], out: &mut Vec<ImportDecl>) {
    match node.kind() {
        "identifier" | "crate" | "super" | "self" | "scoped_identifier" => {
            let full = join(prefix, text(node, src));
            if text(node, src) == "self" && !prefix.is_empty() {
                let (module, name) = split_last(prefix);
                out.push(ImportDecl { module_path: module, name, alias: None });
            } else {
                let (module, name) = split_last(&full);
                out.push(ImportDecl { module_path: module, name, alias: None });
            }
        }
        "use_as_clause" => {
            let (Some(path), alias) = (node.child_by_field_name("path"), node.child_by_field_name("alias")) else {
                return;
            };
            let (module, name) = split_last(&join(prefix, text(path, src)));
            out.push(ImportDecl { module_path: module, name, alias: alias.map(|a| text(a, src).to_string()) });
        }
        "scoped_use_list" => {
            let base = node
                .child_by_field_name("path")
                .map(|p| join(prefix, text(p, src)))
                .unwrap_or_else(|| prefix.to_string());
            if let Some(list) = node.child_by_field_name("list") {
                flatten_use(list, &base, src, out);
            }
        }
        "use_list" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                flatten_use(child, prefix, src, out);
            }
        }
        "use_wildcard" => {
            let t = text(node, src).trim_end_matches('*').trim_end_matches("::");
            out.push(ImportDecl { module_path: join(prefix, t), name: "*".to_string(), alias: None });
        }
        _ => {}
    }
}
