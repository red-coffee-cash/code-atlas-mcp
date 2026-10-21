//! Per-language knowledge for the generic parser engine: the `.scm` queries (spec §4.1) plus the
//! small hooks that queries can't express (scope chains, parameter types, branch node kinds).

use anyhow::{Context, Result};
use tree_sitter::{Language as TsLanguage, Node, Query};

use super::model::{ImplRel, ImportDecl, TypeExpr, TypeFacts};
use super::walker::{Language, SourceFile};

pub mod go;
pub mod python;
pub mod ruby;
pub mod rust;
pub mod typescript;

pub struct Compiled {
    pub language: TsLanguage,
    pub symbols: Query,
    pub calls: Query,
    pub risks: Query,
}

impl Compiled {
    pub fn build(language: TsLanguage, symbols: &str, calls: &str, risks: &str, name: &str) -> Result<Self> {
        let q =
            |src: &str, what: &str| Query::new(&language, src).with_context(|| format!("compiling {name}/{what}.scm"));
        Ok(Self { symbols: q(symbols, "symbols")?, calls: q(calls, "calls")?, risks: q(risks, "risks")?, language })
    }
}

pub trait Frontend: Sync {
    fn compiled(&self, file: &SourceFile) -> Result<&Compiled>;

    /// `None` drops the match (e.g. a construct that isn't a symbol in this language).
    fn symbol_kind(&self, def: Node, src: &[u8]) -> Option<&'static str>;
    /// Enclosing named scopes, outermost first (spec §3.1 `scope_chain`).
    fn scope_chain(&self, def: Node, src: &[u8]) -> Vec<String>;
    fn param_types(&self, def: Node, src: &[u8]) -> Vec<String>;
    fn visibility(&self, def: Node, src: &[u8]) -> &'static str;
    fn doc_comment(&self, def: Node, src: &[u8]) -> String;
    fn is_entrypoint(&self, def: Node, src: &[u8]) -> bool;

    /// The node that carries `parameters` / `return_type` / `body`. Differs from the symbol's
    /// definition node for `const f = () => {}`, where those live on the arrow function.
    fn function_node<'t>(&self, def: Node<'t>) -> Node<'t> {
        def
    }

    /// Nodes that add one to cyclomatic complexity (spec §4.3).
    fn is_branch(&self, node: Node, src: &[u8]) -> bool;
    /// Nodes whose subtree belongs to a different symbol and is skipped when counting.
    fn is_nested_def(&self, node: Node) -> bool;

    /// Expands one `@import.path` capture into flat declarations.
    fn flatten_import(&self, node: Node, src: &[u8], out: &mut Vec<ImportDecl>);

    /// Inheritance and trait-implementation relations declared in the file.
    fn implements(&self, _root: Node, _src: &[u8]) -> Vec<ImplRel> {
        Vec::new()
    }

    /// The receiver's type as far as the enclosing code shows it (parameters, locals, literals).
    fn receiver_type(&self, _receiver: Node, _src: &[u8]) -> Option<TypeExpr> {
        None
    }

    /// File-level declarations that type receivers in other files (fields, results, globals).
    fn type_facts(&self, _root: Node, _src: &[u8]) -> TypeFacts {
        TypeFacts::default()
    }

    /// Whether a macro invocation wraps external I/O or unsafe code (spec §10.3).
    fn macro_is_risky(&self, _macro_name: &str, _tokens: Node, _src: &[u8]) -> bool {
        false
    }
}

/// `None` for languages without a frontend; such files are skipped (counted as unsupported).
pub fn frontend_for(file: &SourceFile) -> Option<&'static dyn Frontend> {
    match file.language {
        Language::Rust => Some(&rust::RUST),
        Language::Python => Some(&python::PYTHON),
        Language::TypeScript => Some(&typescript::TYPESCRIPT),
        Language::Go => Some(&go::GO),
        Language::Ruby => Some(&ruby::RUBY),
    }
}

/// Maps the suffix of a `@risk.*` capture to the flag stored on `symbol_node.risk_flags`.
pub fn risk_flag_for_capture(suffix: &str) -> Option<&'static str> {
    Some(match suffix {
        "unsafe" => "unsafe_usage",
        "unwrap" => "unchecked_unwrap",
        "memory" => "memory_unsafe",
        "exec" => "command_exec",
        "io" => "external_io",
        "concurrency" => "concurrency_primitive",
        "raw_sql" => "raw_sql",
        "error_swallow" => "error_swallow",
        "xss" => "xss_sink",
        "deserialization" => "unsafe_deserialization",
        "alloc" => "manual_alloc",
        _ => return None,
    })
}

/// A comment on a line of its own, not trailing code (`A = 1 // first`); only those can be docs.
pub(crate) fn own_line(comment: Node) -> bool {
    comment.prev_sibling().is_none_or(|p| p.end_position().row < comment.start_position().row)
}

pub(crate) fn text<'a>(node: Node, src: &'a [u8]) -> &'a str {
    node.utf8_text(src).unwrap_or("")
}
