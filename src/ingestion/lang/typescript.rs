//! TypeScript and JavaScript (`.ts` uses the TypeScript grammar; `.tsx/.js/.jsx/.mjs/.cjs`
//! use the TSX grammar, which also parses plain JavaScript and JSX).

use std::sync::OnceLock;

use anyhow::Result;
use tree_sitter::Node;

use super::{Compiled, Frontend, text};
use crate::ingestion::model::{ImplRel, ImportDecl};
use crate::ingestion::walker::SourceFile;

pub struct TypeScriptFrontend {
    ts: OnceLock<Result<Compiled, String>>,
    tsx: OnceLock<Result<Compiled, String>>,
}

pub static TYPESCRIPT: TypeScriptFrontend = TypeScriptFrontend { ts: OnceLock::new(), tsx: OnceLock::new() };

/// Decorator names that expose a method (NestJS, GraphQL, schedulers).
const ENTRYPOINT_DECORATORS: &[&str] = &[
    "Get",
    "Post",
    "Put",
    "Delete",
    "Patch",
    "Head",
    "Options",
    "All",
    "MessagePattern",
    "EventPattern",
    "Cron",
    "Command",
    "SubscribeMessage",
    "Query",
    "Mutation",
    "Subscription",
    "Process",
];

fn build(lang: tree_sitter::Language, name: &str, jsx: bool) -> Result<Compiled, String> {
    let risks = if jsx {
        concat!(
            include_str!("../../../queries/typescript/risks.scm"),
            "\n",
            include_str!("../../../queries/typescript/risks_jsx.scm")
        )
    } else {
        include_str!("../../../queries/typescript/risks.scm")
    };
    Compiled::build(
        lang,
        include_str!("../../../queries/typescript/symbols.scm"),
        include_str!("../../../queries/typescript/calls.scm"),
        risks,
        name,
    )
    .map_err(|e| format!("{e:#}"))
}

impl Frontend for TypeScriptFrontend {
    fn compiled(&self, file: &SourceFile) -> Result<&Compiled> {
        let cell = if file.rel_path.ends_with(".ts") {
            self.ts.get_or_init(|| build(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(), "typescript", false))
        } else {
            self.tsx.get_or_init(|| build(tree_sitter_typescript::LANGUAGE_TSX.into(), "tsx", true))
        };
        cell.as_ref().map_err(|e| anyhow::anyhow!("{e}"))
    }

    fn symbol_kind(&self, def: Node, _src: &[u8]) -> Option<&'static str> {
        Some(match def.kind() {
            "function_declaration" | "generator_function_declaration" => "fn",
            "class_declaration" | "abstract_class_declaration" => "class",
            "interface_declaration" => "interface",
            "enum_declaration" => "enum",
            "method_definition" | "public_field_definition" => "method",
            "lexical_declaration" | "variable_declaration" => "fn",
            _ => return None,
        })
    }

    fn function_node<'t>(&self, def: Node<'t>) -> Node<'t> {
        match def.kind() {
            "lexical_declaration" | "variable_declaration" => declared_function(def).unwrap_or(def),
            "public_field_definition" => def.child_by_field_name("value").unwrap_or(def),
            _ => def,
        }
    }

    fn scope_chain(&self, def: Node, src: &[u8]) -> Vec<String> {
        let mut chain = Vec::new();
        let mut cur = def.parent();
        while let Some(n) = cur {
            if matches!(
                n.kind(),
                "class_declaration"
                    | "abstract_class_declaration"
                    | "interface_declaration"
                    | "function_declaration"
                    | "internal_module"
                    | "method_definition"
            ) && let Some(name) = n.child_by_field_name("name")
            {
                chain.push(text(name, src).to_string());
            }
            cur = n.parent();
        }
        chain.reverse();
        chain
    }

    fn param_types(&self, def: Node, src: &[u8]) -> Vec<String> {
        let f = self.function_node(def);
        if let Some(single) = f.child_by_field_name("parameter") {
            return vec![text(single, src).to_string()];
        }
        let Some(params) = f.child_by_field_name("parameters") else {
            return Vec::new();
        };
        let mut cursor = params.walk();
        params
            .named_children(&mut cursor)
            .filter(|p| p.kind() != "comment")
            .map(|p| match p.child_by_field_name("type") {
                Some(t) => text(t, src).trim_start_matches(':').trim().to_string(),
                None => "_".to_string(),
            })
            .collect()
    }

    fn visibility(&self, def: Node, src: &[u8]) -> &'static str {
        if matches!(def.kind(), "method_definition" | "public_field_definition") {
            let mut cursor = def.walk();
            let modifier = def.children(&mut cursor).find(|c| c.kind() == "accessibility_modifier");
            let name = def.child_by_field_name("name").map_or("", |n| text(n, src));
            return match modifier.map(|m| text(m, src)) {
                Some("private") | Some("protected") => "private",
                _ if name.starts_with('#') => "private",
                _ => "pub",
            };
        }
        if def.parent().is_some_and(|p| p.kind() == "export_statement") { "export" } else { "private" }
    }

    fn doc_comment(&self, def: Node, src: &[u8]) -> String {
        let anchor = def.parent().filter(|p| p.kind() == "export_statement").unwrap_or(def);
        let mut prev = anchor.prev_sibling();
        let mut block = Vec::new();
        while let Some(n) = prev {
            if n.kind() != "comment" {
                break;
            }
            let t = text(n, src);
            if let Some(body) = t.strip_prefix("/**") {
                let body = body.trim_end_matches("*/");
                let cleaned: Vec<&str> =
                    body.lines().map(|l| l.trim().trim_start_matches('*').trim()).filter(|l| !l.is_empty()).collect();
                block.insert(0, cleaned.join("\n"));
                break;
            }
            prev = n.prev_sibling();
        }
        block.join("\n")
    }

    fn is_entrypoint(&self, def: Node, src: &[u8]) -> bool {
        let name = symbol_name(def, src);
        let exported = def.parent().is_some_and(|p| p.kind() == "export_statement");
        let top_level = exported || def.parent().is_some_and(|p| p.kind() == "program");
        if top_level && matches!(name, "main" | "handler") {
            return true;
        }
        // Next.js route handlers: `export async function GET(req)`
        if exported && matches!(name, "GET" | "POST" | "PUT" | "DELETE" | "PATCH" | "HEAD" | "OPTIONS") {
            return true;
        }
        decorators(def).into_iter().any(|d| {
            let t = text(d, src).trim_start_matches('@');
            ENTRYPOINT_DECORATORS.contains(&t.split('(').next().unwrap_or(t).trim())
        })
    }

    fn is_branch(&self, node: Node, src: &[u8]) -> bool {
        match node.kind() {
            "if_statement" | "for_statement" | "for_in_statement" | "while_statement" | "do_statement"
            | "catch_clause" | "switch_case" | "ternary_expression" => true,
            "binary_expression" => {
                node.child_by_field_name("operator").is_some_and(|op| matches!(text(op, src), "&&" | "||" | "??"))
            }
            _ => false,
        }
    }

    fn is_nested_def(&self, node: Node) -> bool {
        match node.kind() {
            "function_declaration"
            | "generator_function_declaration"
            | "class_declaration"
            | "abstract_class_declaration"
            | "method_definition" => true,
            "lexical_declaration" | "variable_declaration" => declared_function(node).is_some(),
            _ => false,
        }
    }

    fn implements(&self, root: Node, src: &[u8]) -> Vec<ImplRel> {
        let mut out = Vec::new();
        crate::ingestion::parser::walk(root, |n| {
            if matches!(n.kind(), "class_declaration" | "abstract_class_declaration" | "interface_declaration")
                && let Some(name) = n.child_by_field_name("name")
            {
                let mut cursor = n.walk();
                for heritage in
                    n.children(&mut cursor).filter(|c| matches!(c.kind(), "class_heritage" | "extends_type_clause"))
                {
                    let mut stack = vec![heritage];
                    while let Some(h) = stack.pop() {
                        let mut c2 = h.walk();
                        for part in h.named_children(&mut c2) {
                            match part.kind() {
                                "extends_clause" | "implements_clause" => stack.push(part),
                                "identifier"
                                | "type_identifier"
                                | "member_expression"
                                | "nested_type_identifier"
                                | "generic_type" => {
                                    let t = text(part, src);
                                    let base = t.split('<').next().unwrap_or(t).rsplit('.').next().unwrap_or("").trim();
                                    if !base.is_empty() {
                                        out.push(ImplRel {
                                            type_name: text(name, src).to_string(),
                                            trait_name: base.to_string(),
                                            line: n.start_position().row + 1,
                                        });
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
            true
        });
        out
    }

    fn flatten_import(&self, node: Node, src: &[u8], out: &mut Vec<ImportDecl>) {
        let Some(source) = node.child_by_field_name("source") else {
            return;
        };
        let module = text(source, src).trim_matches(|c| c == '"' || c == '\'' || c == '`').to_string();
        let mut cursor = node.walk();
        let Some(clause) = node.children(&mut cursor).find(|c| c.kind() == "import_clause") else {
            return;
        };
        let mut c2 = clause.walk();
        for part in clause.named_children(&mut c2) {
            match part.kind() {
                "identifier" => {
                    out.push(ImportDecl { module_path: module.clone(), name: text(part, src).to_string(), alias: None })
                }
                "namespace_import" => {
                    let alias = part.named_child(0).map(|n| text(n, src).to_string());
                    out.push(ImportDecl { module_path: module.clone(), name: "*".to_string(), alias });
                }
                "named_imports" => {
                    let mut c3 = part.walk();
                    for spec in part.named_children(&mut c3).filter(|s| s.kind() == "import_specifier") {
                        let name = spec.child_by_field_name("name").map_or(String::new(), |n| text(n, src).to_string());
                        let alias = spec.child_by_field_name("alias").map(|n| text(n, src).to_string());
                        out.push(ImportDecl { module_path: module.clone(), name, alias });
                    }
                }
                _ => {}
            }
        }
    }
}

/// Name of the symbol a definition node introduces (declarations carry it on their declarator).
fn symbol_name<'a>(def: Node, src: &'a [u8]) -> &'a str {
    if let Some(n) = def.child_by_field_name("name") {
        return text(n, src);
    }
    let mut cursor = def.walk();
    def.named_children(&mut cursor)
        .find(|c| c.kind() == "variable_declarator")
        .and_then(|d| d.child_by_field_name("name"))
        .map_or("", |n| text(n, src))
}

/// Decorators attached to `def`: children of the node, or immediately preceding siblings
/// (the TypeScript grammar uses both shapes depending on the construct).
fn decorators(def: Node) -> Vec<Node> {
    let mut out = Vec::new();
    let mut cursor = def.walk();
    out.extend(def.children(&mut cursor).filter(|c| c.kind() == "decorator"));
    let mut prev = def.prev_sibling();
    while let Some(n) = prev.filter(|n| n.kind() == "decorator") {
        out.push(n);
        prev = n.prev_sibling();
    }
    out
}

/// The arrow function or function expression a `const f = ...` declaration binds, if any.
pub(crate) fn declared_function(decl: Node) -> Option<Node> {
    let mut cursor = decl.walk();
    decl.named_children(&mut cursor).filter(|c| c.kind() == "variable_declarator").find_map(|d| {
        d.child_by_field_name("value").filter(|v| matches!(v.kind(), "arrow_function" | "function_expression"))
    })
}

#[cfg(test)]
mod tests {
    use crate::ingestion::model::*;
    use crate::ingestion::parser::parse_file;
    use crate::ingestion::walker::{Language, SourceFile};

    fn parse_as(path: &str, src: &str) -> ParsedFile {
        let f = SourceFile {
            rel_path: path.into(),
            language: Language::TypeScript,
            blake3: String::new(),
            line_count: 0,
            source: src.into(),
        };
        parse_file(&f).expect("typescript queries must compile").unwrap()
    }

    fn parse(src: &str) -> ParsedFile {
        parse_as("src/api/users.ts", src)
    }

    fn sym<'a>(p: &'a ParsedFile, name: &str) -> &'a ParsedSymbol {
        p.symbols
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("no symbol {name}: {:?}", p.symbols.iter().map(|s| &s.name).collect::<Vec<_>>()))
    }

    #[test]
    fn symbols_kinds_visibility_ids_and_jsdoc() {
        let p = parse(
            r#"
/**
 * Loads a user.
 * @param id user id
 */
export async function loadUser(id: string, opts?: Options): Promise<User> { return db.get(id); }

function internal(a: number): number { return a; }

export const handler = async (req: Request): Promise<Response> => { return respond(req); };
const double = (x: number) => x * 2;

export class UserService {
  private cache = new Map();
  constructor(private db: Db) {}
  public find(id: string): User | undefined { return this.cache.get(id); }
  private evict() {}
  save = async (u: User) => { await this.db.put(u); };
}

interface Repo { get(id: string): User }
enum Role { Admin, User }
"#,
        );
        let load = sym(&p, "loadUser");
        assert_eq!((load.kind, load.visibility), ("fn", "export"));
        assert!(load.symbol_id.starts_with("src/api/users.ts::::loadUser#fn:"), "{}", load.symbol_id);
        assert_eq!(load.doc_comment, "Loads a user.\n@param id user id");
        assert_eq!(load.signature, "async function loadUser(id: string, opts?: Options): Promise<User>");
        assert_eq!(sym(&p, "internal").visibility, "private");

        let h = sym(&p, "handler");
        assert_eq!((h.kind, h.visibility), ("fn", "export"));
        assert!(h.is_entrypoint);
        assert_eq!(h.signature, "const handler = async (req: Request): Promise<Response> =>");
        assert_eq!(sym(&p, "double").signature, "const double = (x: number) =>");

        assert_eq!(sym(&p, "UserService").kind, "class");
        let find = sym(&p, "find");
        assert_eq!((find.kind, find.visibility), ("method", "pub"));
        assert!(find.symbol_id.contains("::UserService::find#method:"), "{}", find.symbol_id);
        assert_eq!(sym(&p, "evict").visibility, "private");
        assert_eq!(sym(&p, "save").kind, "method");
        assert_eq!(sym(&p, "Repo").kind, "interface");
        assert_eq!(sym(&p, "Role").kind, "enum");
    }

    #[test]
    fn complexity_counts_js_branches_and_nested_declared_functions() {
        let p = parse(
            r#"
function f(a: number, b: number) {
  const inner = (z: number) => { if (z) { return 1; } return 0; };
  if (a && b) { return 1; } else if (a || b) { return 2; }
  for (const x of [1]) { while (b) { break; } }
  switch (a) { case 1: return 1; case 2: return 2; default: return 3; }
  try { risky(); } catch (e) { handle(e); }
  return a ?? b ? 1 : 2;
}
"#,
        );
        // if, &&, else-if, ||, for-of, while, case, case, catch, ??, ternary
        assert_eq!(sym(&p, "f").cyclomatic_complexity, 12);
        assert_eq!(sym(&p, "inner").cyclomatic_complexity, 2);
    }

    #[test]
    fn risk_flags_follow_the_spec() {
        let p = parse(
            r#"
function a(s: string) { return eval(s); }
function b(el: HTMLElement, v: string) { el.innerHTML = v; }
function c(s: string) { return JSON.parse(s); }
function d(cmd: string) { return execSync(cmd); }
function e(db: Db, n: string) { return db.query(`SELECT * FROM t WHERE n = '${n}'`); }
function f(db: Db, n: string) { return db.query("SELECT * FROM t WHERE n = $1", [n]); }
function g(p: string) { return fs.readFileSync(p); }
"#,
        );
        assert_eq!(sym(&p, "a").risk_flags, ["command_exec"]);
        assert_eq!(sym(&p, "b").risk_flags, ["xss_sink"]);
        assert_eq!(sym(&p, "c").risk_flags, ["unsafe_deserialization"]);
        assert_eq!(sym(&p, "d").risk_flags, ["command_exec"]);
        assert_eq!(sym(&p, "e").risk_flags, ["raw_sql"]);
        assert!(sym(&p, "f").risk_flags.is_empty(), "{:?}", sym(&p, "f").risk_flags);
        assert_eq!(sym(&p, "g").risk_flags, ["external_io"]);
    }

    #[test]
    fn jsx_in_tsx_and_js_files_parses_and_flags_xss() {
        let p = parse_as("web/view.tsx", "export const View = () => <div dangerouslySetInnerHTML={{ __html: x }} />;");
        assert_eq!(sym(&p, "View").risk_flags, ["xss_sink"]);
        let js = parse_as("web/legacy.js", "function legacy(a) { return a ? 1 : 2; }\nmodule.exports = { legacy };");
        assert_eq!(sym(&js, "legacy").cyclomatic_complexity, 2);
    }

    #[test]
    fn calls_callbacks_types_imports_and_nest_entrypoints() {
        let p = parse(
            r#"
import axios, { get as httpGet, post } from "./http/client";
import * as auth from "../auth/jwt";
import type { User } from "./types";

@Controller()
export class Ctl {
  @Get(":id")
  show(id: string): User { return auth.verify(id); }
}

export function setup(app: App) {
  app.get("/users", listUsers);
  app.use(mw.logger);
  helper(1);
  items.map(transform);
}
function listUsers(req: Req, res: Res) {}
function transform(x: number) { return x; }
function helper(n: number) {}
"#,
        );
        assert!(sym(&p, "show").is_entrypoint);
        assert!(!sym(&p, "helper").is_entrypoint);

        let setup = sym(&p, "setup").symbol_id.clone();
        let find = |from: &str, n: &str| p.calls.iter().find(|c| c.from_symbol_id == from && c.target_name == n);
        assert_eq!(find(&setup, "helper").unwrap().kind, CallKind::Direct);
        let cb = find(&setup, "listUsers").unwrap();
        assert_eq!((cb.kind.clone(), cb.via.as_deref()), (CallKind::Callback, Some("get")));
        assert_eq!(find(&setup, "logger").unwrap().kind, CallKind::Callback);
        assert_eq!(find(&setup, "transform").unwrap().via.as_deref(), Some("map"));
        let show = sym(&p, "show").symbol_id.clone();
        assert_eq!(find(&show, "verify").unwrap().receiver.as_deref(), Some("auth"));

        assert!(p.type_refs.iter().any(|t| t.type_name == "User" && t.kind == TypeRefKind::Return));
        assert!(p.type_refs.iter().any(|t| t.type_name == "App" && t.kind == TypeRefKind::Param));

        let imp = |m: &str, n: &str| p.imports.iter().find(|i| i.module_path == m && i.name == n);
        assert!(imp("./http/client", "axios").is_some());
        assert_eq!(imp("./http/client", "get").unwrap().alias.as_deref(), Some("httpGet"));
        assert!(imp("./http/client", "post").is_some());
        assert_eq!(imp("../auth/jwt", "*").unwrap().alias.as_deref(), Some("auth"));
    }

    #[test]
    fn ast_hash_ignores_comments_and_formatting() {
        let a = parse("function f(x: number) { return x + 1; }");
        let b = parse("// hi\nfunction f( x: number ) {\n  /* c */ return x  +  1;\n}");
        let c = parse("function f(x: number) { return x + 2; }");
        assert_eq!(sym(&a, "f").ast_blake3, sym(&b, "f").ast_blake3);
        assert_ne!(sym(&a, "f").ast_blake3, sym(&c, "f").ast_blake3);
    }
}
