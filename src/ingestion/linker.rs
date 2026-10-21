//! Two-pass static and heuristic symbol linker (spec §4.4, §10.2, §10.3).
//!
//! Pass 1 indexes every symbol and every file's imports. Pass 2 resolves each call site,
//! callback reference, macro identifier and signature type to edges, trying tiers in order
//! (same file, imported modules, whole repo) and stopping at the first tier with candidates.

use std::collections::{BTreeMap, HashMap, HashSet};

use std::cell::RefCell;

use super::model::{CallKind, CallSite, ParsedFile, ParsedSymbol, TypeExpr, TypeRefKind};
use super::walker::Language;

/// Beyond this many candidates a name is too generic (`clone`, `to_string`) to link.
const MAX_HEURISTIC_CANDIDATES: usize = 8;

/// Callees that register their function arguments as externally invoked handlers.
const ROUTE_REGISTRARS: &[&str] = &[
    "route",
    "get",
    "post",
    "put",
    "delete",
    "patch",
    "head",
    "options",
    "any",
    "on",
    "service",
    "resource",
    "handler",
    "add_route",
    "subscribe",
    "listen",
    "Handle",
    "HandleFunc",
    "GET",
    "POST",
    "PUT",
    "DELETE",
    "PATCH",
];

const CALLABLE: &[&str] = &["fn", "method"];
const TYPES: &[&str] = &["struct", "enum", "interface", "class"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub kind: &'static str,
    pub confidence: &'static str,
    pub lines: Vec<usize>,
    pub snippet: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tier {
    Exact,
    Heuristic,
}

type SymRef = (usize, usize);

struct Index<'a> {
    files: &'a [ParsedFile],
    by_name: HashMap<&'a str, Vec<SymRef>>,
    /// normalized module key -> file indices
    modules: HashMap<String, Vec<usize>>,
    module_keys: Vec<String>,
    /// per file: files reachable through its imports
    imported_files: Vec<HashSet<usize>>,
    /// per file: local alias -> original imported name
    aliases: Vec<HashMap<&'a str, &'a str>>,
    /// Go, per file: local package name (alias or last path segment) -> that package's files
    go_imports: Vec<HashMap<&'a str, Vec<usize>>>,
    /// Go, per file: files of dot-imported packages, whose names are callable unqualified
    go_dot_imports: Vec<HashSet<usize>>,
    ruby: RubyModel<'a>,
    go: GoModel<'a>,
}

/// Resolves all references in `files` to edges and marks symbols registered as route handlers
/// as entrypoints (which also makes them require a strict contract).
pub fn link(files: &mut [ParsedFile]) -> Vec<Edge> {
    let (edges, new_entrypoints) = {
        let index = Index::build(files);
        resolve_all(&index)
    };
    for f in files.iter_mut() {
        for s in &mut f.symbols {
            if new_entrypoints.contains(&s.symbol_id) {
                s.is_entrypoint = true;
                s.requires_strict_contract = true;
            }
        }
    }
    edges
}

/// `src/auth/mod.rs` -> `auth`, `app/billing/stripe.py` -> `app::billing::stripe`
fn module_key(rel_path: &str) -> String {
    // A Go package is a directory: every `.go` file in it shares one key.
    if let Some(dir) = rel_path.strip_suffix(".go") {
        let dir = dir.rsplit_once('/').map_or("", |(d, _)| d);
        return dir.split('/').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("::");
    }
    let stem = rel_path.rsplit_once('.').map_or(rel_path, |(s, _)| s);
    let mut parts: Vec<&str> = stem.split('/').collect();
    if matches!(parts.first(), Some(&"src") | Some(&"lib")) {
        parts.remove(0);
    }
    if matches!(parts.last(), Some(&"mod") | Some(&"index") | Some(&"__init__") | Some(&"lib") | Some(&"main"))
        && parts.len() > 1
    {
        parts.pop();
    }
    parts.join("::")
}

/// Strips relative-path prefixes and unifies separators: `crate::a.b` -> `a::b`.
fn normalize_module(path: &str) -> String {
    let mut p = path.replace(['.', '/'], "::");
    loop {
        let before = p.len();
        for prefix in ["crate::", "self::", "super::", "::"] {
            if let Some(rest) = p.strip_prefix(prefix) {
                p = rest.to_string();
            }
        }
        if p == "crate" || p == "self" || p == "super" {
            p.clear();
        }
        if p.len() == before {
            break;
        }
    }
    p.trim_matches(':').to_string()
}

impl<'a> Index<'a> {
    fn build(files: &'a [ParsedFile]) -> Self {
        let mut by_name: HashMap<&str, Vec<SymRef>> = HashMap::new();
        for (fi, f) in files.iter().enumerate() {
            for (si, s) in f.symbols.iter().enumerate() {
                by_name.entry(s.name.as_str()).or_default().push((fi, si));
            }
        }
        let module_keys: Vec<String> = files.iter().map(|f| module_key(&f.rel_path)).collect();
        let mut modules: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, k) in module_keys.iter().enumerate() {
            modules.entry(k.clone()).or_default().push(i);
        }

        let mut index = Self {
            files,
            by_name,
            modules,
            module_keys,
            imported_files: vec![HashSet::new(); files.len()],
            aliases: vec![HashMap::new(); files.len()],
            go_imports: vec![HashMap::new(); files.len()],
            go_dot_imports: vec![HashSet::new(); files.len()],
            ruby: RubyModel::build(files),
            go: GoModel::default(),
        };
        index.go = GoModel::build(files, &index.module_keys);
        let go_packages = GoPackages::build(files, &index.module_keys);
        let by_path: HashMap<&str, usize> = files.iter().enumerate().map(|(i, f)| (f.rel_path.as_str(), i)).collect();
        for (fi, f) in files.iter().enumerate() {
            if f.language == Language::Go {
                for imp in &f.imports {
                    let targets: Vec<usize> =
                        go_packages.resolve(&imp.module_path).into_iter().filter(|&t| t != fi).collect();
                    index.imported_files[fi].extend(targets.iter().copied());
                    if imp.name == "*" {
                        index.go_dot_imports[fi].extend(targets);
                    } else {
                        index.go_imports[fi].insert(imp.alias.as_deref().unwrap_or(&imp.name), targets);
                    }
                }
                continue;
            }
            for imp in &f.imports {
                // Ruby `require_relative`: an exact file next to the requiring one.
                if let Some(rel) = imp.module_path.strip_prefix("./") {
                    let dir = f.rel_path.rsplit_once('/').map_or("", |(d, _)| d);
                    let target = join_relative(dir, rel, &imp.name);
                    let found =
                        [format!("{target}.rb"), target].into_iter().find_map(|p| by_path.get(p.as_str()).copied());
                    index.imported_files[fi].extend(found.filter(|&t| t != fi));
                    continue;
                }
                let module = normalize_module(&imp.module_path);
                let mut targets = index.files_for_module(&module);
                // `use crate::auth::jwt;` imports a module, so the name is part of the path.
                if imp.name != "*" {
                    let joined = if module.is_empty() { imp.name.clone() } else { format!("{module}::{}", imp.name) };
                    targets.extend(index.files_for_module(&joined));
                }
                index.imported_files[fi].extend(targets.into_iter().filter(|&t| t != fi));
                if let Some(alias) = &imp.alias {
                    index.aliases[fi].insert(alias.as_str(), imp.name.as_str());
                }
            }
        }
        index
    }

    fn files_for_module(&self, module: &str) -> Vec<usize> {
        if module.is_empty() {
            return Vec::new();
        }
        let suffix = format!("::{module}");
        let mut out: Vec<usize> = self
            .module_keys
            .iter()
            .enumerate()
            .filter(|(_, k)| k.as_str() == module || k.ends_with(&suffix))
            .map(|(i, _)| i)
            .collect();
        out.sort_unstable();
        out.dedup();
        if let Some(exact) = self.modules.get(module) {
            out.retain(|i| exact.contains(i)); // prefer exact matches when they exist
            if out.is_empty() {
                out = exact.clone();
            }
        }
        out
    }

    /// Same Go package: both Go files in one directory.
    fn same_go_package(&self, a: usize, b: usize) -> bool {
        self.files[a].language == Language::Go
            && self.files[b].language == Language::Go
            && self.module_keys[a] == self.module_keys[b]
    }

    fn sym(&self, r: SymRef) -> &'a ParsedSymbol {
        &self.files[r.0].symbols[r.1]
    }

    fn candidates(&self, name: &str, kinds: &[&str]) -> Vec<SymRef> {
        self.by_name
            .get(name)
            .map(|v| v.iter().copied().filter(|&r| kinds.contains(&self.sym(r).kind)).collect())
            .unwrap_or_default()
    }
}

/// `dir/rel/name` with `.` and `..` collapsed: (`app/models`, `../lib`, `x`) -> `app/lib/x`.
fn join_relative(dir: &str, rel: &str, name: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in dir.split('/').chain(rel.split('/')).chain(std::iter::once(name)) {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// Maps Go import paths to the repo files of that package.
enum GoPackages<'a> {
    /// From `go.mod`: full import path -> files.
    Exact(HashMap<&'a str, Vec<usize>>),
    /// No `go.mod` known: match the import path's tail against package directories (`::dir`).
    Suffix(Vec<(usize, String, String)>),
}

impl<'a> GoPackages<'a> {
    fn build(files: &'a [ParsedFile], module_keys: &[String]) -> Self {
        let go = || files.iter().enumerate().filter(|(_, f)| f.language == Language::Go);
        if go().any(|(_, f)| f.package_path.is_some()) {
            let mut map: HashMap<&str, Vec<usize>> = HashMap::new();
            for (i, f) in go() {
                if let Some(p) = &f.package_path {
                    map.entry(p.as_str()).or_default().push(i);
                }
            }
            return Self::Exact(map);
        }
        Self::Suffix(
            go().filter(|(i, _)| !module_keys[*i].is_empty())
                .map(|(i, _)| (i, module_keys[i].clone(), format!("::{}", module_keys[i])))
                .collect(),
        )
    }

    fn resolve(&self, import_path: &str) -> Vec<usize> {
        match self {
            Self::Exact(map) => map.get(import_path).cloned().unwrap_or_default(),
            Self::Suffix(keys) => {
                let module = normalize_module(import_path);
                keys.iter()
                    .filter(|(_, k, sk)| module == *k || module.ends_with(sk.as_str()))
                    .map(|(i, _, _)| *i)
                    .collect()
            }
        }
    }
}

fn scope_of(s: &ParsedSymbol) -> &str {
    s.qualified_name.strip_suffix(s.name.as_str()).unwrap_or("").trim_end_matches(':')
}

/// Narrows candidates using the call's receiver text (`Self`, `self`, `Type::`, `module::`).
fn narrow_by_receiver(
    index: &Index,
    caller: &ParsedSymbol,
    caller_file: usize,
    receiver: &str,
    cands: Vec<SymRef>,
) -> Vec<SymRef> {
    // `auth.Verify()` / `str.ToUpper()` in Go: the receiver names an imported package, so only that
    // package's functions qualify; a package outside the repo (stdlib) yields nothing.
    if let Some(pkg) = index.go_imports[caller_file].get(receiver.trim()) {
        let in_pkg = cands.into_iter().filter(|r| pkg.contains(&r.0) && index.sym(*r).kind == "fn").collect();
        return go_in_build(index, caller_file, in_pkg);
    }
    let segment = receiver.rsplit("::").next().unwrap_or(receiver).trim();
    // `np.array(..)` after `import numpy as np`: the receiver is an alias for the module.
    let segment = index.aliases[caller_file].get(segment).copied().unwrap_or(segment);
    let narrowed: Vec<SymRef> = if is_self_receiver(caller, receiver) {
        // Go methods of one type can live in any file of the package.
        cands
            .iter()
            .copied()
            .filter(|&r| {
                (r.0 == caller_file || index.same_go_package(r.0, caller_file))
                    && scope_of(index.sym(r)) == scope_of(caller)
            })
            .collect()
    } else {
        cands
            .iter()
            .copied()
            .filter(|&r| {
                let s = index.sym(r);
                let scope_last = scope_of(s).rsplit("::").next().unwrap_or("");
                let module_last = index.module_keys[r.0].rsplit("::").next().unwrap_or("");
                scope_last == segment || module_last == segment
            })
            .collect()
    };
    if !narrowed.is_empty() {
        return narrowed;
    }
    // The receiver is a value of unknown type (or a std/global like `JSON`), so the call is a method
    // call: it cannot be a free function unless the receiver names that function's module.
    cands.into_iter().filter(|&r| index.sym(r).kind == "method").collect()
}

/// `self`/`this`/`Self`/`cls`, or the receiver variable of a Go method (`func (s *Server) Run()` -> `s`).
fn is_self_receiver(caller: &ParsedSymbol, receiver: &str) -> bool {
    let receiver = receiver.trim();
    if matches!(receiver, "self" | "Self" | "this" | "cls") {
        return true;
    }
    caller
        .signature
        .strip_prefix("func (")
        .and_then(|rest| rest.split_whitespace().next())
        .is_some_and(|name| name == receiver)
}

/// Whether a caller->caller edge through `receiver` can be real recursion: a call on the receiver
/// itself, or through the caller's own type (`Foo::build` in `impl Foo`, `Tree.walk` in `Tree`).
fn may_recurse(caller: &ParsedSymbol, receiver: &str) -> bool {
    let own_type = scope_of(caller).rsplit("::").next().unwrap_or("");
    let segment = receiver.trim().rsplit(['.', ':']).next().unwrap_or("");
    is_self_receiver(caller, receiver) || (!own_type.is_empty() && segment == own_type)
}

/// The class a symbol's body belongs to: a class symbol's own name (for class-body calls such as
/// `before_action :x`), else its enclosing scope.
fn own_scope(s: &ParsedSymbol) -> &str {
    if s.kind == "class" { s.qualified_name.as_str() } else { scope_of(s) }
}

fn pick_tier(index: &Index, caller_file: usize, cands: &[SymRef]) -> Option<(Vec<SymRef>, Tier)> {
    let tiers: [Vec<SymRef>; 3] = [
        cands.iter().copied().filter(|r| r.0 == caller_file).collect(),
        cands.iter().copied().filter(|r| index.imported_files[caller_file].contains(&r.0)).collect(),
        cands.to_vec(),
    ];
    let chosen = tiers.into_iter().find(|t| !t.is_empty())?;
    match chosen.len() {
        1 => Some((chosen, Tier::Exact)),
        n if n <= MAX_HEURISTIC_CANDIDATES => Some((chosen, Tier::Heuristic)),
        _ => None,
    }
}

struct EdgeAcc {
    edges: BTreeMap<(String, String, &'static str), Edge>,
}

impl EdgeAcc {
    fn add(&mut self, from: &str, to: &str, kind: &'static str, conf: &'static str, line: usize, snippet: &str) {
        let e = self.edges.entry((from.to_string(), to.to_string(), kind)).or_insert_with(|| Edge {
            from: from.to_string(),
            to: to.to_string(),
            kind,
            confidence: conf,
            lines: Vec::new(),
            snippet: snippet.to_string(),
        });
        // An exact resolution anywhere upgrades the edge.
        if conf == "STATIC_EXACT" {
            e.confidence = "STATIC_EXACT";
        }
        if line > 0 && !e.lines.contains(&line) {
            e.lines.push(line);
            e.lines.sort_unstable();
        }
    }
}

/// Tiered resolution shared by all languages: candidates by name and kind, narrowed by receiver,
/// then the nearest tier (same file, imported, repo).
fn resolve_generic(
    index: &Index,
    file: &ParsedFile,
    fi: usize,
    caller: &ParsedSymbol,
    call: &CallSite,
    name: &str,
) -> Option<(Vec<SymRef>, Tier)> {
    // A bare `f(..)` or bare identifier argument can only name a free function; methods need a
    // receiver or qualifier (`x.f()`, `Self::f`, `self.f`).
    let kinds: &[&str] = if call.receiver.is_none() { &["fn"] } else { CALLABLE };
    let mut cands = index.candidates(name, kinds);
    if file.language == Language::Go && call.receiver.is_none() {
        // An unqualified Go name is declared in the caller's own package or a dot-import.
        cands.retain(|r| index.same_go_package(r.0, fi) || index.go_dot_imports[fi].contains(&r.0));
        cands = go_in_build(index, fi, cands);
    }
    // A call can only target a definition that accepts its arguments.
    if let Some(args) = call.args {
        cands.retain(|r| index.sym(*r).arity.is_none_or(|a| a.accepts(args)));
    }
    if cands.is_empty() {
        return None;
    }
    // A Go receiver whose type the caller's code shows: resolve through the type model.
    if file.language == Language::Go
        && let (Some(r), Some(rt)) = (&call.receiver, &call.receiver_type)
        && !index.go_imports[fi].contains_key(r.trim())
        && let Some(typed) = go_typed_call(index, fi, rt, name)
    {
        return typed;
    }
    if let Some(r) = &call.receiver {
        cands = narrow_by_receiver(index, caller, fi, r, cands);
    }
    match call.kind {
        // Macro identifiers are only trusted when defined locally or imported (§10.3).
        CallKind::MacroIdent => {
            let near: Vec<SymRef> =
                cands.iter().copied().filter(|r| r.0 == fi || index.imported_files[fi].contains(&r.0)).collect();
            (!near.is_empty() && near.len() <= MAX_HEURISTIC_CANDIDATES).then_some((near, Tier::Heuristic))
        }
        _ => pick_tier(index, fi, &cands),
    }
}

/// Exact for one target, heuristic for a few, nothing for none or too many.
fn decide(targets: Vec<SymRef>, tier: Tier) -> Option<(Vec<SymRef>, Tier)> {
    match targets.len() {
        0 => None,
        1 => Some((targets, tier)),
        n if n <= MAX_HEURISTIC_CANDIDATES => Some((targets, Tier::Heuristic)),
        _ => None,
    }
}

/// Ruby method resolution through the class model. `Some(None)` means "resolved to nothing in the
/// repo" (a stdlib class, an inherited external method); `None` defers to the generic tiers, which
/// is what calls on values of unknown type get.
fn resolve_ruby(
    index: &Index,
    file: &ParsedFile,
    fi: usize,
    caller: &ParsedSymbol,
    call: &CallSite,
    name: &str,
) -> Option<Option<(Vec<SymRef>, Tier)>> {
    let rb = &index.ruby;
    let scope = own_scope(caller);
    // `Foo.x` and calls inside `def self.y` want class methods; calls on instances want instance
    // methods. `def self.x` and `def x` can coexist; when the wanted kind exists, keep only it.
    // `None`: no class in the ancestry defines it; `Some([])`: an accessor or alias (no symbol).
    let in_class = |class: &str, class_level: bool| -> Option<Vec<SymRef>> {
        let Some(mut found) = rb.lookup(class, name) else {
            return (name == "new").then(|| rb.lookup(class, "initialize").unwrap_or_default());
        };
        let singleton = |r: &SymRef| index.sym(*r).signature.starts_with("def self.");
        let wanted: Vec<SymRef> = found.iter().copied().filter(|r| singleton(r) == class_level).collect();
        if !wanted.is_empty() {
            found = wanted;
        }
        Some(found)
    };
    // a class-body DSL call takes symbols naming instance methods (`before_action :x`)
    let self_level = if caller.kind == "class" {
        call.kind != CallKind::SymbolRef
    } else {
        caller.signature.starts_with("def self.")
    };
    match call.receiver.as_deref().map(str::trim) {
        // A self call: the caller's class and its ancestors, then (for a module or a base class)
        // the classes that mix it in or extend it, then top-level methods, which are private
        // methods of Object. It never reaches an unrelated class.
        None | Some("self") => {
            if !scope.is_empty() {
                if let Some(own) = in_class(scope, self_level) {
                    return Some(decide(own, Tier::Exact));
                }
                let below = rb.descendant_methods(scope, name);
                if !below.is_empty() {
                    return Some(decide(below, Tier::Heuristic));
                }
            }
            let top: Vec<SymRef> = index
                .candidates(name, &["fn"])
                .into_iter()
                .filter(|r| index.files[r.0].language == Language::Ruby)
                .collect();
            Some(if top.is_empty() { None } else { pick_tier(index, fi, &top) })
        }
        // `Foo::Bar.build`: a class (or module) method, or `new` -> `initialize`. A constant that is
        // not a repo class (`File`, `JSON`) has no repo methods.
        Some(r) if is_const_path(r) => {
            Some(rb.resolve_const(r, scope).and_then(|c| decide(in_class(&c, true).unwrap_or_default(), Tier::Exact)))
        }
        Some(_) => {
            // A receiver whose class the method shows (`x = Foo.new`, `@x`, `self.class`).
            // (a parameter typed from its callers is closed-world evidence, like a name defined
            // once in the repo: one target is exact, several are heuristic)
            if let Some(ty) = &call.receiver_type
                && let Some(class) = ruby_receiver_class(index, scope, self_level, ty, 0)
            {
                return Some(class.and_then(|(c, level)| decide(in_class(&c, level).unwrap_or_default(), Tier::Exact)));
            }
            // A value of unknown type: any class that responds to the name, counting the methods
            // that have no `def` (accessors, Struct members). Too many responders is too generic
            // to link, and when an accessor could be the target no `def` is certain.
            let responders = rb.responders.get(name).map_or(0, HashSet::len);
            if responders > MAX_HEURISTIC_CANDIDATES || RUBY_CORE_METHODS.contains(&name) {
                return Some(None);
            }
            let resolved = resolve_generic(index, file, fi, caller, call, name);
            let defined = rb.defining_classes(name);
            Some(resolved.map(|(t, tier)| (t, if responders > defined { Tier::Heuristic } else { tier })))
        }
    }
}

/// The receiver's class and whether the receiver is the class object itself (class methods):
/// `Some(Some(..))` for a repo class, `Some(None)` for a class outside the repo, `None` unknown.
/// `self_level` is whether `self` is a class object (inside `def self.x`).
fn ruby_receiver_class(
    index: &Index,
    scope: &str,
    self_level: bool,
    ty: &TypeExpr,
    depth: u8,
) -> Option<Option<(String, bool)>> {
    if depth > 6 {
        return None;
    }
    let d = depth + 1;
    let rb = &index.ruby;
    match ty {
        TypeExpr::Named(t) if t == "self" => (!scope.is_empty()).then(|| Some((scope.to_string(), self_level))),
        TypeExpr::Named(t) if t == "self.class" => (!scope.is_empty()).then(|| Some((scope.to_string(), true))),
        TypeExpr::Named(t) => Some(rb.resolve_const(t, scope).map(|c| (c, false))),
        TypeExpr::Ident(t) => Some(rb.resolve_const(t, scope).map(|c| (c, true))),
        TypeExpr::Field(base, ivar) => {
            let (class, _) = ruby_receiver_class(index, scope, self_level, base, d)??;
            let (owner, t) = rb.ivar_type(&class, ivar)?;
            ruby_receiver_class(index, &owner, false, t, d)
        }
        TypeExpr::Call(callee) => {
            let TypeExpr::Field(base, m) = callee.as_ref() else { return None };
            let (class, level) = ruby_receiver_class(index, scope, self_level, base, d)??;
            match m.as_str() {
                "new" if level => return Some(Some((class, false))),
                "class" => return Some(Some((class, true))),
                _ => {}
            }
            let (owner, t) = rb.result_type(&class, m)?;
            ruby_receiver_class(index, &owner, level, t, d)
        }
        TypeExpr::Param(method, i) => {
            let hit = rb.param_types.borrow().get(&(scope.to_string(), method.clone(), *i)).cloned()?;
            Some(Some(hit))
        }
        TypeExpr::Elem(_) => None,
    }
}

/// Methods every Ruby value (Object, Kernel, Comparable) or the ubiquitous core collections and
/// scalars (Enumerable, Array, Hash, String, Integer, IO) define. On a receiver of unknown type the
/// runtime's own definitions are as likely as any repo one, so such a call is too generic to link.
const RUBY_CORE_METHODS: &[&str] = &[
    "new",
    "initialize",
    "class",
    "to_s",
    "inspect",
    "hash",
    "==",
    "!=",
    "===",
    "=~",
    "eql?",
    "equal?",
    "<=>",
    "<",
    ">",
    "<=",
    ">=",
    "nil?",
    "is_a?",
    "kind_of?",
    "instance_of?",
    "respond_to?",
    "send",
    "public_send",
    "dup",
    "clone",
    "freeze",
    "frozen?",
    "tap",
    "then",
    "object_id",
    "instance_variable_get",
    "instance_variable_set",
    "method",
    "methods",
    "extend",
    "display",
    "to_a",
    "to_h",
    "to_str",
    "to_sym",
    "to_i",
    "to_f",
    "to_proc",
    "each",
    "each_with_index",
    "each_with_object",
    "map",
    "flat_map",
    "select",
    "filter",
    "reject",
    "find",
    "detect",
    "find_all",
    "include?",
    "member?",
    "any?",
    "all?",
    "none?",
    "count",
    "first",
    "last",
    "min",
    "max",
    "sort",
    "sort_by",
    "group_by",
    "partition",
    "reduce",
    "inject",
    "sum",
    "zip",
    "take",
    "drop",
    "uniq",
    "compact",
    "flatten",
    "reverse",
    "empty?",
    "size",
    "length",
    "[]",
    "[]=",
    "<<",
    "push",
    "pop",
    "shift",
    "unshift",
    "concat",
    "delete",
    "clear",
    "fetch",
    "dig",
    "key?",
    "has_key?",
    "keys",
    "values",
    "merge",
    "merge!",
    "update",
    "join",
    "split",
    "strip",
    "chomp",
    "sub",
    "gsub",
    "start_with?",
    "end_with?",
    "match",
    "match?",
    "upcase",
    "downcase",
    "+",
    "-",
    "*",
    "/",
    "%",
    "read",
    "write",
    "close",
    "puts",
    "print",
    "call",
];

fn is_const_path(r: &str) -> bool {
    r.trim_start_matches("::").split("::").all(|seg| {
        seg.starts_with(|c: char| c.is_ascii_uppercase()) && seg.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// Go's type model: named types per package, struct fields and embedding, interface method sets,
/// first results of functions and methods, package-level variables, and method sets.
#[derive(Default)]
struct GoModel<'a> {
    /// per file: package id (Go files only)
    pkg: Vec<Option<usize>>,
    types: HashMap<(usize, &'a str), &'static str>,
    fields: HashMap<(usize, &'a str, &'a str), (&'a TypeExpr, usize)>,
    embeds: HashMap<(usize, &'a str), Vec<(&'a str, usize)>>,
    iface: HashMap<(usize, &'a str), Vec<&'a str>>,
    results: HashMap<(usize, &'a str, &'a str), (&'a str, usize)>,
    vars: HashMap<(usize, &'a str), (&'a TypeExpr, usize)>,
    /// (package, type, method) -> method symbols
    methods: HashMap<(usize, &'a str, &'a str), Vec<SymRef>>,
    /// (package, type) -> method names
    method_sets: HashMap<(usize, &'a str), HashSet<&'a str>>,
    /// (required method names joined, called method) -> implementing methods
    cha_cache: RefCell<HashMap<(String, String), Vec<SymRef>>>,
    /// per file: part of the default build (linux/amd64, cgo); false for other platforms' files
    default_build: Vec<bool>,
    /// per file: a `_test.go` file
    test_file: Vec<bool>,
}

/// A resolved Go type.
#[derive(Clone, Debug)]
enum GoType {
    /// A named type declared in the repo: (package, name, "struct" | "interface").
    Repo(usize, String, &'static str),
    /// A type from outside the repo (stdlib, third party, builtin), as written.
    External(String),
    /// A slice, array, map or channel: its element type as written, in the given file's context.
    Container(String, usize),
    /// An imported package (its repo files; empty for an external package).
    Package(Vec<usize>),
}

const GO_BUILTIN_TYPES: &[&str] = &[
    "bool",
    "byte",
    "rune",
    "string",
    "error",
    "any",
    "int",
    "int8",
    "int16",
    "int32",
    "int64",
    "uint",
    "uint8",
    "uint16",
    "uint32",
    "uint64",
    "uintptr",
    "float32",
    "float64",
    "complex64",
    "complex128",
];

/// Method sets of the standard-library interfaces that repo types commonly implement. A call on
/// one of these may dispatch to any repo type with all the methods.
const GO_STD_INTERFACES: &[(&str, &[&str])] = &[
    ("error", &["Error"]),
    ("fmt.Stringer", &["String"]),
    ("io.Reader", &["Read"]),
    ("io.Writer", &["Write"]),
    ("io.Closer", &["Close"]),
    ("io.ReadCloser", &["Read", "Close"]),
    ("io.WriteCloser", &["Write", "Close"]),
    ("io.ReadWriter", &["Read", "Write"]),
    ("io.ReadWriteCloser", &["Read", "Write", "Close"]),
    ("io.ReaderAt", &["ReadAt"]),
    ("io.WriterTo", &["WriteTo"]),
    ("io.ReaderFrom", &["ReadFrom"]),
    ("io.Seeker", &["Seek"]),
    ("io.StringWriter", &["WriteString"]),
    ("io.ByteReader", &["ReadByte"]),
    ("sort.Interface", &["Len", "Less", "Swap"]),
    ("heap.Interface", &["Len", "Less", "Swap", "Push", "Pop"]),
    ("http.Handler", &["ServeHTTP"]),
    ("http.RoundTripper", &["RoundTrip"]),
    ("http.ResponseWriter", &["Header", "Write", "WriteHeader"]),
    ("net.Conn", &["Read", "Write", "Close", "LocalAddr", "RemoteAddr"]),
    ("net.Listener", &["Accept", "Close", "Addr"]),
    ("net.Addr", &["Network", "String"]),
    ("net.Error", &["Error", "Timeout"]),
    ("fs.FS", &["Open"]),
    ("fs.File", &["Stat", "Read", "Close"]),
    ("fs.FileInfo", &["Name", "Size", "Mode", "ModTime", "IsDir", "Sys"]),
    ("os.FileInfo", &["Name", "Size", "Mode", "ModTime", "IsDir", "Sys"]),
    ("fs.DirEntry", &["Name", "IsDir", "Type", "Info"]),
    ("os.DirEntry", &["Name", "IsDir", "Type", "Info"]),
    ("context.Context", &["Deadline", "Done", "Err", "Value"]),
    ("flag.Value", &["String", "Set"]),
    ("hash.Hash", &["Write", "Sum", "Reset", "Size", "BlockSize"]),
    ("json.Marshaler", &["MarshalJSON"]),
    ("json.Unmarshaler", &["UnmarshalJSON"]),
    ("encoding.TextMarshaler", &["MarshalText"]),
    ("encoding.TextUnmarshaler", &["UnmarshalText"]),
    ("driver.Valuer", &["Value"]),
    ("sql.Scanner", &["Scan"]),
];

impl<'a> GoModel<'a> {
    fn build(files: &'a [ParsedFile], module_keys: &[String]) -> Self {
        let mut m = Self::default();
        let mut ids: HashMap<&str, usize> = HashMap::new();
        m.pkg = files
            .iter()
            .enumerate()
            .map(|(i, f)| {
                (f.language == Language::Go).then(|| {
                    let n = ids.len();
                    *ids.entry(module_keys[i].as_str()).or_insert(n)
                })
            })
            .collect();
        m.test_file = files.iter().map(|f| f.language == Language::Go && f.rel_path.ends_with("_test.go")).collect();
        m.default_build = files
            .iter()
            .map(|f| {
                f.language != Language::Go
                    || (go_filename_matches(&f.rel_path)
                        && f.facts.build_constraint.as_deref().is_none_or(go_constraint_holds))
            })
            .collect();
        for (fi, f) in files.iter().enumerate() {
            let Some(p) = m.pkg[fi] else { continue };
            for (si, s) in f.symbols.iter().enumerate() {
                match s.kind {
                    "struct" | "interface" => {
                        m.types.insert((p, s.name.as_str()), s.kind);
                    }
                    "method" => {
                        let owner = scope_of(s);
                        m.methods.entry((p, owner, s.name.as_str())).or_default().push((fi, si));
                        m.method_sets.entry((p, owner)).or_default().insert(s.name.as_str());
                    }
                    _ => {}
                }
            }
            let facts = &f.facts;
            for (owner, field, ty) in &facts.fields {
                m.fields.insert((p, owner.as_str(), field.as_str()), (ty, fi));
            }
            for (owner, ty) in &facts.embeds {
                m.embeds.entry((p, owner.as_str())).or_default().push((ty.as_str(), fi));
            }
            for (iface, method) in &facts.iface_methods {
                m.iface.entry((p, iface.as_str())).or_default().push(method.as_str());
            }
            for (owner, name, ty) in &facts.results {
                if let TypeExpr::Named(t) = ty {
                    m.results.insert((p, owner.as_str(), name.as_str()), (t.as_str(), fi));
                }
            }
            for (name, ty) in &facts.vars {
                m.vars.insert((p, name.as_str()), (ty, fi));
            }
        }
        m
    }
}

const GO_OS: &[&str] = &[
    "aix",
    "android",
    "darwin",
    "dragonfly",
    "freebsd",
    "hurd",
    "illumos",
    "ios",
    "js",
    "linux",
    "nacl",
    "netbsd",
    "openbsd",
    "plan9",
    "solaris",
    "wasip1",
    "windows",
    "zos",
];
const GO_ARCH: &[&str] = &[
    "386",
    "amd64",
    "amd64p32",
    "arm",
    "armbe",
    "arm64",
    "arm64be",
    "loong64",
    "mips",
    "mipsle",
    "mips64",
    "mips64le",
    "mips64p32",
    "mips64p32le",
    "ppc",
    "ppc64",
    "ppc64le",
    "riscv",
    "riscv64",
    "s390",
    "s390x",
    "sparc",
    "sparc64",
    "wasm",
];

/// `x_windows.go`, `x_arm64.go`, `x_darwin_arm64.go` only build on that platform.
fn go_filename_matches(rel_path: &str) -> bool {
    let name = rel_path.rsplit('/').next().unwrap_or(rel_path);
    let stem = name.strip_suffix(".go").unwrap_or(name);
    let stem = stem.strip_suffix("_test").unwrap_or(stem);
    let parts: Vec<&str> = stem.split('_').skip(1).collect();
    match parts.as_slice() {
        [.., os, arch] if GO_OS.contains(os) && GO_ARCH.contains(arch) => *os == "linux" && *arch == "amd64",
        [.., last] if GO_ARCH.contains(last) => *last == "amd64",
        [.., last] if GO_OS.contains(last) => *last == "linux",
        _ => true,
    }
}

/// Evaluates a `//go:build` expression for linux/amd64 with cgo and the gc toolchain.
fn go_constraint_holds(expr: &str) -> bool {
    fn tag(t: &str) -> bool {
        matches!(t, "linux" | "amd64" | "unix" | "cgo" | "gc") || t.starts_with("go1.")
    }
    // recursive descent over `||`, `&&`, `!`, parentheses
    fn or(tokens: &[String], i: &mut usize) -> bool {
        let mut v = and(tokens, i);
        while tokens.get(*i).is_some_and(|t| t == "||") {
            *i += 1;
            v |= and(tokens, i);
        }
        v
    }
    fn and(tokens: &[String], i: &mut usize) -> bool {
        let mut v = not(tokens, i);
        while tokens.get(*i).is_some_and(|t| t == "&&") {
            *i += 1;
            v &= not(tokens, i);
        }
        v
    }
    fn not(tokens: &[String], i: &mut usize) -> bool {
        match tokens.get(*i).map(String::as_str) {
            Some("!") => {
                *i += 1;
                !not(tokens, i)
            }
            Some("(") => {
                *i += 1;
                let v = or(tokens, i);
                *i += 1; // `)`
                v
            }
            Some(t) => {
                *i += 1;
                tag(t)
            }
            None => true,
        }
    }
    let mut tokens = Vec::new();
    let mut word = String::new();
    let mut chars = expr.chars().peekable();
    while let Some(c) = chars.next() {
        let op = match c {
            '(' | ')' | '!' => Some(c.to_string()),
            '&' | '|' if chars.peek() == Some(&c) => {
                chars.next();
                Some(format!("{c}{c}"))
            }
            c if c.is_whitespace() => None,
            c => {
                word.push(c);
                continue;
            }
        };
        if !word.is_empty() {
            tokens.push(std::mem::take(&mut word));
        }
        tokens.extend(op);
    }
    if !word.is_empty() {
        tokens.push(word);
    }
    or(&tokens, &mut 0)
}

/// The candidates that exist in the build the caller is part of. `_test.go` files are compiled
/// only into test binaries, so production code can never reach their declarations (test code can
/// reach everything). Among same-named definitions left (platform variants selected by build
/// constraints), keep those of the default build when it has any.
fn go_in_build(index: &Index, caller_file: usize, mut cands: Vec<SymRef>) -> Vec<SymRef> {
    if !index.go.test_file[caller_file] {
        cands.retain(|r| !index.go.test_file[r.0]);
    }
    if cands.len() < 2 {
        return cands;
    }
    let default: Vec<SymRef> = cands.iter().copied().filter(|r| index.go.default_build[r.0]).collect();
    if default.is_empty() { cands } else { default }
}

/// Resolves a call on a receiver of known type. `None`: the type is unknown, use the generic
/// tiers. `Some(None)`: the call provably targets no repo method (a stdlib type, a field holding
/// a func, a method promoted from an external embedded type).
fn go_typed_call(index: &Index, fi: usize, rt: &TypeExpr, method: &str) -> Option<Option<(Vec<SymRef>, Tier)>> {
    Some(match go_resolve(index, fi, rt, 0)? {
        GoType::Repo(p, t, "struct") => decide(go_in_build(index, fi, go_method(index, p, &t, method, 0)), Tier::Exact),
        GoType::Repo(p, t, _) => {
            let required = go_iface_methods(index, p, &t, 0);
            decide(go_in_build(index, fi, go_implementers(index, &required, method)), Tier::Heuristic)
        }
        GoType::External(t) => {
            let t = t.trim_start_matches('*');
            match GO_STD_INTERFACES.iter().find(|(name, _)| *name == t) {
                Some((_, required)) => {
                    let required: Vec<String> = required.iter().map(|s| s.to_string()).collect();
                    decide(go_in_build(index, fi, go_implementers(index, &required, method)), Tier::Heuristic)
                }
                None => None,
            }
        }
        GoType::Container(..) | GoType::Package(_) => None,
    })
}

/// A named type as written (`*pkg.T`, `[]T`, `map[K]V`), in the context of file `fi`.
fn go_resolve_text(index: &Index, fi: usize, text: &str) -> Option<GoType> {
    let t = text.trim().trim_start_matches('*').trim();
    for prefix in ["[]", "chan ", "<-chan ", "chan<- ", "..."] {
        if let Some(rest) = t.strip_prefix(prefix) {
            return Some(GoType::Container(rest.to_string(), fi));
        }
    }
    if t.starts_with('[') || t.starts_with("map[") {
        // `[N]T` / `map[K]V`: the element follows the bracket that closes the first one
        let open = t.find('[')?;
        let mut depth = 0;
        for (i, c) in t[open..].char_indices() {
            match c {
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(GoType::Container(t[open + i + 1..].to_string(), fi));
                    }
                }
                _ => {}
            }
        }
        return None;
    }
    // anonymous struct and func types have no methods; `interface{ M() }` is left unknown
    if t.starts_with("func") || t.starts_with("struct") || t == "interface{}" {
        return Some(GoType::External(t.to_string()));
    }
    if t.starts_with("interface") {
        return None;
    }
    let t = t.split('[').next().unwrap_or(t); // generic instantiation `List[T]`
    let p = index.go.pkg[fi]?;
    match t.split_once('.') {
        Some((q, name)) => {
            let files = index.go_imports[fi].get(q)?;
            let Some(&f0) = files.first() else {
                return Some(GoType::External(t.to_string()));
            };
            let qp = index.go.pkg[f0]?;
            let kind = index.go.types.get(&(qp, name))?;
            Some(GoType::Repo(qp, name.to_string(), kind))
        }
        None => {
            if let Some(kind) = index.go.types.get(&(p, t)) {
                return Some(GoType::Repo(p, t.to_string(), kind));
            }
            for &d in &index.go_dot_imports[fi] {
                if let Some(dp) = index.go.pkg[d]
                    && let Some(kind) = index.go.types.get(&(dp, t))
                {
                    return Some(GoType::Repo(dp, t.to_string(), kind));
                }
            }
            GO_BUILTIN_TYPES.contains(&t).then(|| GoType::External(t.to_string()))
        }
    }
}

fn go_resolve(index: &Index, fi: usize, expr: &TypeExpr, depth: u8) -> Option<GoType> {
    if depth > 12 {
        return None;
    }
    let d = depth + 1;
    let p = index.go.pkg[fi]?;
    match expr {
        TypeExpr::Named(t) => go_resolve_text(index, fi, t),
        TypeExpr::Ident(n) => {
            if let Some(files) = index.go_imports[fi].get(n.as_str()) {
                return Some(GoType::Package(files.clone()));
            }
            let &(ty, vf) = index.go.vars.get(&(p, n.as_str()))?;
            go_resolve(index, vf, ty, d)
        }
        TypeExpr::Field(base, f) => match go_resolve(index, fi, base, d)? {
            GoType::Package(files) => {
                let &f0 = files.first()?;
                let qp = index.go.pkg[f0]?;
                let &(ty, vf) = index.go.vars.get(&(qp, f.as_str()))?;
                go_resolve(index, vf, ty, d)
            }
            GoType::Repo(tp, t, "struct") => {
                let (ty, vf) = go_field(index, tp, &t, f, 0)?;
                go_resolve(index, vf, ty, d)
            }
            GoType::External(t) => Some(GoType::External(format!("{t}.{f}?"))),
            _ => None,
        },
        TypeExpr::Call(callee) => match callee.as_ref() {
            TypeExpr::Ident(f) => {
                // a conversion `T(x)` or a call of a package-level function
                if let Some(t) = go_resolve_text(index, fi, f) {
                    return Some(t);
                }
                let &(ty, rf) = index.go.results.get(&(p, "", f.as_str()))?;
                go_resolve_text(index, rf, ty)
            }
            TypeExpr::Field(base, m) => match go_resolve(index, fi, base, d)? {
                GoType::Package(files) => {
                    let Some(&f0) = files.first() else {
                        return Some(GoType::External(format!("{m}()?")));
                    };
                    let qp = index.go.pkg[f0]?;
                    if let Some(kind) = index.go.types.get(&(qp, m.as_str())) {
                        return Some(GoType::Repo(qp, m.to_string(), kind));
                    }
                    let &(ty, rf) = index.go.results.get(&(qp, "", m.as_str()))?;
                    go_resolve_text(index, rf, ty)
                }
                GoType::Repo(tp, t, _) => {
                    let (ty, rf) = go_result(index, tp, &t, m, 0)?;
                    go_resolve_text(index, rf, ty)
                }
                GoType::External(t) => Some(GoType::External(format!("{t}.{m}()?"))),
                GoType::Container(..) => None,
            },
            _ => None,
        },
        TypeExpr::Elem(inner) => match go_resolve(index, fi, inner, d)? {
            GoType::Container(t, cf) => go_resolve_text(index, cf, &t),
            _ => None,
        },
        TypeExpr::Param(..) => None, // Go parameters carry their declared types
    }
}

/// The embedded types of `t`, resolved.
fn go_embedded(index: &Index, p: usize, t: &str) -> Vec<GoType> {
    index.go.embeds.get(&(p, t)).into_iter().flatten().filter_map(|&(e, ef)| go_resolve_text(index, ef, e)).collect()
}

fn go_field<'a>(index: &Index<'a>, p: usize, t: &str, f: &str, depth: u8) -> Option<(&'a TypeExpr, usize)> {
    if let Some(&hit) = index.go.fields.get(&(p, t, f)) {
        return Some(hit);
    }
    if depth > 4 {
        return None;
    }
    go_embedded(index, p, t).into_iter().find_map(|e| match e {
        GoType::Repo(ep, et, _) => go_field(index, ep, &et, f, depth + 1),
        _ => None,
    })
}

fn go_result<'a>(index: &Index<'a>, p: usize, t: &str, m: &str, depth: u8) -> Option<(&'a str, usize)> {
    if let Some(&hit) = index.go.results.get(&(p, t, m)) {
        return Some(hit);
    }
    if depth > 4 {
        return None;
    }
    go_embedded(index, p, t).into_iter().find_map(|e| match e {
        GoType::Repo(ep, et, _) => go_result(index, ep, &et, m, depth + 1),
        _ => None,
    })
}

/// Methods named `m` on struct `t`, or promoted from its embedded repo types.
fn go_method(index: &Index, p: usize, t: &str, m: &str, depth: u8) -> Vec<SymRef> {
    if let Some(v) = index.go.methods.get(&(p, t, m)) {
        return v.clone();
    }
    if depth > 4 {
        return Vec::new();
    }
    go_embedded(index, p, t)
        .into_iter()
        .find_map(|e| match e {
            GoType::Repo(ep, et, "struct") => Some(go_method(index, ep, &et, m, depth + 1)).filter(|v| !v.is_empty()),
            GoType::Repo(ep, et, _) => {
                let required = go_iface_methods(index, ep, &et, 0);
                Some(go_implementers(index, &required, m)).filter(|v| !v.is_empty())
            }
            _ => None,
        })
        .unwrap_or_default()
}

/// The method names an interface requires, including those of embedded interfaces.
fn go_iface_methods(index: &Index, p: usize, t: &str, depth: u8) -> Vec<String> {
    let mut out: Vec<String> = index.go.iface.get(&(p, t)).into_iter().flatten().map(|s| s.to_string()).collect();
    if depth < 4 {
        for &(e, ef) in index.go.embeds.get(&(p, t)).into_iter().flatten() {
            match go_resolve_text(index, ef, e) {
                Some(GoType::Repo(ep, et, "interface")) => out.extend(go_iface_methods(index, ep, &et, depth + 1)),
                Some(GoType::External(x)) => {
                    if let Some((_, ms)) = GO_STD_INTERFACES.iter().find(|(n, _)| *n == x) {
                        out.extend(ms.iter().map(|s| s.to_string()));
                    }
                }
                _ => {}
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Class-hierarchy analysis: methods named `m` on repo types whose method set covers `required`.
fn go_implementers(index: &Index, required: &[String], m: &str) -> Vec<SymRef> {
    let key = (required.join(","), m.to_string());
    if let Some(hit) = index.go.cha_cache.borrow().get(&key) {
        return hit.clone();
    }
    let mut out: Vec<SymRef> = Vec::new();
    for (&(p, t), set) in &index.go.method_sets {
        if set.contains(m) && required.iter().all(|r| set.contains(r.as_str())) {
            out.extend(index.go.methods.get(&(p, t, m)).into_iter().flatten().copied());
        }
    }
    out.sort_unstable();
    index.go.cha_cache.borrow_mut().insert(key, out.clone());
    out
}

/// (class, method, positional index) of a Ruby parameter.
type ParamSlot = (String, String, usize);

/// Ruby classes and modules by qualified name (a class reopened in several files is one class),
/// their methods, and their ancestors from `<`, `include`, `extend` and `prepend`.
#[derive(Default)]
struct RubyModel<'a> {
    classes: HashSet<&'a str>,
    /// (class, @ivar) -> type assigned in that class; `None` when assignments disagree
    ivars: HashMap<(&'a str, &'a str), Option<&'a TypeExpr>>,
    /// (class, method) -> type of its last expression; `None` when definitions disagree
    results: HashMap<(&'a str, &'a str), Option<&'a TypeExpr>>,
    /// (class, method, positional index) -> the class every resolved caller passes there
    param_types: RefCell<HashMap<ParamSlot, (String, bool)>>,
    /// (class, method) for methods without a `def` (accessors, Struct members, aliases)
    implicit: HashSet<(&'a str, &'a str)>,
    /// method name -> every class that has it, with or without a `def`
    responders: HashMap<&'a str, HashSet<&'a str>>,
    methods: HashMap<(&'a str, &'a str), Vec<SymRef>>,
    ancestors: HashMap<String, Vec<String>>,
    descendants: HashMap<String, Vec<String>>,
}

impl<'a> RubyModel<'a> {
    fn build(files: &'a [ParsedFile]) -> Self {
        let mut m = Self::default();
        let ruby = || files.iter().enumerate().filter(|(_, f)| f.language == Language::Ruby);
        for (fi, f) in ruby() {
            for (si, s) in f.symbols.iter().enumerate() {
                match s.kind {
                    "class" => {
                        m.classes.insert(s.qualified_name.as_str());
                    }
                    "method" => m.methods.entry((scope_of(s), s.name.as_str())).or_default().push((fi, si)),
                    _ => {}
                }
            }
        }
        for (_, f) in ruby() {
            m.classes.extend(f.facts.classes.iter().map(String::as_str));
            for (class, method) in &f.facts.methods {
                m.implicit.insert((class.as_str(), method.as_str()));
            }
        }
        for &(class, method) in m.methods.keys().chain(m.implicit.iter()) {
            m.responders.entry(method).or_default().insert(class);
        }
        for (_, f) in ruby() {
            for (owner, ivar, ty) in &f.facts.fields {
                let slot = m.ivars.entry((owner.as_str(), ivar.as_str())).or_insert(Some(ty));
                if slot.is_some_and(|t| t != ty) {
                    *slot = None;
                }
            }
            for (owner, method, ty) in &f.facts.results {
                let slot = m.results.entry((owner.as_str(), method.as_str())).or_insert(Some(ty));
                if slot.is_some_and(|t| t != ty) {
                    *slot = None;
                }
            }
            for rel in &f.impls {
                // the innermost class/module named `type_name` around the relation's line
                let owner = f
                    .symbols
                    .iter()
                    .filter(|s| {
                        s.kind == "class"
                            && s.name == rel.type_name
                            && s.start_line <= rel.line
                            && rel.line <= s.end_line
                    })
                    .min_by_key(|s| s.end_line - s.start_line);
                let Some(owner) = owner else { continue };
                let Some(anc) = m.resolve_const(&rel.trait_name, &owner.qualified_name) else { continue };
                if anc == owner.qualified_name {
                    continue;
                }
                m.ancestors.entry(owner.qualified_name.clone()).or_default().push(anc.clone());
                m.descendants.entry(anc).or_default().push(owner.qualified_name.clone());
            }
        }
        m
    }

    /// Ruby constant lookup: the lexical nesting from innermost outwards, then a unique suffix match.
    fn resolve_const(&self, path: &str, nesting: &str) -> Option<String> {
        let path = path.trim().trim_start_matches("::");
        let mut scope = nesting;
        loop {
            let candidate = if scope.is_empty() { path.to_string() } else { format!("{scope}::{path}") };
            if self.classes.contains(candidate.as_str()) {
                return Some(candidate);
            }
            if scope.is_empty() {
                break;
            }
            scope = scope.rsplit_once("::").map_or("", |(outer, _)| outer);
        }
        let suffix = format!("::{path}");
        let mut hits = self.classes.iter().filter(|c| c.ends_with(&suffix));
        match (hits.next(), hits.next()) {
            (Some(one), None) => Some(one.to_string()),
            _ => None,
        }
    }

    /// The type assigned to `ivar` in `class` or the nearest ancestor that assigns it, with that
    /// class (the constant's lexical context).
    fn ivar_type(&self, class: &str, ivar: &str) -> Option<(String, &'a TypeExpr)> {
        self.walk(class, &self.ancestors, |c| self.ivars.get(&(c, ivar)).map(|t| t.map(|t| (c.to_string(), t))))
            .flatten()
    }

    /// The result type of `method` on `class` or the nearest ancestor defining it, with that class.
    fn result_type(&self, class: &str, method: &str) -> Option<(String, &'a TypeExpr)> {
        self.walk(class, &self.ancestors, |c| self.results.get(&(c, method)).map(|t| t.map(|t| (c.to_string(), t))))
            .flatten()
    }

    /// The methods named `name` on the nearest class in `class`'s ancestry (itself first).
    /// `None` when no class in the ancestry has the method; an empty list when the nearest one has
    /// it without a `def`.
    fn lookup(&self, class: &str, name: &str) -> Option<Vec<SymRef>> {
        self.walk(class, &self.ancestors, |c| match self.methods.get(&(c, name)) {
            Some(v) if !v.is_empty() => Some(v.clone()),
            _ => self.implicit.contains(&(c, name)).then(Vec::new),
        })
    }

    /// Classes with a `def` for `name`.
    fn defining_classes(&self, name: &str) -> usize {
        self.responders.get(name).map_or(0, |cs| cs.iter().filter(|c| self.methods.contains_key(&(**c, name))).count())
    }

    /// Methods named `name` in classes that inherit or mix in `class` (a module's host classes, a
    /// base class's overrides).
    fn descendant_methods(&self, class: &str, name: &str) -> Vec<SymRef> {
        let mut out = Vec::new();
        let mut seen: HashSet<&str> = HashSet::from([class]);
        let mut queue: Vec<&str> = vec![class];
        while let Some(c) = queue.pop() {
            for d in self.descendants.get(c).into_iter().flatten() {
                if seen.len() < 64 && seen.insert(d.as_str()) {
                    if let Some(v) = self.methods.get(&(d.as_str(), name)) {
                        out.extend(v.iter().copied());
                    }
                    queue.push(d.as_str());
                }
            }
        }
        out
    }

    fn walk<T>(
        &self,
        start: &str,
        edges: &HashMap<String, Vec<String>>,
        mut visit: impl FnMut(&str) -> Option<T>,
    ) -> Option<T> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut queue = std::collections::VecDeque::from([start.to_string()]);
        while let Some(c) = queue.pop_front() {
            if !seen.insert(c.clone()) || seen.len() > 64 {
                continue;
            }
            if let Some(hit) = visit(&c) {
                return Some(hit);
            }
            queue.extend(edges.get(&c).into_iter().flatten().cloned());
        }
        None
    }
}

fn resolve_call(
    index: &Index,
    fi: usize,
    file: &ParsedFile,
    caller: &ParsedSymbol,
    call: &CallSite,
) -> Option<(Vec<SymRef>, Tier)> {
    let name = index.aliases[fi].get(call.target_name.as_str()).copied().unwrap_or(call.target_name.as_str());
    match (file.language == Language::Ruby).then(|| resolve_ruby(index, file, fi, caller, call, name)) {
        Some(Some(r)) => r,
        _ => resolve_generic(index, file, fi, caller, call, name),
    }
}

/// Types Ruby positional parameters from the arguments of the calls that resolve exactly to their
/// method: a parameter gets a class only when every such call passes that same repo class. Runs
/// to a fixpoint (a typed parameter can type the arguments it is passed on as).
fn infer_ruby_params(index: &Index) {
    for _ in 0..4 {
        let mut seen: HashMap<ParamSlot, Option<(String, bool)>> = HashMap::new();
        for (fi, file) in index.files.iter().enumerate().filter(|(_, f)| f.language == Language::Ruby) {
            let by_id: HashMap<&str, &ParsedSymbol> = file.symbols.iter().map(|s| (s.symbol_id.as_str(), s)).collect();
            for call in file.calls.iter().filter(|c| !c.arg_types.is_empty()) {
                let Some(&caller) = by_id.get(call.from_symbol_id.as_str()) else { continue };
                let Some((targets, Tier::Exact)) = resolve_call(index, fi, file, caller, call) else { continue };
                let [t] = targets.as_slice() else { continue };
                let target = index.sym(*t);
                if target.kind != "method" && target.kind != "fn" {
                    continue;
                }
                let self_level = caller.signature.starts_with("def self.");
                for (i, arg) in call.arg_types.iter().enumerate() {
                    let class = arg
                        .as_ref()
                        .and_then(|a| ruby_receiver_class(index, own_scope(caller), self_level, a, 0))
                        .flatten();
                    let key = (scope_of(target).to_string(), target.name.clone(), i);
                    match seen.get_mut(&key) {
                        None => {
                            seen.insert(key, class);
                        }
                        Some(prev) if *prev != class => *prev = None,
                        Some(_) => {}
                    }
                }
            }
        }
        let typed: HashMap<_, _> = seen.into_iter().filter_map(|(k, v)| v.map(|v| (k, v))).collect();
        if *index.ruby.param_types.borrow() == typed {
            return;
        }
        *index.ruby.param_types.borrow_mut() = typed;
    }
}

fn resolve_all(index: &Index) -> (Vec<Edge>, HashSet<String>) {
    let mut acc = EdgeAcc { edges: BTreeMap::new() };
    let mut entrypoints = HashSet::new();
    infer_ruby_params(index);

    for (fi, file) in index.files.iter().enumerate() {
        let by_id: HashMap<&str, &ParsedSymbol> = file.symbols.iter().map(|s| (s.symbol_id.as_str(), s)).collect();

        for call in &file.calls {
            let Some(&caller) = by_id.get(call.from_symbol_id.as_str()) else {
                continue;
            };
            let Some((targets, mut tier)) = resolve_call(index, fi, file, caller, call) else {
                continue;
            };
            // `items.map(&:name)` / `other.send(:x)` name a method of an object of unknown type.
            if call.kind == CallKind::SymbolRef
                && call.receiver.as_deref().is_some_and(|r| !is_self_receiver(caller, r))
            {
                tier = Tier::Heuristic;
            }
            let conf = if tier == Tier::Exact { "STATIC_EXACT" } else { "STATIC_HEURISTIC" };
            // `fd.inner.Close()` inside `Close` delegates to another value that happens to share the
            // method name; only a call on the receiver itself can be real recursion.
            let delegating =
                call.kind == CallKind::SymbolRef || call.receiver.as_deref().is_some_and(|r| !may_recurse(caller, r));
            for t in targets {
                let target = index.sym(t);
                if delegating && target.symbol_id == caller.symbol_id {
                    continue;
                }
                acc.add(&caller.symbol_id, &target.symbol_id, "CALLS", conf, call.line, &call.snippet);
                if call.kind == CallKind::Callback && call.via.as_deref().is_some_and(|v| ROUTE_REGISTRARS.contains(&v))
                {
                    entrypoints.insert(target.symbol_id.clone());
                }
            }
        }

        for rel in &file.impls {
            let owners: Vec<SymRef> =
                index.candidates(&rel.type_name, TYPES).into_iter().filter(|r| r.0 == fi).collect();
            let last = rel.trait_name.rsplit(['.', ':']).next().unwrap_or(&rel.trait_name);
            let mut supers = index.candidates(last, TYPES);
            if file.language == Language::Ruby {
                let nesting = owners.first().map_or("", |o| index.sym(*o).qualified_name.as_str());
                let Some(full) = index.ruby.resolve_const(&rel.trait_name, nesting) else { continue };
                supers.retain(|r| index.sym(*r).qualified_name == full);
                supers.truncate(1); // a class reopened in several files is still one class
            }
            let Some((targets, tier)) = pick_tier(index, fi, &supers) else {
                continue;
            };
            let conf = if tier == Tier::Exact { "STATIC_EXACT" } else { "STATIC_HEURISTIC" };
            for o in &owners {
                for t in &targets {
                    let (from, to) = (&index.sym(*o).symbol_id, &index.sym(*t).symbol_id);
                    if from != to {
                        acc.add(
                            from,
                            to,
                            "IMPLEMENTS",
                            conf,
                            rel.line,
                            &format!("{} : {}", rel.type_name, rel.trait_name),
                        );
                    }
                }
            }
        }

        for tr in &file.type_refs {
            let Some(&owner) = by_id.get(tr.from_symbol_id.as_str()) else {
                continue;
            };
            let mut cands = index.candidates(&tr.type_name, TYPES);
            if file.language == Language::Go {
                // Go types are package-scoped: `T` is the caller's package (or a dot-import), `pkg.T`
                // that import's package, and nothing for a package outside the repo.
                match &tr.qualifier {
                    Some(q) => {
                        let pkg = index.go_imports[fi].get(q.as_str());
                        cands.retain(|r| pkg.is_some_and(|p| p.contains(&r.0)));
                    }
                    None => cands.retain(|r| index.same_go_package(r.0, fi) || index.go_dot_imports[fi].contains(&r.0)),
                }
            }
            let Some((targets, tier)) = pick_tier(index, fi, &cands) else {
                continue;
            };
            let conf = if tier == Tier::Exact { "STATIC_EXACT" } else { "STATIC_HEURISTIC" };
            let kind = match tr.kind {
                TypeRefKind::Param => "PASSES_TYPE",
                TypeRefKind::Return => "RETURNS_TYPE",
            };
            for t in targets {
                acc.add(&owner.symbol_id, &index.sym(t).symbol_id, kind, conf, 0, &tr.type_name);
            }
        }
    }
    (acc.edges.into_values().collect(), entrypoints)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingestion::parser::parse_file;
    use crate::ingestion::walker::{Language, SourceFile};

    fn parsed(files: &[(&str, &str)]) -> Vec<ParsedFile> {
        files
            .iter()
            .map(|(path, src)| {
                let f = SourceFile {
                    rel_path: path.to_string(),
                    language: Language::Rust,
                    source: src.to_string(),
                    blake3: String::new(),
                    line_count: 0,
                };
                parse_file(&f).unwrap().unwrap()
            })
            .collect()
    }

    fn id(files: &[ParsedFile], name: &str) -> String {
        files
            .iter()
            .flat_map(|f| &f.symbols)
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("no {name}"))
            .symbol_id
            .clone()
    }

    fn edge<'a>(edges: &'a [Edge], files: &[ParsedFile], from: &str, to: &str, kind: &str) -> Option<&'a Edge> {
        let (f, t) = (id(files, from), id(files, to));
        edges.iter().find(|e| e.from == f && e.to == t && e.kind == kind)
    }

    #[test]
    fn module_keys_and_normalization() {
        assert_eq!(module_key("src/auth/mod.rs"), "auth");
        assert_eq!(module_key("src/auth/jwt.rs"), "auth::jwt");
        assert_eq!(module_key("src/lib.rs"), "lib");
        assert_eq!(module_key("app/billing/stripe.py"), "app::billing::stripe");
        assert_eq!(normalize_module("crate::auth::jwt"), "auth::jwt");
        assert_eq!(normalize_module("super::x"), "x");
        assert_eq!(normalize_module("app.billing"), "app::billing");
        assert_eq!(module_key("internal/auth/jwt.go"), "internal::auth");
        assert_eq!(module_key("main.go"), "");
    }

    #[test]
    fn ruby_self_calls_and_symbol_refs_resolve_to_same_class_methods() {
        let mk = |path: &str, src: &str| {
            parse_file(&SourceFile {
                rel_path: path.into(),
                language: Language::Ruby,
                source: src.into(),
                blake3: String::new(),
                line_count: 0,
            })
            .unwrap()
            .unwrap()
        };
        let mut files = vec![
            mk(
                "app/a.rb",
                "class A < Base\n  before_action :check\n  def run\n    step(1)\n    send(:check)\n    self.step(2)\n    Util.helper\n  end\n  def step(n); end\n  def check; end\nend\n",
            ),
            mk("app/b.rb", "class B\n  def step(n); end\n  def check; end\nend\nclass Base; end\n"),
            mk("app/util.rb", "class Util\n  def self.helper; end\nend\n"),
        ];
        let edges = link(&mut files);
        let a = |name: &str| files[0].symbols.iter().find(|s| s.name == name).unwrap().symbol_id.clone();
        let calls =
            |from: &str, to: &str| edges.iter().any(|e| e.from == a(from) && e.to == a(to) && e.kind == "CALLS");
        assert!(calls("run", "step"), "{edges:#?}");
        assert!(calls("run", "check"), "send(:check) names a method of the same class");
        let b_step = files[1].symbols.iter().find(|s| s.name == "step").unwrap().symbol_id.clone();
        assert!(!edges.iter().any(|e| e.from == a("run") && e.to == b_step), "other class's step is not a candidate");
        assert!(edges.iter().any(|e| e.from == a("run") && e.to == id(&files, "helper")));
        let ext = edges.iter().find(|e| e.from == id(&files, "A") && e.to == id(&files, "Base")).unwrap();
        assert_eq!(ext.kind, "IMPLEMENTS");
    }

    #[test]
    fn delegation_to_a_same_named_method_on_another_value_is_not_recursion() {
        let mk = |path: &str, lang, src: &str| {
            parse_file(&SourceFile {
                rel_path: path.into(),
                language: lang,
                source: src.into(),
                blake3: String::new(),
                line_count: 0,
            })
            .unwrap()
            .unwrap()
        };
        let mut files = vec![mk(
            "a.go",
            Language::Go,
            "package p\ntype T struct{ inner I }\nfunc (t *T) Close() { t.inner.Close() }\nfunc (t *T) Walk(n int) { if n > 0 { t.Walk(n - 1) } }\n",
        )];
        let edges = link(&mut files);
        assert!(!edges.iter().any(|e| e.from == id(&files, "Close") && e.to == id(&files, "Close")));
        assert!(
            edges.iter().any(|e| e.from == id(&files, "Walk") && e.to == id(&files, "Walk")),
            "real recursion on the receiver stays"
        );
    }

    fn parse_as(path: &str, language: Language, src: &str) -> ParsedFile {
        parse_file(&SourceFile {
            rel_path: path.into(),
            language,
            source: src.into(),
            blake3: String::new(),
            line_count: 0,
        })
        .unwrap()
        .unwrap()
    }

    fn calls_between(edges: &[Edge], files: &[ParsedFile], from: (&str, &str), to: (&str, &str)) -> Option<Edge> {
        let sym = |(path, name): (&str, &str)| {
            files
                .iter()
                .find(|f| f.rel_path == path)
                .unwrap()
                .symbols
                .iter()
                .find(|s| s.name == name)
                .unwrap()
                .symbol_id
                .clone()
        };
        let (f, t) = (sym(from), sym(to));
        edges.iter().find(|e| e.from == f && e.to == t && e.kind == "CALLS").cloned()
    }

    #[test]
    fn go_bare_calls_stay_in_their_package() {
        let mut files = vec![
            parse_as(
                "internal/auth/a.go",
                Language::Go,
                "package auth\nimport \"example.com/app/internal/util\"\nfunc Run() { helper(); util.Other() }",
            ),
            parse_as("internal/auth/b.go", Language::Go, "package auth\nfunc helper() {}"),
            parse_as("internal/util/u.go", Language::Go, "package util\nfunc helper() {}\nfunc Other() {}"),
        ];
        let edges = link(&mut files);
        let sibling = calls_between(&edges, &files, ("internal/auth/a.go", "Run"), ("internal/auth/b.go", "helper"));
        assert_eq!(sibling.expect("same-package helper").confidence, "STATIC_EXACT");
        assert!(
            calls_between(&edges, &files, ("internal/auth/a.go", "Run"), ("internal/util/u.go", "helper")).is_none()
        );
        assert!(
            calls_between(&edges, &files, ("internal/auth/a.go", "Run"), ("internal/util/u.go", "Other")).is_some()
        );
    }

    #[test]
    fn go_stdlib_imports_never_link_to_same_named_repo_packages() {
        let mut files = vec![
            parse_as("cmd/m.go", Language::Go, "package main\nimport \"errors\"\nfunc main() { errors.New(\"x\") }"),
            parse_as("pkg/errors/e.go", Language::Go, "package errors\nfunc New(s string) error { return nil }"),
        ];
        let edges = link(&mut files);
        assert!(edges.iter().all(|e| e.kind != "CALLS"), "{edges:#?}");
    }

    #[test]
    fn go_receiver_variables_narrow_to_their_own_type_across_package_files() {
        let mut files = vec![
            parse_as("p/a.go", Language::Go, "package p\ntype A struct{}\nfunc (a *A) Run() { a.start() }"),
            parse_as("p/a_start.go", Language::Go, "package p\nfunc (a *A) start() {}"),
            parse_as("p/b.go", Language::Go, "package p\ntype B struct{}\nfunc (b *B) start() {}"),
        ];
        let edges = link(&mut files);
        let own = calls_between(&edges, &files, ("p/a.go", "Run"), ("p/a_start.go", "start")).expect("A.start");
        assert_eq!(own.confidence, "STATIC_EXACT");
        assert!(calls_between(&edges, &files, ("p/a.go", "Run"), ("p/b.go", "start")).is_none());
    }

    #[test]
    fn recursion_through_the_callers_own_type_is_kept() {
        let mut rs = vec![parse_as(
            "src/a.rs",
            Language::Rust,
            "struct Foo;\nimpl Foo { fn build(n: u32) -> Foo { if n == 0 { Foo } else { Foo::build(n - 1) } } }",
        )];
        let edges = link(&mut rs);
        assert!(calls_between(&edges, &rs, ("src/a.rs", "build"), ("src/a.rs", "build")).is_some(), "{edges:#?}");

        let mut py = vec![parse_as(
            "t.py",
            Language::Python,
            "class Tree:\n    @classmethod\n    def walk(cls, n):\n        cls.walk(n - 1)\n    @staticmethod\n    def visit(n):\n        Tree.visit(n - 1)\n",
        )];
        let edges = link(&mut py);
        assert!(calls_between(&edges, &py, ("t.py", "walk"), ("t.py", "walk")).is_some(), "{edges:#?}");
        assert!(calls_between(&edges, &py, ("t.py", "visit"), ("t.py", "visit")).is_some(), "{edges:#?}");
    }

    #[test]
    fn ruby_class_body_callbacks_resolve_within_their_own_class() {
        let mut files = vec![parse_as(
            "app/c.rb",
            Language::Ruby,
            "class User\n  before_action :check\n  def check; end\nend\nclass Admin\n  def check; end\nend\n",
        )];
        let edges = link(&mut files);
        let ids = |name: &str| {
            files[0].symbols.iter().filter(|s| s.name == name).map(|s| s.symbol_id.clone()).collect::<Vec<_>>()
        };
        let user = ids("User")[0].clone();
        let from_user: Vec<_> = edges.iter().filter(|e| e.from == user && e.kind == "CALLS").collect();
        assert_eq!(from_user.len(), 1, "{from_user:#?}");
        assert!(from_user[0].to.contains("::User::check#"), "{}", from_user[0].to);
        assert_eq!(from_user[0].confidence, "STATIC_EXACT");
    }

    #[test]
    fn ruby_symbol_refs_on_other_objects_are_not_exact_self_calls() {
        let mut files = vec![parse_as(
            "app/u.rb",
            Language::Ruby,
            "class User\n  def go(items, other)\n    items.map(&:name)\n    other.send(:check)\n    send(:audit)\n  end\n  def name; end\n  def check; end\n  def audit; end\nend\n",
        )];
        let edges = link(&mut files);
        let e = |to: &str| calls_between(&edges, &files, ("app/u.rb", "go"), ("app/u.rb", to));
        assert_eq!(e("name").map(|e| e.confidence), Some("STATIC_HEURISTIC"));
        assert_eq!(e("check").map(|e| e.confidence), Some("STATIC_HEURISTIC"));
        assert_eq!(e("audit").map(|e| e.confidence), Some("STATIC_EXACT"), "receiver-less send stays a self call");
    }

    #[test]
    fn ruby_require_relative_resolves_against_the_requiring_file() {
        let mut files = vec![
            parse_as("helper.rb", Language::Ruby, "def helper; end\n"),
            parse_as("app/helper.rb", Language::Ruby, "def helper; end\n"),
            parse_as("lib/util.rb", Language::Ruby, "def util; end\n"),
            parse_as("util.rb", Language::Ruby, "def util; end\n"),
            parse_as(
                "app/main.rb",
                Language::Ruby,
                "require_relative 'helper'\nrequire_relative '../lib/util'\ndef run\n  helper()\n  util()\nend\n",
            ),
        ];
        let edges = link(&mut files);
        let to = |path: &str, name: &str| calls_between(&edges, &files, ("app/main.rb", "run"), (path, name));
        assert_eq!(to("app/helper.rb", "helper").map(|e| e.confidence), Some("STATIC_EXACT"));
        assert!(to("helper.rb", "helper").is_none());
        assert_eq!(to("lib/util.rb", "util").map(|e| e.confidence), Some("STATIC_EXACT"));
        assert!(to("util.rb", "util").is_none());
        assert_eq!(join_relative("app/models", "../lib", "x"), "app/lib/x");
    }

    #[test]
    fn go_receivers_are_typed_from_params_locals_fields_and_results() {
        let mut files = vec![
            parse_as(
                "store/store.go",
                Language::Go,
                "package store\ntype Store struct{}\nfunc New() *Store { return &Store{} }\nfunc (s *Store) Save() {}\n",
            ),
            parse_as(
                "app/app.go",
                Language::Go,
                "package app\nimport (\n\t\"testing\"\n\t\"example.com/m/store\"\n)\ntype Server struct{ db *store.Store; name string }\ntype Other struct{}\nfunc (o *Other) Save() {}\nfunc (s *Server) Run(t *testing.T) {\n\ts.db.Save()\n\tx := store.New()\n\tx.Save()\n\tt.Errorf(\"x\")\n\tfor _, tt := range []struct{ name string }{{\"a\"}} {\n\t\tuse(tt.name)\n\t}\n}\nfunc use(string) {}\n",
            ),
        ];
        let edges = link(&mut files);
        let run = id(&files, "Run");
        let save_store = files[0].symbols.iter().find(|s| s.name == "Save").unwrap().symbol_id.clone();
        let save_other = files[1].symbols.iter().find(|s| s.name == "Save").unwrap().symbol_id.clone();
        let to_store: Vec<_> = edges.iter().filter(|e| e.from == run && e.to == save_store).collect();
        assert_eq!(to_store.len(), 1, "{edges:#?}");
        assert_eq!(to_store[0].confidence, "STATIC_EXACT");
        assert_eq!(to_store[0].lines.len(), 2, "field chain and local from a constructor");
        assert!(!edges.iter().any(|e| e.from == run && e.to == save_other), "typed receivers never guess");
        assert!(edges.iter().filter(|e| e.from == run).all(|e| e.confidence == "STATIC_EXACT"), "{edges:#?}");
    }

    #[test]
    fn go_interface_calls_reach_only_implementers() {
        let mut files = vec![parse_as(
            "p/p.go",
            Language::Go,
            "package p\ntype Saver interface{ Save(); Flush() }\ntype A struct{}\nfunc (a *A) Save() {}\nfunc (a *A) Flush() {}\ntype B struct{}\nfunc (b *B) Save() {}\nfunc Run(s Saver) { s.Save() }\n",
        )];
        let edges = link(&mut files);
        let calls: Vec<_> = edges.iter().filter(|e| e.from == id(&files, "Run") && e.kind == "CALLS").collect();
        assert_eq!(calls.len(), 1, "B lacks Flush, so it is not a Saver: {calls:#?}");
        assert!(calls[0].to.contains("::A::Save#"));
        assert_eq!(calls[0].confidence, "STATIC_HEURISTIC", "interface dispatch stays a guess");
    }

    #[test]
    fn go_platform_variants_resolve_to_the_default_build() {
        let mut files = vec![
            parse_as("p/run.go", Language::Go, "package p\nfunc Run() { lock() }\n"),
            parse_as("p/lock_linux.go", Language::Go, "package p\nfunc lock() {}\n"),
            parse_as("p/lock_windows.go", Language::Go, "package p\nfunc lock() {}\n"),
            parse_as("p/lock_other.go", Language::Go, "//go:build !linux && !windows\n\npackage p\nfunc lock() {}\n"),
        ];
        let edges = link(&mut files);
        let calls: Vec<_> = edges.iter().filter(|e| e.kind == "CALLS").collect();
        assert_eq!(calls.len(), 1, "{calls:#?}");
        assert!(calls[0].to.starts_with("p/lock_linux.go::"), "{}", calls[0].to);
        assert_eq!(calls[0].confidence, "STATIC_EXACT");
        assert!(go_constraint_holds("linux && (amd64 || arm64)"));
        assert!(!go_constraint_holds("darwin || windows"));
        assert!(go_constraint_holds("!windows && go1.21"));
    }

    #[test]
    fn go_type_refs_respect_package_scope() {
        let mut files = vec![
            parse_as("a/a.go", Language::Go, "package a\ntype Package struct{}\n"),
            parse_as("b/b.go", Language::Go, "package b\ntype Package struct{}\n"),
            parse_as(
                "c/c.go",
                Language::Go,
                "package c\nimport \"example.com/m/a\"\nimport \"go/build\"\nfunc F(p *a.Package, q *build.Package) {}\n",
            ),
        ];
        let edges = link(&mut files);
        let refs: Vec<_> = edges.iter().filter(|e| e.kind == "PASSES_TYPE").collect();
        assert_eq!(refs.len(), 1, "{refs:#?}");
        assert!(refs[0].to.starts_with("a/a.go::"));
        assert_eq!(refs[0].confidence, "STATIC_EXACT");
    }

    #[test]
    fn ruby_constants_ancestors_and_locals_resolve_through_the_class_model() {
        let mut files = vec![
            parse_as(
                "lib/base.rb",
                Language::Ruby,
                "module Greets\n  def greet; end\n  def host_hook; hook(); end\nend\nclass Base\n  def shared; end\nend\n",
            ),
            parse_as(
                "lib/shop.rb",
                Language::Ruby,
                "class Cart < Base\n  include Greets\n  def initialize(x); end\n  def self.build; end\n  def build; end\n  def hook; end\n  def total; end\n  def run\n    shared()\n    greet()\n    Cart.build()\n    c = Cart.new(1)\n    c.total()\n    File.read('x')\n  end\nend\nclass Other\n  def total; end\n  def shared; end\nend\n",
            ),
        ];
        let edges = link(&mut files);
        let from_run: Vec<_> = edges.iter().filter(|e| e.from == id(&files, "run") && e.kind == "CALLS").collect();
        let to = |q: &str| from_run.iter().find(|e| e.to.contains(q)).map(|e| e.confidence);
        assert_eq!(to("::Base::shared#"), Some("STATIC_EXACT"), "inherited: {from_run:#?}");
        assert_eq!(to("::Greets::greet#"), Some("STATIC_EXACT"), "mixed in");
        assert_eq!(to("::Cart::initialize#"), Some("STATIC_EXACT"), "Cart.new");
        assert_eq!(to("::Cart::total#"), Some("STATIC_EXACT"), "local typed by Cart.new");
        assert!(to("::Other::").is_none(), "an unrelated class is never a candidate: {from_run:#?}");
        let builds: Vec<_> = from_run.iter().filter(|e| e.to.contains("::Cart::build#")).collect();
        assert_eq!(builds.len(), 1, "Cart.build is the class method only");
        assert_eq!(builds[0].confidence, "STATIC_EXACT");
        assert!(from_run.iter().all(|e| !e.to.contains("read")), "File is not a repo class");
        // a module's self call reaches the classes that include it
        let hook = edges.iter().find(|e| e.from == id(&files, "host_hook")).expect("module -> host class");
        assert!(hook.to.contains("::Cart::hook#"));
        assert_eq!(hook.confidence, "STATIC_HEURISTIC");
    }

    #[test]
    fn ruby_unknown_receivers_count_implicit_methods_and_skip_core_names() {
        let mut files = vec![parse_as(
            "lib/m.rb",
            Language::Ruby,
            "class Spec\n  attr_reader :source\nend\nTuple = Struct.new(:source, :spec)\nclass Api\n  def source; end\n  def push(a, b); end\n  def each; end\nend\nclass Runner\n  def go(t, list)\n    t.source()\n    list.each()\n    list.push(1)\n    @items = []\n    @items.push(1, 2)\n  end\nend\n",
        )];
        let edges = link(&mut files);
        let from_go: Vec<_> = edges.iter().filter(|e| e.from == id(&files, "go")).collect();
        let to = |q: &str| from_go.iter().find(|e| e.to.contains(q)).map(|e| e.confidence);
        // Api#source is one of three classes with `source` (two without a def): not certain
        assert_eq!(to("::Api::source#"), Some("STATIC_HEURISTIC"), "{from_go:#?}");
        assert_eq!(to("::Api::each#"), None, "`each` on an unknown value is too generic");
        assert_eq!(to("::Api::push#"), None, "push(1) cannot bind push(a, b), and @items is an Array");
    }

    #[test]
    fn ruby_results_self_calls_and_caller_typed_params_resolve() {
        let mut files = vec![parse_as(
            "lib/n.rb",
            Language::Ruby,
            "class Conf
  def verbose; end
end
class Other
  def verbose; end
  def render(x); end
end
class Page
  def render(x); end
end
class App
  def self.config
    @config ||= Conf.new
  end
  def run
    App.config.verbose()
    show(Page.new)
  end
  def show(page)
    page.render(1)
  end
end
",
        )];
        let edges = link(&mut files);
        let from = |caller: &str| edges.iter().filter(|e| e.from == id(&files, caller)).collect::<Vec<_>>();
        let run = from("run");
        let verbose: Vec<_> = run.iter().filter(|e| e.to.contains("::verbose#")).collect();
        assert_eq!(verbose.len(), 1, "{run:#?}");
        assert!(verbose[0].to.contains("::Conf::verbose#"));
        assert_eq!(verbose[0].confidence, "STATIC_EXACT");
        let show = from("show");
        assert_eq!(show.len(), 1, "every caller passes a Page: {show:#?}");
        assert!(show[0].to.contains("::Page::render#"));
    }

    #[test]
    fn go_package_qualified_calls_resolve_through_import_paths_and_aliases() {
        let mk = |path: &str, src: &str| {
            parse_file(&SourceFile {
                rel_path: path.into(),
                language: Language::Go,
                source: src.into(),
                blake3: String::new(),
                line_count: 0,
            })
            .unwrap()
            .unwrap()
        };
        let mut files = vec![
            mk("internal/auth/verify.go", "package auth\nfunc Verify() {}\nfunc helper() {}\nfunc Run() { helper() }"),
            mk("internal/other/verify.go", "package other\nfunc Verify() {}"),
            mk(
                "cmd/main.go",
                "package main\nimport (\n\ta \"github.com/acme/app/internal/auth\"\n)\nfunc main() { a.Verify() }\nfunc plain() { Verify() }",
            ),
            mk(
                "cmd/other.go",
                "package main\nimport \"github.com/acme/app/internal/auth\"\nfunc two() { auth.Verify() }",
            ),
        ];
        let edges = link(&mut files);
        let verify_in = |dir: &str| {
            files
                .iter()
                .find(|f| f.rel_path.contains(dir))
                .unwrap()
                .symbols
                .iter()
                .find(|s| s.name == "Verify")
                .unwrap()
                .symbol_id
                .clone()
        };
        let from = |name: &str| id(&files, name);
        let to_auth = edges.iter().find(|e| e.from == from("main") && e.to == verify_in("auth")).expect("alias call");
        assert_eq!(to_auth.confidence, "STATIC_EXACT");
        assert!(!edges.iter().any(|e| e.from == from("main") && e.to == verify_in("other")));
        assert!(edges.iter().any(|e| e.from == from("two") && e.to == verify_in("auth")));
        // same-package call across files of one directory still works via the same-file tier
        assert!(edge(&edges, &files, "Run", "helper", "CALLS").is_some());
    }

    #[test]
    fn local_unique_call_is_exact_and_recursion_is_kept() {
        let mut files = parsed(&[("src/a.rs", "fn helper() {}\nfn caller() { helper(); caller(); }")]);
        let edges = link(&mut files);
        let e = edge(&edges, &files, "caller", "helper", "CALLS").unwrap();
        assert_eq!(e.confidence, "STATIC_EXACT");
        assert_eq!(e.lines, [2]);
        assert!(edge(&edges, &files, "caller", "caller", "CALLS").is_some());
    }

    #[test]
    fn import_beats_global_ambiguity() {
        let mut files = parsed(&[
            ("src/main.rs", "use crate::auth::check;\nfn run() { check(); }"),
            ("src/auth.rs", "pub fn check() {}"),
            ("src/other.rs", "pub fn check() {}"),
        ]);
        let edges = link(&mut files);
        let run = id(&files, "run");
        let to_auth: Vec<_> = edges.iter().filter(|e| e.from == run).collect();
        assert_eq!(to_auth.len(), 1);
        assert!(to_auth[0].to.starts_with("src/auth.rs"));
        assert_eq!(to_auth[0].confidence, "STATIC_EXACT");
    }

    #[test]
    fn ambiguous_methods_are_heuristic_and_generic_names_are_suppressed() {
        let mut src = String::from("fn run(s: S) { s.validate(); s.clone(); }\n");
        for i in 0..2 {
            src.push_str(&format!("struct T{i};\nimpl T{i} {{ fn validate(&self) {{}} }}\n"));
        }
        for i in 0..9 {
            src.push_str(&format!("struct C{i};\nimpl C{i} {{ fn clone(&self) {{}} }}\n"));
        }
        let mut files = parsed(&[("src/a.rs", &src)]);
        let edges = link(&mut files);
        let run = id(&files, "run");
        let from_run: Vec<_> = edges.iter().filter(|e| e.from == run).collect();
        assert_eq!(from_run.len(), 2, "{from_run:?}");
        assert!(from_run.iter().all(|e| e.confidence == "STATIC_HEURISTIC" && e.kind == "CALLS"));
    }

    #[test]
    fn self_receiver_resolves_within_the_same_impl() {
        let mut files = parsed(&[(
            "src/a.rs",
            "struct A; struct B;\nimpl A { fn go(&self) { self.step(); } fn step(&self) {} }\nimpl B { fn step(&self) {} }",
        )]);
        let edges = link(&mut files);
        let go = id(&files, "go");
        let out: Vec<_> = edges.iter().filter(|e| e.from == go).collect();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].confidence, "STATIC_EXACT");
        assert!(out[0].to.contains("::A::step#"), "{}", out[0].to);
    }

    #[test]
    fn bare_callback_identifiers_never_resolve_to_methods() {
        let mut files = parsed(&[(
            "src/a.rs",
            "struct S;\nimpl S { fn name(&self) {} fn go(&self) { self.map.get(name); self.map.get(Self::name); } }",
        )]);
        let edges = link(&mut files);
        let go = id(&files, "go");
        let out: Vec<_> = edges.iter().filter(|e| e.from == go).collect();
        assert_eq!(out.len(), 1, "only the qualified `Self::name` links: {out:?}");
        assert!(out[0].to.contains("::S::name#"), "{}", out[0].to);
    }

    #[test]
    fn callbacks_link_and_route_registration_marks_entrypoints() {
        let mut files = parsed(&[(
            "src/a.rs",
            "fn login_handler() {}\nfn transform(x: i32) -> i32 { x }\nfn setup() { app.route(\"/login\", get(login_handler)); v.iter().map(transform); }",
        )]);
        let edges = link(&mut files);
        assert!(edge(&edges, &files, "setup", "login_handler", "CALLS").is_some());
        assert!(edge(&edges, &files, "setup", "transform", "CALLS").is_some());
        let sym = |n: &str| files[0].symbols.iter().find(|s| s.name == n).unwrap();
        assert!(sym("login_handler").is_entrypoint && sym("login_handler").requires_strict_contract);
        assert!(!sym("transform").is_entrypoint);
    }

    #[test]
    fn macro_idents_link_only_locally_or_via_imports_and_always_heuristic() {
        let mut files = parsed(&[
            (
                "src/a.rs",
                "fn fetch() {}\nfn run() { tokio::select! { x = fetch() => {} } }\nfn far() { tokio::select! { x = remote() => {} } }",
            ),
            ("src/b.rs", "pub fn remote() {}"),
        ]);
        let edges = link(&mut files);
        let e = edge(&edges, &files, "run", "fetch", "CALLS").unwrap();
        assert_eq!(e.confidence, "STATIC_HEURISTIC");
        assert!(edge(&edges, &files, "far", "remote", "CALLS").is_none());
    }

    #[test]
    fn implements_edges_for_rust_python_and_typescript() {
        let rust =
            parsed(&[("src/a.rs", "trait Shape {}\nstruct Sq;\nimpl Shape for Sq {}\nimpl<T> Shape for Vec<T> {}")]);
        let mut files = rust;
        let edges = link(&mut files);
        let e = edge(&edges, &files, "Sq", "Shape", "IMPLEMENTS").expect("rust impl edge");
        assert_eq!((e.confidence, e.lines.as_slice()), ("STATIC_EXACT", &[3][..]));

        let mk = |path: &str, lang, src: &str| {
            parse_file(&SourceFile {
                rel_path: path.into(),
                language: lang,
                source: src.into(),
                blake3: String::new(),
                line_count: 0,
            })
            .unwrap()
            .unwrap()
        };
        let mut py = vec![mk(
            "m.py",
            Language::Python,
            "class Base: pass\nclass Child(Base, object): pass\nclass Other(pkg.Mixin): pass",
        )];
        let edges = link(&mut py);
        assert!(edge(&edges, &py, "Child", "Base", "IMPLEMENTS").is_some());
        assert_eq!(
            edges.iter().filter(|e| e.kind == "IMPLEMENTS").count(),
            1,
            "object and unknown Mixin are not repo types"
        );

        let mut ts = vec![mk(
            "a.ts",
            Language::TypeScript,
            "interface Repo {}\nclass Base {}\nclass Impl extends Base implements Repo {}\ninterface Sub extends Repo {}",
        )];
        let edges = link(&mut ts);
        assert!(edge(&edges, &ts, "Impl", "Base", "IMPLEMENTS").is_some(), "{edges:?}");
        assert!(edge(&edges, &ts, "Impl", "Repo", "IMPLEMENTS").is_some());
        assert!(edge(&edges, &ts, "Sub", "Repo", "IMPLEMENTS").is_some());
    }

    #[test]
    fn receiver_calls_never_resolve_to_free_functions() {
        let mk = |path: &str, lang, src: &str| {
            parse_file(&SourceFile {
                rel_path: path.into(),
                language: lang,
                source: src.into(),
                blake3: String::new(),
                line_count: 0,
            })
            .unwrap()
            .unwrap()
        };
        // `JSON.parse` inside the arrow function `parse` must not become a self-edge.
        let mut ts = vec![mk("a.ts", Language::TypeScript, "export const parse = (s: string) => JSON.parse(s);")];
        assert!(link(&mut ts).is_empty());

        // ...but a method call on an unknown receiver still finds the method.
        let mut rs = parsed(&[(
            "src/a.rs",
            "struct P;\nimpl P { fn parse(&self) {} }\nfn parse() {}\nfn run(x: P) { x.parse(); parse(); }",
        )]);
        let edges = link(&mut rs);
        let run = id(&rs, "run");
        let out: Vec<_> = edges.iter().filter(|e| e.from == run && e.kind == "CALLS").collect();
        assert_eq!(out.len(), 2, "{out:?}");
        // `x.parse()` -> the method, bare `parse()` -> the free function, each unambiguous
        assert!(out.iter().all(|e| e.confidence == "STATIC_EXACT"), "{out:?}");
        assert!(
            out.iter().any(|e| e.to.contains("::P::parse#method")) && out.iter().any(|e| e.to.contains("::::parse#fn"))
        );
    }

    #[test]
    fn signature_types_become_type_edges() {
        let mut files = parsed(&[("src/a.rs", "struct Req; struct Resp;\nfn handle(r: Req) -> Resp { Resp }")]);
        let edges = link(&mut files);
        assert!(edge(&edges, &files, "handle", "Req", "PASSES_TYPE").is_some());
        assert!(edge(&edges, &files, "handle", "Resp", "RETURNS_TYPE").is_some());
    }
}
