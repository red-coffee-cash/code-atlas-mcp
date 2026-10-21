use std::sync::OnceLock;

use anyhow::Result;
use tree_sitter::Node;

use super::{Compiled, Frontend, own_line, text};
use crate::ingestion::model::{ImportDecl, TypeExpr, TypeFacts};
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

    fn receiver_type(&self, receiver: Node, src: &[u8]) -> Option<TypeExpr> {
        expr_type(receiver, src, 0)
    }

    fn call_arg_types(&self, site: Node, src: &[u8]) -> Vec<Option<TypeExpr>> {
        let Some(args) = site.child_by_field_name("arguments") else { return Vec::new() };
        let mut out = Vec::new();
        for a in named(args) {
            if a.kind() == "variadic_argument" {
                out.push(None);
                break;
            }
            out.push(expr_type(a, src, 0));
        }
        out
    }

    fn type_facts(&self, root: Node, src: &[u8]) -> TypeFacts {
        let mut facts = TypeFacts::default();
        let mut cursor = root.walk();
        for decl in root.named_children(&mut cursor) {
            match decl.kind() {
                "comment" if facts.build_constraint.is_none() => {
                    facts.build_constraint =
                        text(decl, src).strip_prefix("//go:build").map(|expr| expr.trim().to_string());
                }
                "type_declaration" => {
                    let mut c = decl.walk();
                    for spec in decl.named_children(&mut c).filter(|n| n.kind() == "type_spec") {
                        let (Some(name), Some(ty)) =
                            (spec.child_by_field_name("name"), spec.child_by_field_name("type"))
                        else {
                            continue;
                        };
                        record_type(text(name, src), ty, src, &mut facts);
                    }
                }
                "function_declaration" | "method_declaration" => {
                    let owner = receiver_type(decl, src).unwrap_or_default();
                    if let Some(name) = decl.child_by_field_name("name") {
                        let mut i = 0;
                        for p in decl.child_by_field_name("parameters").map(named).unwrap_or_default() {
                            let ty = p.child_by_field_name("type").map_or(String::new(), |t| text(t, src).to_string());
                            let n = p.children_by_field_name("name", &mut p.walk()).count().max(1);
                            for _ in 0..n {
                                facts.params.push((owner.clone(), text(name, src).to_string(), i, ty.clone()));
                                i += 1;
                            }
                        }
                    }
                    if let (Some(name), Some(_), Some(body)) = (
                        decl.child_by_field_name("name"),
                        decl.child_by_field_name("result"),
                        decl.child_by_field_name("body"),
                    ) {
                        let name = text(name, src).to_string();
                        for value in returned_values(body, src) {
                            facts.returns.push((owner.clone(), name.clone(), value));
                        }
                    }
                    let (Some(name), Some(result)) =
                        (decl.child_by_field_name("name"), decl.child_by_field_name("result"))
                    else {
                        continue;
                    };
                    record_results(&owner, text(name, src), result, src, &mut facts);
                }
                "var_declaration" => {
                    let mut c = decl.walk();
                    let specs: Vec<Node> = decl
                        .named_children(&mut c)
                        .flat_map(|n| if n.kind() == "var_spec_list" { named(n) } else { vec![n] })
                        .filter(|n| n.kind() == "var_spec")
                        .collect();
                    for spec in specs {
                        let names: Vec<Node> = spec.children_by_field_name("name", &mut spec.walk()).collect();
                        let values = spec.child_by_field_name("value").map(named).unwrap_or_default();
                        for (i, n) in names.iter().enumerate() {
                            let ty = match spec.child_by_field_name("type") {
                                Some(t) => Some(TypeExpr::Named(text(t, src).to_string())),
                                None if values.len() == names.len() => expr_type(values[i], src, 0),
                                None => None,
                            };
                            if let Some(ty) = ty {
                                facts.vars.push((text(*n, src).to_string(), ty));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        record_field_flows(root, src, &mut facts);
        // types declared inside functions: not symbols, but part of the package's type model
        crate::ingestion::parser::walk(root, |n| {
            if n.kind() == "type_spec"
                && n.parent().and_then(|d| d.parent()).is_some_and(|p| p.kind() != "source_file")
                && let (Some(name), Some(ty)) = (n.child_by_field_name("name"), n.child_by_field_name("type"))
            {
                let kind = if ty.kind() == "interface_type" { "interface" } else { "struct" };
                facts.local_types.push((text(name, src).to_string(), kind));
                record_type(text(name, src), ty, src, &mut facts);
            }
            true
        });
        facts
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

fn named(n: Node) -> Vec<Node> {
    let mut c = n.walk();
    n.named_children(&mut c).filter(|x| x.kind() != "comment").collect()
}

/// Each result's type: `f` for the first, `f#1`, `f#2`, ... for the rest.
fn record_results(owner: &str, name: &str, result: Node, src: &[u8], facts: &mut TypeFacts) {
    let types: Vec<String> = if result.kind() == "parameter_list" {
        named(result)
            .into_iter()
            .filter_map(|p| {
                let t = text(p.child_by_field_name("type")?, src).to_string();
                Some(std::iter::repeat_n(t, p.children_by_field_name("name", &mut p.walk()).count().max(1)))
            })
            .flatten()
            .collect()
    } else {
        vec![text(result, src).to_string()]
    };
    for (i, t) in types.into_iter().enumerate() {
        let key = if i == 0 { name.to_string() } else { format!("{name}#{i}") };
        facts.results.push((owner.to_string(), key, TypeExpr::Named(t)));
    }
}

fn record_type(name: &str, ty: Node, src: &[u8], facts: &mut TypeFacts) {
    match ty.kind() {
        "struct_type" => {
            let Some(list) = named(ty).into_iter().find(|n| n.kind() == "field_declaration_list") else {
                return;
            };
            for field in named(list).into_iter().filter(|n| n.kind() == "field_declaration") {
                let Some(ft) = field.child_by_field_name("type") else { continue };
                let names: Vec<Node> = field.children_by_field_name("name", &mut field.walk()).collect();
                if names.is_empty() {
                    // an embedded field is also a field named after its type (`s.Conn` for `Conn`)
                    let ty = text(ft, src);
                    facts.embeds.push((name.to_string(), ty.to_string()));
                    let field = ty.trim_start_matches('*').rsplit('.').next().unwrap_or(ty);
                    let field = field.split('[').next().unwrap_or(field);
                    facts.fields.push((name.to_string(), field.to_string(), TypeExpr::Named(ty.to_string())));
                }
                for n in names {
                    facts.fields.push((
                        name.to_string(),
                        text(n, src).to_string(),
                        TypeExpr::Named(text(ft, src).to_string()),
                    ));
                }
            }
        }
        "interface_type" => {
            for m in named(ty) {
                match m.kind() {
                    "method_elem" => {
                        if let Some(n) = m.child_by_field_name("name") {
                            facts.iface_methods.push((name.to_string(), text(n, src).to_string()));
                            if let Some(r) = m.child_by_field_name("result") {
                                record_results(name, text(n, src), r, src, facts);
                            }
                        }
                    }
                    "type_elem" => facts.embeds.push((name.to_string(), text(m, src).to_string())),
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// Every first value a function body returns (nested func literals excluded), locals expanded to
/// all values assigned to them. `None` entries are values of unknown type.
fn returned_values(body: Node, src: &[u8]) -> Vec<Option<TypeExpr>> {
    let mut out = Vec::new();
    crate::ingestion::parser::walk(body, |n| {
        if n.kind() == "func_literal" {
            return false;
        }
        if n.kind() == "return_statement" {
            match n.named_child(0).and_then(|l| named(l).into_iter().next()) {
                // a bare return yields the named results' current values
                None => {
                    let func = body.parent();
                    let first = func
                        .and_then(|f| f.child_by_field_name("result"))
                        .filter(|r| r.kind() == "parameter_list")
                        .and_then(|r| r.named_child(0))
                        .and_then(|p| p.child_by_field_name("name"));
                    match (func, first) {
                        (Some(f), Some(name)) => match assigned_values(f, text(name, src), src) {
                            Some(vs) => out.extend(vs.into_iter().map(Some)),
                            None => out.push(None),
                        },
                        _ => out.push(None),
                    }
                }
                Some(v) => match value_flows(v, src) {
                    Some(vs) => out.extend(vs.into_iter().map(Some)),
                    None => out.push(None),
                },
            }
        }
        true
    });
    out
}

/// The typed values an expression can hold: `nil` none, a local everything assigned to it in its
/// function, anything else its own type. `None` when any of them is untyped.
fn value_flows(expr: Node, src: &[u8]) -> Option<Vec<TypeExpr>> {
    if expr.kind() == "nil" || (expr.kind() == "identifier" && text(expr, src) == "nil") {
        return Some(Vec::new());
    }
    if expr.kind() == "call_expression"
        && let Some(f) = expr.child_by_field_name("function").filter(|f| f.kind() == "identifier")
        && let Some(lit) = local_func_literal(f, src)
    {
        let body = lit.child_by_field_name("body")?;
        return returned_values(body, src).into_iter().collect();
    }
    if expr.kind() != "identifier" {
        return expr_type(expr, src, 0).map(|t| vec![t]);
    }
    let name = text(expr, src);
    if matches!(binding(expr, src, 0), Binding::Unbound) {
        return Some(vec![TypeExpr::Ident(name.to_string())]);
    }
    let mut func = expr.parent();
    while let Some(f) =
        func.filter(|f| !matches!(f.kind(), "function_declaration" | "method_declaration" | "func_literal"))
    {
        func = f.parent();
    }
    assigned_values(func?, name, src)
}

/// The func literal a local is bound to (`listen := func(..) T { .. }`), if it is only ever that.
fn local_func_literal<'t>(ident: Node<'t>, src: &[u8]) -> Option<Node<'t>> {
    let name = text(ident, src);
    let mut func = ident.parent();
    while let Some(f) = func.filter(|f| !matches!(f.kind(), "function_declaration" | "method_declaration")) {
        func = f.parent();
    }
    let mut found = None;
    let mut other = false;
    crate::ingestion::parser::walk(func?, |n| {
        if matches!(n.kind(), "short_var_declaration" | "assignment_statement")
            && let (Some(l), Some(r)) = (n.child_by_field_name("left"), n.child_by_field_name("right"))
        {
            let (l, r) = (named(l), named(r));
            if let Some(i) = l.iter().position(|x| text(*x, src) == name) {
                match r.get(i).filter(|v| v.kind() == "func_literal" && l.len() == r.len()) {
                    Some(lit) if found.is_none() => found = Some(*lit),
                    _ => other = true,
                }
            }
        }
        true
    });
    if other { None } else { found }
}

/// Everything assigned to local `name` in `func` (named results included: they start as zero
/// values). `None` for parameters, whose values come from callers.
fn assigned_values(func: Node, name: &str, src: &[u8]) -> Option<Vec<TypeExpr>> {
    if matches!(func.kind(), "function_declaration" | "method_declaration")
        && let Some(i) = param_index(func, name, src)
    {
        let owner = receiver_type(func, src).unwrap_or_default();
        let f = func.child_by_field_name("name").map_or("", |n| text(n, src));
        return Some(vec![TypeExpr::Param(format!("{owner}.{f}"), i)]);
    }
    let is_param = ["receiver", "parameters"]
        .into_iter()
        .filter_map(|f| func.child_by_field_name(f))
        .flat_map(named)
        .any(|p| p.children_by_field_name("name", &mut p.walk()).any(|n| text(n, src) == name));
    if is_param {
        return None;
    }
    let mut values: Vec<Node> = Vec::new();
    let mut unknown = false;
    crate::ingestion::parser::walk(func, |n| {
        let (left, right) = match n.kind() {
            "short_var_declaration" | "assignment_statement" => {
                if n.kind() == "assignment_statement"
                    && n.child_by_field_name("operator").is_some_and(|o| text(o, src) != "=")
                {
                    return true;
                }
                (n.child_by_field_name("left").map(named), n.child_by_field_name("right").map(named))
            }
            "var_spec" => (
                Some(n.children_by_field_name("name", &mut n.walk()).collect()),
                n.child_by_field_name("value").map(named),
            ),
            "range_clause" => {
                if n.child_by_field_name("left").is_some_and(|l| named(l).iter().any(|x| text(*x, src) == name)) {
                    unknown = true;
                }
                return true;
            }
            _ => return true,
        };
        let (Some(left), right) = (left, right.unwrap_or_default()) else { return true };
        if let Some(i) = left.iter().position(|l| l.kind() == "identifier" && text(*l, src) == name) {
            match (right.len() == left.len(), i, right.first()) {
                (true, _, _) => values.push(right[i]),
                (false, 0, Some(r)) => values.push(*r),
                (false, _, None) => {} // `var x T`: the zero value
                _ => unknown = true,
            }
        }
        true
    });
    if unknown {
        return None;
    }
    let mut out = Vec::new();
    for v in values {
        if v.kind() == "nil" || text(v, src) == "nil" {
            continue;
        }
        out.push(expr_type(v, src, 0)?);
    }
    Some(out)
}

/// Values stored into struct fields: keyed composite literals (`&S{L: ln}`) and `x.f = v`.
/// Positional literals and stores into receivers of unknown type poison the field.
fn record_field_flows(root: Node, src: &[u8], facts: &mut TypeFacts) {
    crate::ingestion::parser::walk(root, |n| {
        match n.kind() {
            "composite_literal" => {
                let ty = literal_type(n, src).map(TypeExpr::Named);
                let elems = n.child_by_field_name("body").map(named).unwrap_or_default();
                for e in &elems {
                    if e.kind() == "keyed_element" {
                        let key = e.child_by_field_name("key").and_then(|k| k.named_child(0));
                        let value = e.child_by_field_name("value").and_then(|v| v.named_child(0));
                        let (Some(key), Some(value)) = (key, value) else { continue };
                        if key.kind() != "identifier" && key.kind() != "field_identifier" {
                            continue;
                        }
                        let base = ty.clone().unwrap_or_else(|| TypeExpr::Ident("?".to_string()));
                        record_flow(facts, base, text(key, src), value_flows(value, src));
                    }
                }
                let positional = elems.iter().any(|e| e.kind() == "literal_element");
                if positional && let Some(t) = &ty {
                    facts.field_flows.push((t.clone(), "*".to_string(), None));
                }
            }
            "assignment_statement" => {
                let left = n.child_by_field_name("left").map(named).unwrap_or_default();
                let right = n.child_by_field_name("right").map(named).unwrap_or_default();
                for (i, l) in left.iter().enumerate().filter(|(_, l)| l.kind() == "selector_expression") {
                    let (Some(operand), Some(field)) =
                        (l.child_by_field_name("operand"), l.child_by_field_name("field"))
                    else {
                        continue;
                    };
                    let base = expr_type(operand, src, 0).unwrap_or_else(|| TypeExpr::Ident("?".to_string()));
                    let value = if right.len() == left.len() { value_flows(right[i], src) } else { None };
                    record_flow(facts, base, text(field, src), value);
                }
            }
            _ => {}
        }
        true
    });
}

/// A composite literal's type, also when elided inside a slice, array or map literal
/// (`[]T{{A: 1}}`, `map[string]T{"k": {A: 1}}`).
fn literal_type(lit: Node, src: &[u8]) -> Option<String> {
    if let Some(t) = lit.child_by_field_name("type") {
        return Some(text(t, src).to_string());
    }
    // literal -> literal_element -> (keyed_element ->) literal_value -> outer composite literal
    let elem = lit.parent().filter(|p| p.kind() == "literal_element")?;
    let mut up = elem.parent()?;
    if up.kind() == "keyed_element" {
        if up.child_by_field_name("value").is_none_or(|v| v.id() != elem.id()) {
            return None; // an elided map key
        }
        up = up.parent()?;
    }
    let outer = up.parent().filter(|o| o.kind() == "composite_literal")?;
    let outer_ty = literal_type(outer, src)?;
    let t = outer_ty.trim();
    let elem_ty = if let Some(rest) = t.strip_prefix("map[") {
        let mut depth = 1;
        let close = rest.char_indices().find(|&(_, c)| {
            match c {
                '[' => depth += 1,
                ']' => depth -= 1,
                _ => {}
            }
            depth == 0
        })?;
        &rest[close.0 + 1..]
    } else {
        let close = t.strip_prefix('[').and_then(|r| r.find(']'))?;
        &t[close + 2..]
    };
    Some(elem_ty.trim().trim_start_matches('*').to_string())
}

fn record_flow(facts: &mut TypeFacts, base: TypeExpr, field: &str, values: Option<Vec<TypeExpr>>) {
    match values {
        Some(vs) => facts.field_flows.extend(vs.into_iter().map(|v| (base.clone(), field.to_string(), Some(v)))),
        None => facts.field_flows.push((base, field.to_string(), None)),
    }
}

/// The type of an expression, as far as the enclosing function shows it.
fn expr_type(n: Node, src: &[u8], depth: u8) -> Option<TypeExpr> {
    if depth > 48 {
        return None;
    }
    let d = depth + 1;
    let field = |f: &str| n.child_by_field_name(f);
    match n.kind() {
        "identifier" => match binding(n, src, d) {
            Binding::Typed(t) => Some(t),
            Binding::Untyped => None,
            Binding::Unbound => Some(TypeExpr::Ident(text(n, src).to_string())),
        },
        "selector_expression" => {
            let operand = expr_type(field("operand")?, src, d)?;
            Some(TypeExpr::Field(Box::new(operand), text(field("field")?, src).to_string()))
        }
        "call_expression" => {
            let f = field("function")?;
            if f.kind() == "identifier" && matches!(text(f, src), "new" | "make") {
                let arg = field("arguments")?.named_child(0)?;
                return Some(TypeExpr::Named(text(arg, src).to_string()));
            }
            Some(TypeExpr::Call(Box::new(expr_type(f, src, d)?)))
        }
        "composite_literal" | "type_assertion_expression" => {
            Some(TypeExpr::Named(text(field("type")?, src).to_string()))
        }
        "unary_expression" => {
            let inner = expr_type(field("operand")?, src, d)?;
            // `&x` and `*x` keep the named type; `<-ch` is the channel's element
            let op = field("operator").map_or("", |o| text(o, src));
            Some(if op == "<-" { TypeExpr::Elem(Box::new(inner)) } else { inner })
        }
        "parenthesized_expression" => expr_type(n.named_child(0)?, src, d),
        // `func(a int) T { .. }`: its signature is its type
        "func_literal" => {
            let body = field("body")?;
            let sig = String::from_utf8_lossy(&src[n.start_byte()..body.start_byte()]);
            Some(TypeExpr::Named(sig.split_whitespace().collect::<Vec<_>>().join(" ")))
        }
        "index_expression" => Some(TypeExpr::Elem(Box::new(expr_type(field("operand")?, src, d)?))),
        "slice_expression" => expr_type(field("operand")?, src, d),
        _ => None,
    }
}

enum Binding {
    /// Declared with a known type.
    Typed(TypeExpr),
    /// Declared locally, type not derivable; shadows any package-level name.
    Untyped,
    /// No local declaration: an import, or a package-level name.
    Unbound,
}

/// The nearest declaration of `ident` visible at its position: an earlier statement in an
/// enclosing block, an `if`/`for`/`switch` header, a type-switch case, or a parameter.
fn binding(ident: Node, src: &[u8], depth: u8) -> Binding {
    let name = text(ident, src);
    if name == "_" {
        return Binding::Untyped;
    }
    let before = |n: Node| n.end_byte() <= ident.start_byte();
    let mut child = ident;
    let mut cur = ident.parent();
    while let Some(scope) = cur {
        let found = match scope.kind() {
            "statement_list" | "block" => named(scope)
                .into_iter()
                .filter(|c| before(*c))
                .filter_map(|c| declares(c, name, src, depth))
                .next_back(),
            "if_statement" | "expression_switch_statement" | "type_switch_statement" | "for_statement" => {
                let header = scope
                    .child_by_field_name("initializer")
                    .or_else(|| named(scope).into_iter().find(|c| matches!(c.kind(), "for_clause" | "range_clause")));
                let header = header.and_then(|h| {
                    if h.kind() == "for_clause" { h.child_by_field_name("initializer") } else { Some(h) }
                });
                let from_header = header.filter(|h| before(*h)).and_then(|h| declares(h, name, src, depth));
                let alias = (scope.kind() == "type_switch_statement")
                    .then(|| scope.child_by_field_name("alias"))
                    .flatten()
                    .filter(|a| named(*a).iter().any(|x| text(*x, src) == name));
                match (alias, child.kind()) {
                    // `switch v := x.(type) { case *T: v... }`: one type per case gives v that type
                    (Some(_), "type_case") => {
                        let types: Vec<Node> = child.children_by_field_name("type", &mut child.walk()).collect();
                        Some(match types.as_slice() {
                            [t] => Binding::Typed(TypeExpr::Named(text(*t, src).to_string())),
                            _ => Binding::Untyped,
                        })
                    }
                    (Some(_), _) => Some(Binding::Untyped),
                    _ => from_header,
                }
            }
            // `case res := <-results:`
            "communication_case" => scope
                .child_by_field_name("communication")
                .filter(|c| c.kind() == "receive_statement" && before(*c))
                .and_then(|c| {
                    let left = named(c.child_by_field_name("left")?);
                    let i = left.iter().position(|l| text(*l, src) == name)?;
                    let right = c.child_by_field_name("right")?;
                    Some(if i == 0 {
                        expr_type(right, src, depth).map_or(Binding::Untyped, Binding::Typed)
                    } else {
                        Binding::Untyped
                    })
                }),
            // a parameter of a named function: typed (and possibly narrowed) by the linker from its
            // callers' arguments; `Param("Owner.func", i)`
            "function_declaration" | "method_declaration" if let Some(i) = param_index(scope, name, src) => {
                let owner = receiver_type(scope, src).unwrap_or_default();
                let func = scope.child_by_field_name("name").map_or("", |n| text(n, src));
                Some(Binding::Typed(TypeExpr::Param(format!("{owner}.{func}"), i)))
            }
            "function_declaration" | "method_declaration" | "func_literal" => {
                let lists =
                    ["receiver", "parameters", "result"].into_iter().filter_map(|f| scope.child_by_field_name(f));
                lists.flat_map(named).find_map(|p| {
                    let declared = p.children_by_field_name("name", &mut p.walk()).any(|n| text(n, src) == name);
                    declared.then(|| match p.child_by_field_name("type") {
                        Some(t) if p.kind() == "variadic_parameter_declaration" => {
                            Binding::Typed(TypeExpr::Named(format!("[]{}", text(t, src))))
                        }
                        Some(t) => Binding::Typed(TypeExpr::Named(text(t, src).to_string())),
                        None => Binding::Untyped,
                    })
                })
            }
            "source_file" => return Binding::Unbound,
            _ => None,
        };
        if let Some(b) = found {
            return b;
        }
        child = scope;
        cur = scope.parent();
    }
    Binding::Unbound
}

/// The positional index of non-variadic parameter `name` of a function declaration.
fn param_index(func: Node, name: &str, src: &[u8]) -> Option<usize> {
    let mut i = 0;
    for p in func.child_by_field_name("parameters").map(named).unwrap_or_default() {
        let names: Vec<Node> = p.children_by_field_name("name", &mut p.walk()).collect();
        for n in &names {
            if text(*n, src) == name {
                return (p.kind() == "parameter_declaration").then_some(i);
            }
            i += 1;
        }
        if names.is_empty() {
            i += 1;
        }
    }
    None
}

/// What `decl` says about `name`, if it declares it.
fn declares(decl: Node, name: &str, src: &[u8], depth: u8) -> Option<Binding> {
    let typed = |t: Option<TypeExpr>| Some(t.map_or(Binding::Untyped, Binding::Typed));
    match decl.kind() {
        "short_var_declaration" => {
            let left = named(decl.child_by_field_name("left")?);
            let right = named(decl.child_by_field_name("right")?);
            let i = left.iter().position(|l| text(*l, src) == name)?;
            match (right.len() == left.len(), i) {
                (true, _) => typed(expr_type(right[i], src, depth)),
                // `v, err := f()` / `v, ok := m[k]`: the first value is the expression's own type;
                // later values of a call are its later results
                (false, 0) => typed(right.first().and_then(|r| expr_type(*r, src, depth))),
                (false, i) => typed(right.first().filter(|r| r.kind() == "call_expression").and_then(|r| {
                    match expr_type(*r, src, depth)? {
                        TypeExpr::Call(callee) => Some(TypeExpr::Result(callee, i)),
                        _ => None,
                    }
                })),
            }
        }
        "range_clause" => {
            let left = named(decl.child_by_field_name("left")?);
            let i = left.iter().position(|l| text(*l, src) == name)?;
            // the first variable is an index, a map key or a channel element; the second the element
            let ranged = decl.child_by_field_name("right").and_then(|r| expr_type(r, src, depth));
            typed(ranged.map(|e| if i == 1 { TypeExpr::Elem(Box::new(e)) } else { TypeExpr::Key(Box::new(e)) }))
        }
        "var_declaration" => {
            let specs: Vec<Node> = named(decl)
                .into_iter()
                .flat_map(|n| if n.kind() == "var_spec_list" { named(n) } else { vec![n] })
                .collect();
            specs.into_iter().rev().find_map(|spec| {
                let names: Vec<Node> = spec.children_by_field_name("name", &mut spec.walk()).collect();
                let i = names.iter().position(|n| text(*n, src) == name)?;
                if let Some(t) = spec.child_by_field_name("type") {
                    return Some(Binding::Typed(TypeExpr::Named(text(t, src).to_string())));
                }
                let values = spec.child_by_field_name("value").map(named).unwrap_or_default();
                typed(values.get(i).and_then(|v| expr_type(*v, src, depth)))
            })
        }
        _ => None,
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
    // Only the first spec of a group inherits the declaration's comment.
    let mut prev = def.prev_named_sibling();
    while prev.is_some_and(|p| p.kind() == "comment") {
        prev = prev.and_then(|p| p.prev_named_sibling());
    }
    let first_spec = prev.is_none();
    let mut anchor = def;
    while let Some(p) = anchor.parent() {
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
    while let Some(c) = prev.filter(|c| c.kind() == "comment" && c.end_position().row + 1 >= row && own_line(*c)) {
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
        assert_eq!(sym(&p, "hidden").doc_comment, "");
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

    #[test]
    fn trailing_comments_and_later_group_specs_get_no_doc() {
        let p = parse(
            "package s\n\nconst (\n\tA = 1 // first\n\tB = 2\n)\n\n// Defaults.\nvar (\n\tX = 1\n\tY = 2\n)\n\nvar z = 1 // trailing\nfunc f() {}\n",
        );
        assert_eq!(sym(&p, "B").doc_comment, "");
        assert_eq!(sym(&p, "X").doc_comment, "Defaults.");
        assert_eq!(sym(&p, "Y").doc_comment, "");
        assert_eq!(sym(&p, "f").doc_comment, "");
    }
}
