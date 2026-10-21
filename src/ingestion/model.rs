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
    /// What the frontend could tell about the receiver's type from the caller's own code; the
    /// linker finishes it with repo-wide facts (field types, function results).
    pub receiver_type: Option<TypeExpr>,
}

/// A receiver's type as far as one file can tell. Resolved by the linker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeExpr {
    /// A type as written: `*pkg.T`, `[]T`, `map[K]V`, `Foo::Bar`.
    Named(String),
    /// A name with no local binding: an import, or a package-level variable or function.
    Ident(String),
    /// `x.f`: a field of `x` (or a package member, when `x` is an import).
    Field(Box<TypeExpr>, String),
    /// The (first) result of calling the callee: `f()`, `x.m()`, `pkg.F()`.
    Call(Box<TypeExpr>),
    /// An element of a slice, array or map: `xs[i]`, the value of `range xs`.
    Elem(Box<TypeExpr>),
}

/// Declarations the linker needs to type receivers across files.
#[derive(Debug, Clone, Default)]
pub struct TypeFacts {
    /// (owner type, field, type): Go struct fields, Ruby instance variables.
    pub fields: Vec<(String, String, TypeExpr)>,
    /// (owner type, embedded type as written): Go struct embedding.
    pub embeds: Vec<(String, String)>,
    /// (interface, method) declared directly on a Go interface.
    pub iface_methods: Vec<(String, String)>,
    /// (receiver type or "" for a function, name, first result type as written).
    pub results: Vec<(String, String, String)>,
    /// Package-level variables: (name, type).
    pub vars: Vec<(String, TypeExpr)>,
    /// Go `//go:build` constraint expression, if the file has one.
    pub build_constraint: Option<String>,
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
    /// Package qualifier as written (`load` in Go `*load.Package`); `None` when unqualified.
    pub qualifier: Option<String>,
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
    pub facts: TypeFacts,
}
