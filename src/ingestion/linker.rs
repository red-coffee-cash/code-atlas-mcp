//! Two-pass static and heuristic symbol linker (spec §4.4, §10.2, §10.3).
//!
//! Pass 1 indexes every symbol and every file's imports. Pass 2 resolves each call site,
//! callback reference, macro identifier and signature type to edges, trying tiers in order
//! (same file, imported modules, whole repo) and stopping at the first tier with candidates.

use std::collections::{BTreeMap, HashMap, HashSet};

use super::model::{CallKind, ParsedFile, ParsedSymbol, TypeRefKind};
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
        };
        for (fi, f) in files.iter().enumerate() {
            for imp in &f.imports {
                let module = normalize_module(&imp.module_path);
                let mut targets = index.files_for_module(&module);
                // `use crate::auth::jwt;` imports a module, so the name is part of the path.
                if imp.name != "*" {
                    let joined = if module.is_empty() { imp.name.clone() } else { format!("{module}::{}", imp.name) };
                    targets.extend(index.files_for_module(&joined));
                }
                // Go imports are full module paths (`github.com/acme/app/auth`) that end in the
                // repo-relative package directory, the reverse of the other languages.
                if f.language == Language::Go {
                    targets.extend(index.files_for_import_path(&module));
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

    fn files_for_import_path(&self, module: &str) -> Vec<usize> {
        let suffix_of = |key: &str| !key.is_empty() && (module == key || module.ends_with(&format!("::{key}")));
        self.module_keys
            .iter()
            .enumerate()
            .filter(|(i, k)| suffix_of(k) && self.files[*i].language == Language::Go)
            .map(|(i, _)| i)
            .collect()
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
    let segment = receiver.rsplit("::").next().unwrap_or(receiver).trim();
    // `str.ToUpper(..)` after `import str "strings"`: the receiver is an alias for the package.
    let segment = index.aliases[caller_file].get(segment).copied().unwrap_or(segment);
    let narrowed: Vec<SymRef> = if matches!(segment, "self" | "Self" | "this") {
        cands.iter().copied().filter(|&r| r.0 == caller_file && scope_of(index.sym(r)) == scope_of(caller)).collect()
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

fn resolve_all(index: &Index) -> (Vec<Edge>, HashSet<String>) {
    let mut acc = EdgeAcc { edges: BTreeMap::new() };
    let mut entrypoints = HashSet::new();

    for (fi, file) in index.files.iter().enumerate() {
        let by_id: HashMap<&str, &ParsedSymbol> = file.symbols.iter().map(|s| (s.symbol_id.as_str(), s)).collect();

        for call in &file.calls {
            let Some(&caller) = by_id.get(call.from_symbol_id.as_str()) else {
                continue;
            };
            let name = index.aliases[fi].get(call.target_name.as_str()).copied().unwrap_or(call.target_name.as_str());
            // A bare `f(..)` or bare identifier argument can only name a free function; methods need a
            // receiver or qualifier (`x.f()`, `Self::f`, `self.f`).
            let kinds: &[&str] = if call.receiver.is_none() { &["fn"] } else { CALLABLE };
            let mut cands = index.candidates(name, kinds);
            if cands.is_empty() {
                continue;
            }
            if let Some(r) = &call.receiver {
                cands = narrow_by_receiver(index, caller, fi, r, cands);
            }

            let resolved = match call.kind {
                // Macro identifiers are only trusted when defined locally or imported (§10.3).
                CallKind::MacroIdent => {
                    let near: Vec<SymRef> = cands
                        .iter()
                        .copied()
                        .filter(|r| r.0 == fi || index.imported_files[fi].contains(&r.0))
                        .collect();
                    (!near.is_empty() && near.len() <= MAX_HEURISTIC_CANDIDATES).then_some((near, Tier::Heuristic))
                }
                _ => pick_tier(index, fi, &cands),
            };
            let Some((targets, tier)) = resolved else {
                continue;
            };
            let conf = if tier == Tier::Exact { "STATIC_EXACT" } else { "STATIC_HEURISTIC" };
            for t in targets {
                let target = index.sym(t);
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
            let supers = index.candidates(&rel.trait_name, TYPES);
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
            let cands = index.candidates(&tr.type_name, TYPES);
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
