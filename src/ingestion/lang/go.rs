use std::sync::OnceLock;

use anyhow::Result;
use tree_sitter::Node;

use super::{Compiled, Frontend, text};
use crate::ingestion::model::ImportDecl;
use crate::ingestion::walker::SourceFile;

pub struct GoFrontend {
    compiled: OnceLock<Result<Compiled, String>>,
}

pub static GO: GoFrontend = GoFrontend { compiled: OnceLock::new() };

/// Single-parameter handler contexts of the common web frameworks.
const CONTEXT_TYPES: &[&str] = &["*gin.Context", "echo.Context", "*fiber.Ctx", "fiber.Ctx", "iris.Context"];

impl Frontend for GoFrontend {
    fn compiled(&self, _file: &SourceFile) -> Result<&Compiled> {
        self.compiled
            .get_or_init(|| {
                Compiled::build(
                    tree_sitter_go::LANGUAGE.into(),
                    include_str!("../../../queries/go/symbols.scm"),
                    include_str!("../../../queries/go/calls.scm"),
                    include_str!("../../../queries/go/risks.scm"),
                    "go",
                )
                .map_err(|e| format!("{e:#}"))
            })
            .as_ref()
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    fn symbol_kind(&self, def: Node, src: &[u8]) -> Option<&'static str> {
        Some(match def.kind() {
            "function_declaration" => "fn",
            "method_declaration" => "method",
            "type_spec" => match def.child_by_field_name("type").map(|t| t.kind()) {
                Some("interface_type") => "interface",
                _ => "struct",
            },
            "type_alias" => "struct",
            "const_spec" | "var_spec" if def.child_by_field_name("name").is_some_and(|n| text(n, src) == "_") => {
                return None;
            }
            "const_spec" => "const",
            "var_spec" => "global",
            _ => return None,
        })
    }

    fn scope_chain(&self, def: Node, src: &[u8]) -> Vec<String> {
        if def.kind() != "method_declaration" {
            return Vec::new();
        }
        receiver_type(def, src).into_iter().collect()
    }

    fn param_types(&self, def: Node, src: &[u8]) -> Vec<String> {
        let Some(params) = def.child_by_field_name("parameters") else {
            return Vec::new();
        };
        let mut cursor = params.walk();
        let mut out = Vec::new();
        for p in params.named_children(&mut cursor) {
            let ty = p.child_by_field_name("type").map_or("_".to_string(), |t| text(t, src).to_string());
            let ty = if p.kind() == "variadic_parameter_declaration" { format!("...{ty}") } else { ty };
            let names = p.children_by_field_name("name", &mut p.walk()).count().max(1);
            out.extend(std::iter::repeat_n(ty, names));
        }
        out
    }

    fn visibility(&self, def: Node, src: &[u8]) -> &'static str {
        let exported = def.child_by_field_name("name").is_some_and(|n| text(n, src).starts_with(char::is_uppercase));
        if exported { "pub" } else { "private" }
    }

    fn doc_comment(&self, def: Node, src: &[u8]) -> String {
        let mut lines: Vec<String> = leading_comments(def, src)
            .iter()
            .flat_map(|c| {
                let body = c.strip_prefix("//").map_or_else(
                    || c.trim_start_matches("/*").trim_end_matches("*/").to_string(),
                    |rest| rest.strip_prefix(' ').unwrap_or(rest).to_string(),
                );
                body.lines().map(|l| l.trim_end().to_string()).collect::<Vec<_>>()
            })
            .collect();
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        lines.join("\n")
    }

    fn is_entrypoint(&self, def: Node, src: &[u8]) -> bool {
        if !matches!(def.kind(), "function_declaration" | "method_declaration") {
            return false;
        }
        let name = def.child_by_field_name("name").map_or("", |n| text(n, src));
        if def.kind() == "function_declaration" {
            if name == "init" {
                return true;
            }
            if name == "main" && in_package_main(def, src) {
                return true;
            }
        }
        if leading_comments(def, src).iter().any(|c| c.starts_with("//export ")) {
            return true;
        }
        name.starts_with(char::is_uppercase) && has_handler_signature(def, src)
    }

    fn is_branch(&self, node: Node, src: &[u8]) -> bool {
        match node.kind() {
            "if_statement" | "for_statement" | "expression_case" | "type_case" | "communication_case" => true,
            "binary_expression" => {
                node.child_by_field_name("operator").is_some_and(|op| matches!(text(op, src), "&&" | "||"))
            }
            _ => false,
        }
    }

    fn is_nested_def(&self, node: Node) -> bool {
        matches!(node.kind(), "function_declaration" | "method_declaration")
    }

    fn flatten_import(&self, node: Node, src: &[u8], out: &mut Vec<ImportDecl>) {
        if node.kind() != "import_spec" {
            return;
        }
        let Some(path) = node.child_by_field_name("path") else {
            return;
        };
        let path = text(path, src).trim_matches(|c| c == '"' || c == '`').to_string();
        let last = path.rsplit('/').next().unwrap_or(&path).to_string();
        match node.child_by_field_name("name") {
            // `_` imports run init() only and bring no names into scope
            Some(n) if n.kind() == "blank_identifier" => {}
            Some(n) if n.kind() == "dot" => {
                out.push(ImportDecl { module_path: path, name: "*".to_string(), alias: None });
            }
            Some(n) => {
                out.push(ImportDecl { module_path: path, name: last, alias: Some(text(n, src).to_string()) });
            }
            None => out.push(ImportDecl { module_path: path, name: last, alias: None }),
        }
    }
}

/// Base type of a method receiver: `(s *Server)` -> `Server`, `(l List[T])` -> `List`.
fn receiver_type(def: Node, src: &[u8]) -> Option<String> {
    let recv = def.child_by_field_name("receiver")?;
    let ty = recv.named_child(0)?.child_by_field_name("type")?;
    let t = text(ty, src).trim_start_matches('*');
    Some(t.split('[').next().unwrap_or(t).to_string())
}

fn in_package_main(def: Node, src: &[u8]) -> bool {
    let mut root = def;
    while let Some(p) = root.parent() {
        root = p;
    }
    let mut cursor = root.walk();
    root.named_children(&mut cursor)
        .find(|n| n.kind() == "package_clause")
        .is_some_and(|n| text(n, src).split_whitespace().nth(1) == Some("main"))
}

/// `(http.ResponseWriter, *http.Request)` or a single framework context parameter.
fn has_handler_signature(def: Node, src: &[u8]) -> bool {
    let Some(params) = def.child_by_field_name("parameters") else {
        return false;
    };
    let mut cursor = params.walk();
    let types: Vec<&str> = params
        .named_children(&mut cursor)
        .filter_map(|p| p.child_by_field_name("type"))
        .map(|t| text(t, src))
        .collect();
    (types.contains(&"http.ResponseWriter") && types.contains(&"*http.Request"))
        || (types.len() == 1 && CONTEXT_TYPES.contains(&types[0]))
}

/// Raw text of the contiguous comment lines directly above `def`. For a spec in a declaration
/// (`type X ...`, `const ( ... )`) the comment may sit above the whole declaration.
fn leading_comments<'a>(def: Node, src: &'a [u8]) -> Vec<&'a str> {
    let direct = comments_above(def, src);
    if !direct.is_empty() {
        return direct;
    }
    let mut anchor = def;
    while let Some(p) = anchor.parent() {
        let first_spec = anchor.prev_named_sibling().is_none();
        match p.kind() {
            "var_spec_list" => anchor = p,
            "type_declaration" | "const_declaration" | "var_declaration" if first_spec => {
                return comments_above(p, src);
            }
            _ => break,
        }
    }
    Vec::new()
}

fn comments_above<'a>(node: Node, src: &'a [u8]) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut row = node.start_position().row;
    let mut prev = node.prev_sibling();
    while let Some(c) = prev.filter(|c| c.kind() == "comment" && c.end_position().row + 1 >= row) {
        out.push(text(c, src));
        row = c.start_position().row;
        prev = c.prev_sibling();
    }
    out.reverse();
    out
}

#[cfg(test)]
mod tests {
    use crate::ingestion::model::*;
    use crate::ingestion::parser::parse_file;
    use crate::ingestion::walker::{Language, SourceFile};

    fn parse(src: &str) -> ParsedFile {
        let f = SourceFile {
            rel_path: "svc/server.go".into(),
            language: Language::Go,
            blake3: String::new(),
            line_count: 0,
            source: src.into(),
        };
        parse_file(&f).expect("go queries must compile").unwrap()
    }

    fn sym<'a>(p: &'a ParsedFile, name: &str) -> &'a ParsedSymbol {
        p.symbols
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("no symbol {name}: {:?}", p.symbols.iter().map(|s| &s.name).collect::<Vec<_>>()))
    }

    #[test]
    fn symbols_kinds_ids_visibility_and_docs() {
        let p = parse(
            r#"package svc

// Server serves requests.
// It is safe for concurrent use.
type Server struct{ addr string }

type (
	// Handler handles.
	Handler interface{ Serve() }
	id      int
	Alias   = string
)

const Max = 3

const (
	// Min is the floor.
	Min = 1
	hidden = 2
)

var counter int

var (
	Registry = map[string]int{}
)

// Handle processes a request.
func (s *Server) Handle(a, b int, rest ...string) (int, error) {
	return 0, nil
}

func (s Server) close() {}

func helper(x int) {
	type local struct{}
	const localConst = 1
	var localVar int
}
"#,
        );
        let server = sym(&p, "Server");
        assert_eq!((server.kind, server.visibility), ("struct", "pub"));
        assert_eq!(server.doc_comment, "Server serves requests.\nIt is safe for concurrent use.");
        assert_eq!(
            (sym(&p, "Handler").kind, sym(&p, "Handler").doc_comment.as_str()),
            ("interface", "Handler handles.")
        );
        assert_eq!((sym(&p, "id").kind, sym(&p, "id").visibility), ("struct", "private"));
        assert_eq!(sym(&p, "Alias").kind, "struct");
        assert_eq!((sym(&p, "Max").kind, sym(&p, "Min").kind, sym(&p, "hidden").kind), ("const", "const", "const"));
        assert_eq!(sym(&p, "Min").doc_comment, "Min is the floor.");
        assert_eq!((sym(&p, "counter").kind, sym(&p, "Registry").kind), ("global", "global"));

        let handle = sym(&p, "Handle");
        assert_eq!((handle.kind, handle.visibility), ("method", "pub"));
        assert!(handle.symbol_id.starts_with("svc/server.go::Server::Handle#method:"), "{}", handle.symbol_id);
        assert_eq!(handle.qualified_name, "Server::Handle");
        assert_eq!(handle.doc_comment, "Handle processes a request.");
        assert_eq!(handle.signature, "func (s *Server) Handle(a, b int, rest ...string) (int, error)");
        assert_eq!(sym(&p, "close").qualified_name, "Server::close");
        assert_eq!(sym(&p, "close").visibility, "private");
        assert_eq!(sym(&p, "helper").kind, "fn");
        for local in ["local", "localConst", "localVar"] {
            assert!(!p.symbols.iter().any(|s| s.name == local), "{local} must not be a symbol");
        }
    }

    #[test]
    fn param_types_expand_grouped_names_and_variadics() {
        let a = parse("package s\nfunc f(a, b int, rest ...string) {}\n");
        let b = parse("package s\nfunc f(a int, b string, rest ...int) {}\n");
        let c = parse("package s\nfunc f(a, b int, rest ...string) {}\n// c\n");
        assert!(sym(&a, "f").symbol_id.contains("#fn:"));
        assert_eq!(sym(&a, "f").symbol_id, sym(&c, "f").symbol_id);
        assert_ne!(sym(&a, "f").symbol_id, sym(&b, "f").symbol_id);
    }

    #[test]
    fn complexity_counts_go_branches_and_inlines_closures() {
        let p = parse(
            r#"package s
func f(x, y int, ch chan int) int {
	if x > 0 && y > 0 {
		return 1
	} else if x < 0 || y < 0 {
		return 2
	}
	for i := 0; i < x; i++ {
	}
	switch x {
	case 1:
	case 2:
	default:
	}
	select {
	case <-ch:
	default:
	}
	func() { if y > 0 {} }()
	return 3
}
"#,
        );
        // if, &&, else-if, ||, for, 2 expression cases, 1 communication case, closure if
        assert_eq!(sym(&p, "f").cyclomatic_complexity, 10);
    }

    #[test]
    fn risk_flags_have_positive_and_negative_cases() {
        let p = parse(
            r#"package s
import ("os/exec"; "unsafe"; "fmt"; "os"; "sync"; "html/template"; "encoding/gob")
func a(c string) { exec.Command("sh", "-c", c).Run() }
func b(p *int) uintptr { return uintptr(unsafe.Pointer(p)) }
func c(p string) { os.ReadFile(p) }
func d(db *DB, n string) { db.Query("SELECT * FROM t WHERE n = '" + n + "'") }
func e(db *DB, n string) { db.Query(fmt.Sprintf("SELECT * FROM t WHERE n = '%s'", n)) }
func f(db *DB, n string) { db.Query("SELECT * FROM t WHERE n = ?", n) }
func g(ctx Ctx, db *DB, n string) { db.QueryContext(ctx, "SELECT " + n) }
func h() { go func() {}() }
func i() { var mu sync.Mutex; mu.Lock() }
func j() { _ = os.Remove("x") }
func k() { x, _ := os.Open("x"); _ = x }
func l() { defer func() { recover() }() }
func m(s string) template.HTML { return template.HTML(s) }
func n(r Reader) { gob.NewDecoder(r) }
func o(ch chan int) {}
func plain(a, b int) int { return a + b }
func sx(db *DB, n string) { var u U; db.Get(&u, "SELECT * FROM t WHERE n = '" + n + "'") }
func sxc(ctx Ctx, db *DB, n string) { var u []U; db.SelectContext(ctx, &u, fmt.Sprintf("SELECT %s", n)) }
func notsql(c *Ctx, rdb *R, ctx Ctx, id string) { c.Get("user" + id); rdb.Get(ctx, "k" + id) }
func fetch(base, path string) { http.Get(base + path) }
"#,
        );
        let flags = |n: &str| sym(&p, n).risk_flags.clone();
        assert_eq!(flags("a"), ["command_exec"]);
        assert!(flags("b").contains(&"unsafe_usage".to_string()) && flags("b").contains(&"memory_unsafe".to_string()));
        assert_eq!(flags("c"), ["external_io"]);
        for n in ["d", "e", "g"] {
            assert_eq!(flags(n), ["raw_sql"], "{n}");
        }
        assert!(flags("f").is_empty(), "parameterised query: {:?}", flags("f"));
        assert_eq!(flags("h"), ["concurrency_primitive"]);
        assert_eq!(flags("i"), ["concurrency_primitive"]);
        assert!(flags("j").contains(&"error_swallow".to_string()));
        assert_eq!(flags("k"), ["external_io"], "x, _ := f() is idiomatic");
        assert_eq!(flags("l"), ["error_swallow"]);
        assert_eq!(flags("m"), ["xss_sink"]);
        assert_eq!(flags("n"), ["unsafe_deserialization"]);
        assert_eq!(flags("o"), ["concurrency_primitive"]);
        assert!(flags("plain").is_empty());
        assert_eq!(flags("sx"), ["raw_sql"]);
        assert_eq!(flags("sxc"), ["raw_sql"]);
        assert!(flags("notsql").is_empty(), "{:?}", flags("notsql"));
        assert_eq!(flags("fetch"), ["external_io"], "http.Get is I/O, not SQL");
        assert!(sym(&p, "a").requires_strict_contract);
    }

    #[test]
    fn entrypoints() {
        let p = parse(
            r#"package main
func main() {}
func init() {}
func Serve(w http.ResponseWriter, r *http.Request) {}
func (s *S) Index(c *gin.Context) {}
func internalHandler(w http.ResponseWriter, r *http.Request) {}
func Helper(a int) {}

//export Callback
func Callback() {}
"#,
        );
        for n in ["main", "init", "Serve", "Index", "Callback"] {
            assert!(sym(&p, n).is_entrypoint, "{n}");
        }
        for n in ["internalHandler", "Helper"] {
            assert!(!sym(&p, n).is_entrypoint, "{n}");
        }
        let lib = parse("package lib\nfunc main() {}\n");
        assert!(!sym(&lib, "main").is_entrypoint, "main outside package main is just a function");
    }

    #[test]
    fn calls_callbacks_types_and_imports() {
        let p = parse(
            r#"package s
import (
	"fmt"
	str "strings"
	_ "net/http/pprof"
	. "os"
	"github.com/acme/app/auth"
)

func Handle(req *Request, n int) (*Response, error) {
	auth.Verify(req)
	req.Validate()
	fmt.Println(str.ToUpper("x"))
	check(n)
	http.HandleFunc("/x", worker)
	go worker()
	defer cleanup()
	return nil, nil
}
func worker() {}
func cleanup() {}
func check(int) {}
"#,
        );
        let handle = sym(&p, "Handle").symbol_id.clone();
        let find = |name: &str| p.calls.iter().find(|c| c.from_symbol_id == handle && c.target_name == name);
        assert_eq!(find("check").unwrap().kind, CallKind::Direct);
        assert_eq!(find("Verify").unwrap().receiver.as_deref(), Some("auth"));
        assert_eq!(find("Validate").unwrap().receiver.as_deref(), Some("req"));
        assert_eq!(find("ToUpper").unwrap().receiver.as_deref(), Some("str"));
        assert_eq!(find("cleanup").unwrap().kind, CallKind::Direct);
        assert!(p.calls.iter().any(|c| c.target_name == "worker" && c.kind == CallKind::Direct));
        let cb = p.calls.iter().find(|c| c.target_name == "worker" && c.kind == CallKind::Callback).unwrap();
        assert_eq!(cb.via.as_deref(), Some("HandleFunc"));

        assert!(p.type_refs.iter().any(|t| t.type_name == "Request" && t.kind == TypeRefKind::Param));
        assert!(p.type_refs.iter().any(|t| t.type_name == "Response" && t.kind == TypeRefKind::Return));

        let imp = |m: &str| p.imports.iter().find(|i| i.module_path == m).unwrap_or_else(|| panic!("no import {m}"));
        assert_eq!((imp("fmt").name.as_str(), imp("fmt").alias.clone()), ("fmt", None));
        assert_eq!((imp("strings").name.as_str(), imp("strings").alias.as_deref()), ("strings", Some("str")));
        assert!(!p.imports.iter().any(|i| i.module_path == "net/http/pprof"), "blank imports bind nothing");
        assert_eq!(imp("os").name, "*");
        assert_eq!(imp("github.com/acme/app/auth").name, "auth");
    }

    #[test]
    fn ast_hash_ignores_comments_and_formatting() {
        let a = parse("package s\nfunc f(x int) int {\n\treturn x + 1\n}\n");
        let b = parse("package s\n// note\nfunc f( x int ) int {\n\t// inner\n\treturn x +  1\n}\n");
        let c = parse("package s\nfunc f(x int) int {\n\treturn x + 2\n}\n");
        assert_eq!(sym(&a, "f").ast_blake3, sym(&b, "f").ast_blake3);
        assert_ne!(sym(&a, "f").ast_blake3, sym(&c, "f").ast_blake3);
    }
}
