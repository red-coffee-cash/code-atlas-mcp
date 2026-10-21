//! Plain data produced by the parser and consumed by the linker and DB writers.

use super::walker::Language;

#[derive(Debug, Clone)]
pub struct ParsedSymbol {
    pub symbol_id: String,
    pub qualified_name: String,
    pub name: String,
    /// "fn" | "method" | "struct" | "class" | "interface" | "enum" | "const" | "global"
    pub kind: &'static str,
    /// "pub" | "crate" | "private" | "export"
    pub visibility: &'static str,
    pub file_path: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub end_line: usize,
    pub signature: String,
    pub doc_comment: String,
    pub ast_blake3: String,
    pub cyclomatic_complexity: u32,
    pub risk_flags: Vec<String>,
    pub requires_strict_contract: bool,
    /// Detected from the definition itself (main, route attributes, FFI exports).
    /// The linker may add more from callback registration (e.g. axum `.route`).
    pub is_entrypoint: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallKind {
    /// `foo(x)`, `a.foo(x)`, `m::foo(x)`
    Direct,
    /// Identifier passed as an argument: `get(login_handler)` (spec §10.2)
    Callback,
    /// Method name passed as a symbol (`before_action :authorize`, `send(:run)`); it names a
    /// method of the caller's own class at least as often as a free function (Ruby).
    SymbolRef,
    /// Identifier inside a macro `token_tree` (spec §10.3)
    MacroIdent,
}

#[derive(Debug, Clone)]
pub struct CallSite {
    pub from_symbol_id: String,
    pub target_name: String,
    pub receiver: Option<String>,
    pub line: usize,
    pub snippet: String,
    pub kind: CallKind,
    /// For callbacks: the callee they were passed to (`get`, `map`, `route`, ...).
    pub via: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeRefKind {
    Param,
    Return,
}

#[derive(Debug, Clone)]
pub struct TypeRef {
    pub from_symbol_id: String,
    pub type_name: String,
    pub kind: TypeRefKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportDecl {
    /// Module the name comes from, in the source language's own notation.
    pub module_path: String,
    /// Imported name, or `*` for glob imports.
    pub name: String,
    /// Local name when renamed (`use a::b as c`).
    pub alias: Option<String>,
}

/// `impl Trait for Type`, `class A(Base)`, `class A implements I`: `type_name` implements or
/// extends `trait_name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImplRel {
    pub type_name: String,
    pub trait_name: String,
    pub line: usize,
}

#[derive(Debug, Clone)]
pub struct ParsedFile {
    pub rel_path: String,
    pub language: Language,
    pub symbols: Vec<ParsedSymbol>,
    pub calls: Vec<CallSite>,
    pub type_refs: Vec<TypeRef>,
    pub imports: Vec<ImportDecl>,
    pub impls: Vec<ImplRel>,
    /// Go only: the package's full import path (`github.com/acme/app/internal/auth`), derived from
    /// the nearest `go.mod` during sync. `None` when unknown.
    pub package_path: Option<String>,
}
