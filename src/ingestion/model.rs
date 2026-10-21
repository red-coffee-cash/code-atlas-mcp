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
    /// Positional parameters it accepts, when the frontend knows (calls with another count can't
    /// target it).
    pub arity: Option<Arity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arity {
    pub min: usize,
    /// `None` with a splat (`*rest`).
    pub max: Option<usize>,
    /// Declares keyword parameters (`k:`, `**opts`), which absorb `key: value` arguments.
    pub keywords: bool,
}

/// The argument shape of a call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallArgs {
    pub positional: usize,
    /// Passes `key: value` pairs.
    pub keywords: bool,
}

impl Arity {
    /// Whether a call with `args` can bind to these parameters. Ruby passes trailing `k: v` pairs
    /// to a method without keyword parameters as one positional hash.
    pub fn accepts(&self, args: CallArgs) -> bool {
        let n = args.positional + usize::from(args.keywords && !self.keywords);
        n >= self.min && self.max.is_none_or(|max| n <= max)
    }
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
    /// `None` when unknown (a splat argument, a callback reference, an unsupported language).
    pub args: Option<CallArgs>,
    /// Types of the positional arguments, as far as the caller's code shows them.
    pub arg_types: Vec<Option<TypeExpr>>,
}

/// A receiver's type as far as one file can tell. Resolved by the linker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeExpr {
    /// A type as written: `*pkg.T`, `[]T`, `map[K]V`, `Foo::Bar`.
    Named(String),
    /// A name with no local binding: an import, or a package-level variable or function. In Ruby,
    /// a constant used as a value: the class object itself (`Gem` in `Gem.configuration`).
    Ident(String),
    /// `x.f`: a field of `x` (or a package member, when `x` is an import).
    Field(Box<TypeExpr>, String),
    /// The (first) result of calling the callee: `f()`, `x.m()`, `pkg.F()`.
    Call(Box<TypeExpr>),
    /// An element of a slice, array or map: `xs[i]`, the value of `range xs`.
    Elem(Box<TypeExpr>),
    /// The first variable of `range xs`: an index, a map key, or a channel's element.
    Key(Box<TypeExpr>),
    /// The positional parameter at this index of the named method (of the enclosing class), typed
    /// by the linker from the arguments its callers pass.
    Param(String, usize),
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
    /// (receiver type or class, or "" for a function; name; (first) result type).
    pub results: Vec<(String, String, TypeExpr)>,
    /// Package-level variables: (name, type).
    pub vars: Vec<(String, TypeExpr)>,
    /// Go `//go:build` constraint expression, if the file has one.
    pub build_constraint: Option<String>,
    /// (class, method) for methods that exist without a `def`: Ruby accessors, `Struct` members,
    /// aliases, delegators, `define_method` with a literal name.
    pub methods: Vec<(String, String)>,
    /// Classes defined without a `class` keyword (Ruby `Point = Struct.new(:x, :y)`), and Go types
    /// declared inside functions as (name, "struct" | "interface") in `local_types`.
    pub classes: Vec<String>,
    pub local_types: Vec<(String, &'static str)>,
    /// (receiver type or "", function, a value it returns): every first value of every `return`,
    /// with locals expanded to everything assigned to them. `None` is a value of unknown type
    /// (which makes the function's result set unknown).
    pub returns: Vec<(String, String, Option<TypeExpr>)>,
    /// (struct expression, field or `*` for all fields, a value stored there) from keyed composite
    /// literals and `x.f = v`. `None` is a value of unknown type.
    pub field_flows: Vec<(TypeExpr, String, Option<TypeExpr>)>,
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
