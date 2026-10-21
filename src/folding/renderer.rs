//! Distance 0/1/2+ progressive-disclosure Markdown (spec §8).

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Result, bail};
use cozo::DbInstance;

use super::pagination::{self, Cursor, Direction};
use crate::db::queries::{self, Contract, Rollup, Semantics, StoredEdge, SymbolInfo};
use crate::mcp::batch::{clip_source, read_checked};

pub const DEFAULT_NEIGHBORS: usize = 12;
pub const MAX_NEIGHBORS: usize = 50;
const MAX_SYMBOLS: usize = 5;
/// Source lines shown per symbol at Distance 0.
const MAX_SOURCE_LINES: usize = 800;
const ENTRYPOINTS_SHOWN: usize = 5;
const REACH_VISIT_CAP: usize = 5_000;

pub struct ZoomOptions {
    pub include_raw_source: bool,
    pub max_neighbors: usize,
    pub cursor: Option<Cursor>,
}

impl Default for ZoomOptions {
    fn default() -> Self {
        Self { include_raw_source: true, max_neighbors: DEFAULT_NEIGHBORS, cursor: None }
    }
}

struct Ctx<'a> {
    repo_root: &'a Path,
    file_hashes: HashMap<String, String>,
    rollups: Vec<Rollup>,
    /// caller -> callees and callee -> callers over live CALLS edges
    forward: HashMap<String, Vec<String>>,
    backward: HashMap<String, Vec<String>>,
    symbols: HashMap<String, SymbolInfo>,
    semantics: HashMap<String, Semantics>,
    contracts: HashMap<String, Contract>,
    source_cache: HashMap<String, Option<String>>,
}

pub fn render_zoom(db: &DbInstance, repo_root: &Path, symbol_ids: &[String], opts: &ZoomOptions) -> Result<String> {
    if symbol_ids.is_empty() {
        bail!("symbol_ids must contain at least one symbol_id");
    }
    if symbol_ids.len() > MAX_SYMBOLS {
        bail!("at most {MAX_SYMBOLS} symbol_ids per call (got {})", symbol_ids.len());
    }
    if opts.cursor.is_some() && symbol_ids.len() != 1 {
        bail!("neighbor_cursor applies to a single symbol; pass exactly one symbol_id");
    }
    let limit = opts.max_neighbors.clamp(1, MAX_NEIGHBORS);

    let targets = queries::symbols_by_ids(db, symbol_ids)?;
    if let Some(missing) = symbol_ids.iter().find(|id| !targets.contains_key(*id)) {
        bail!("unknown symbol_id `{missing}`; use search or query tools to discover exact ids");
    }

    let in_edges = queries::edges_to(db, symbol_ids)?;
    let out_edges = queries::edges_from(db, symbol_ids)?;
    let mut needed: Vec<String> = symbol_ids.to_vec();
    needed.extend(in_edges.iter().map(|e| e.from.clone()));
    needed.extend(out_edges.iter().map(|e| e.to.clone()));
    needed.sort();
    needed.dedup();

    let calls = queries::call_edges(db)?;
    let mut forward: HashMap<String, Vec<String>> = HashMap::new();
    let mut backward: HashMap<String, Vec<String>> = HashMap::new();
    for (f, t) in calls {
        forward.entry(f.clone()).or_default().push(t.clone());
        backward.entry(t).or_default().push(f);
    }

    let mut ctx = Ctx {
        repo_root,
        file_hashes: queries::file_hashes(db)?,
        rollups: queries::rollups(db)?,
        forward,
        backward,
        symbols: queries::symbols_by_ids(db, &needed)?,
        semantics: queries::semantics_by_ids(db, &needed)?,
        contracts: queries::contracts_by_ids(db, &needed)?,
        source_cache: HashMap::new(),
    };

    let mut out = String::new();
    for (i, id) in symbol_ids.iter().enumerate() {
        if i > 0 {
            out.push_str("\n---\n\n");
        }
        let sym = ctx.symbols[id].clone();
        let callers: Vec<&StoredEdge> =
            in_edges.iter().filter(|e| e.to == *id && e.confidence != "LLM_PRUNED").collect();
        let callees: Vec<&StoredEdge> =
            out_edges.iter().filter(|e| e.from == *id && e.confidence != "LLM_PRUNED").collect();
        render_one(db, &mut ctx, &sym, &callers, &callees, opts, limit, &mut out)?;
    }
    Ok(out)
}

fn flag_json(flags: &[String]) -> String {
    serde_json::to_string(flags).unwrap_or_else(|_| "[]".into())
}

/// `* **Label:** None` for an empty list, otherwise the label followed by one bullet per item.
fn section(out: &mut String, label: &str, items: &[String], marker: &str) {
    if items.is_empty() {
        let _ = writeln!(out, "* **{label}:** None");
        return;
    }
    let _ = writeln!(out, "* **{label}:**");
    for it in items {
        let _ = writeln!(out, "  * {marker}{it}");
    }
}

#[allow(clippy::too_many_arguments)]
fn render_one(
    db: &DbInstance,
    ctx: &mut Ctx,
    sym: &SymbolInfo,
    callers: &[&StoredEdge],
    callees: &[&StoredEdge],
    opts: &ZoomOptions,
    limit: usize,
    out: &mut String,
) -> Result<()> {
    let sem = ctx.semantics.get(&sym.symbol_id).cloned();
    let con = ctx.contracts.get(&sym.symbol_id).cloned();

    let freshness = match &sem {
        Some(s) if s.annotated_ast_blake3 == sym.ast_blake3 => " (AST Fresh ✓)",
        Some(_) => " (AST STALE ✗ — code changed since this model was written)",
        None => "",
    };
    let _ = writeln!(out, "# SYMBOL ZOOM: `{}`", sym.symbol_id);
    let _ = writeln!(
        out,
        "**Status:** `{}`{} | **Complexity:** `{}` | **Risk Score:** `{}`",
        sym.mapping_status,
        freshness,
        sym.complexity,
        con.as_ref().map_or("n/a".to_string(), |c| format!("{}/10", c.risk_score))
    );
    let _ = writeln!(
        out,
        "**Risk Flags:** `{}` | **Trust Boundary:** `{}`{}\n",
        flag_json(&sym.risk_flags),
        con.as_ref().map_or("UNKNOWN", |c| if c.trust_boundary { "TRUE" } else { "FALSE" }),
        if sym.is_entrypoint { " | **Entrypoint:** `TRUE`" } else { "" }
    );

    // A cursor selects one neighbour page and nothing else.
    if let Some(cursor) = opts.cursor {
        let (list, label) = match cursor.direction {
            Direction::Caller => (callers, "UPSTREAM CALLERS (Who calls this?"),
            Direction::Callee => (callees, "DOWNSTREAM CALLEES (What does this call?"),
        };
        render_neighbors(ctx, cursor.direction, label, list, cursor.offset, limit, out);
        return Ok(());
    }

    render_tier3(db, ctx, sym, out)?;

    let _ = writeln!(out, "\n## [DISTANCE 0: SEMANTIC CONTRACT & AUDIT MODEL]");
    match (&sem, &con) {
        (None, _) => {
            let _ = writeln!(out, "* *Not yet mapped.* Static facts only.");
            let _ = writeln!(out, "* **Signature:** `{}`", sym.signature);
            if !sym.doc_comment.is_empty() {
                let _ = writeln!(out, "* **Doc:** {}", sym.doc_comment.replace('\n', " "));
            }
        }
        (Some(s), c) => {
            let _ = writeln!(out, "* **Intent:** {}", s.intent_summary);
            let _ = writeln!(out, "* **Mechanism:** {}", s.mechanism_prose);
            if !s.domain_tags.is_empty() {
                let _ = writeln!(out, "* **Tags:** {}", s.domain_tags.join(", "));
            }
            if let Some(c) = c {
                section(out, "Invariants Assumed (Preconditions NOT checked here)", &c.invariants_assumed, "`!` ");
                section(out, "Invariants Enforced", &c.invariants_enforced, "");
                section(out, "State Mutations", &c.state_mutations, "");
                section(out, "Error Surface", &c.error_surface, "");
                if !c.concurrency_assumptions.is_empty() {
                    section(out, "Concurrency Assumptions", &c.concurrency_assumptions, "");
                }
                if !c.taint_sources.is_empty() || !c.taint_sinks.is_empty() {
                    let _ = writeln!(
                        out,
                        "* **Taint:** sources {} → sinks {}",
                        flag_json(&c.taint_sources),
                        flag_json(&c.taint_sinks)
                    );
                }
                section(out, "Audit Smells", &c.audit_smells, "");
            }
        }
    }

    if opts.include_raw_source {
        let _ =
            writeln!(out, "\n## [DISTANCE 0: SOURCE CODE (`{}:{}-{}`)]", sym.file_path, sym.start_line, sym.end_line);
        let hash = ctx.file_hashes.get(&sym.file_path).cloned();
        let text = ctx
            .source_cache
            .entry(sym.file_path.clone())
            .or_insert_with(|| read_checked(ctx.repo_root, &sym.file_path, hash.as_ref()));
        match text.as_deref().and_then(|t| t.get(sym.start_byte..sym.end_byte)) {
            Some(code) => {
                let (shown, omitted) = clip_source(code, MAX_SOURCE_LINES);
                let _ = writeln!(out, "```{}\n{shown}\n```", language_fence(&sym.file_path));
                if omitted > 0 {
                    let _ = writeln!(
                        out,
                        "> [{omitted} more lines omitted ({}:{}-{}). Read the file directly for the rest; `expand_function_cfg` shows the branch structure.]",
                        sym.file_path,
                        sym.start_line + MAX_SOURCE_LINES as i64,
                        sym.end_line
                    );
                }
            }
            None => {
                let _ = writeln!(
                    out,
                    "> SOURCE UNAVAILABLE: `{}` changed since the last sync. Call `sync_repository_skeleton`.",
                    sym.file_path
                );
            }
        }
    }

    out.push('\n');
    render_neighbors(ctx, Direction::Caller, "UPSTREAM CALLERS (Who calls this?", callers, 0, limit, out);
    out.push('\n');
    render_neighbors(ctx, Direction::Callee, "DOWNSTREAM CALLEES (What does this call?", callees, 0, limit, out);
    Ok(())
}

fn language_fence(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "rs" => "rust",
        "py" => "python",
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" => "typescript",
        "go" => "go",
        "c" | "h" => "c",
        "cc" | "cpp" | "hpp" => "cpp",
        "java" => "java",
        _ => "",
    }
}

fn render_tier3(db: &DbInstance, ctx: &Ctx, sym: &SymbolInfo, out: &mut String) -> Result<()> {
    let _ = writeln!(out, "## [TIER 3: THE LARGER PICTURE]");
    let module = ctx
        .rollups
        .iter()
        .filter(|r| sym.file_path == r.module_path || sym.file_path.starts_with(&format!("{}/", r.module_path)))
        .max_by_key(|r| r.module_path.len());
    match module {
        Some(m) => {
            let _ = writeln!(out, "* **Module (`{}`):** {}", m.module_path, m.architectural_role);
            if !m.known_architectural_risks.is_empty() {
                let _ = writeln!(out, "  * Known module risks: {}", m.known_architectural_risks.join("; "));
            }
        }
        None => {
            let _ = writeln!(out, "* **Module:** no rollup recorded for `{}`", sym.file_path);
        }
    }

    let flows = queries::flow_memberships(db, &sym.symbol_id)?;
    if flows.is_empty() {
        let _ = writeln!(out, "* **Participates in Execution Flows:** none recorded");
    } else {
        let _ = writeln!(out, "* **Participates in Execution Flows:**");
        for (i, f) in flows.iter().enumerate() {
            let _ = writeln!(
                out,
                "  {}. `{}` (Step {} of {}) — *{}*",
                i + 1,
                f.flow_id,
                f.step_order,
                f.total_steps,
                f.role_in_flow
            );
        }
    }

    // Distance 2+: how far the blast radius reaches, and which entrypoints can get here.
    let up = reach(&sym.symbol_id, &ctx.backward);
    let down = reach(&sym.symbol_id, &ctx.forward);
    let _ =
        writeln!(out, "* **Distance 2+ reach:** {} transitive callers, {} transitive callees", up.len(), down.len());
    let entry_ids: Vec<&String> = up.iter().collect();
    let entries = queries::symbols_by_ids(db, &entry_ids.iter().map(|s| (*s).clone()).collect::<Vec<_>>())?;
    let mut entry_names: Vec<&str> =
        entries.values().filter(|s| s.is_entrypoint).map(|s| s.symbol_id.as_str()).collect();
    entry_names.sort_unstable();
    if sym.is_entrypoint {
        let _ = writeln!(out, "* **Reachable from entrypoints:** this symbol is itself an entrypoint");
    } else if entry_names.is_empty() {
        let _ = writeln!(out, "* **Reachable from entrypoints:** none found");
    } else {
        let shown: Vec<String> = entry_names.iter().take(ENTRYPOINTS_SHOWN).map(|n| format!("`{n}`")).collect();
        let more = entry_names.len().saturating_sub(ENTRYPOINTS_SHOWN);
        let _ = writeln!(
            out,
            "* **Reachable from entrypoints:** {}{}",
            shown.join(", "),
            if more > 0 { format!(" (+{more} more)") } else { String::new() }
        );
    }
    Ok(())
}

/// Transitive closure excluding the start node; capped so a hub can't stall a render.
fn reach(start: &str, adj: &HashMap<String, Vec<String>>) -> HashSet<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue = VecDeque::from([start.to_string()]);
    while let Some(n) = queue.pop_front() {
        for next in adj.get(&n).into_iter().flatten() {
            if next != start && seen.insert(next.clone()) {
                if seen.len() >= REACH_VISIT_CAP {
                    return seen;
                }
                queue.push_back(next.clone());
            }
        }
    }
    seen
}

fn line_label(lines: &[i64]) -> String {
    match lines {
        [] => String::new(),
        [one] => format!(" (Line {one})"),
        many => format!(" (Lines {})", many.iter().map(i64::to_string).collect::<Vec<_>>().join(", ")),
    }
}

fn render_neighbors(
    ctx: &Ctx,
    direction: Direction,
    label: &str,
    edges: &[&StoredEdge],
    offset: usize,
    limit: usize,
    out: &mut String,
) {
    let neighbor_of = |e: &StoredEdge| if direction == Direction::Caller { e.from.clone() } else { e.to.clone() };
    let risk = |id: &str| ctx.contracts.get(id).map_or(0, |c| c.risk_score);

    // §8.1: tainted first, then riskiest, then stable by id.
    let mut sorted: Vec<&StoredEdge> = edges.to_vec();
    sorted.sort_by(|a, b| {
        let (na, nb) = (neighbor_of(a), neighbor_of(b));
        b.taint.cmp(&a.taint).then(risk(&nb).cmp(&risk(&na))).then(na.cmp(&nb)).then(a.kind.cmp(&b.kind))
    });

    let (shown, remaining) = pagination::page(&sorted, offset, limit);
    let first = if shown.is_empty() { 0 } else { offset + 1 };
    let _ = writeln!(out, "## [DISTANCE 1: {label} — Showing {}–{} of {})]", first, offset + shown.len(), sorted.len());
    if sorted.is_empty() {
        let _ = writeln!(out, "None.");
    }
    let role = if direction == Direction::Caller { "Caller" } else { "Callee" };
    for (i, e) in shown.iter().enumerate() {
        let id = neighbor_of(e);
        let kind = if e.kind == "CALLS" { String::new() } else { format!(" {}", e.kind) };
        let _ = writeln!(out, "{}. **`{id}`** `[{}]`{}{}", offset + i + 1, e.confidence, kind, line_label(&e.lines));
        match ctx.semantics.get(&id) {
            Some(s) => {
                let _ = writeln!(out, "   * *{role} Intent:* {}", s.intent_summary);
            }
            None => {
                let sig = ctx.symbols.get(&id).map_or("", |s| s.signature.as_str());
                let _ = writeln!(out, "   * *{role} Intent:* (unmapped) `{sig}`");
            }
        }
        if !e.semantics.is_empty() || e.taint {
            let _ = writeln!(
                out,
                "   * *Edge Semantics:* {}{}",
                e.semantics,
                if e.taint { " **[Taint Propagated: TRUE]**" } else { "" }
            );
        }
        if direction == Direction::Callee
            && let Some(c) = ctx.contracts.get(&id)
            && !c.invariants_assumed.is_empty()
        {
            let _ = writeln!(out, "   * *Callee Assumptions:* {}", c.invariants_assumed.join("; "));
        }
    }
    if let Some(n) = pagination::omitted_notice(direction, remaining, offset + shown.len()) {
        let _ = writeln!(out, "{n}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinator::embeddings::HashEmbedder;
    use crate::mcp::architecture::{self, FlowArgs, FlowStepInput, RollupArgs};
    use crate::mcp::batch::tests::repo;
    use crate::mcp::submit::*;

    fn id(db: &DbInstance, name: &str) -> String {
        queries::all_symbols(db).unwrap().into_iter().find(|s| s.name == name).unwrap().symbol_id
    }

    fn model(id: &str, intent: &str) -> SymbolModel {
        SymbolModel {
            symbol_id: id.into(),
            intent_summary: intent.into(),
            mechanism_prose: "mechanism".into(),
            domain_tags: vec![],
            promote_to_strict_contract: false,
            strict_contract: None,
            resolved_edges: vec![],
        }
    }

    fn edge(to: &str, taint: bool, semantics: &str) -> ResolvedEdge {
        ResolvedEdge {
            to_symbol_id: to.into(),
            edge_kind: EdgeKind::Calls,
            resolution: Resolution::LlmVerified,
            edge_semantics: semantics.into(),
            data_taint_propagated: taint,
        }
    }

    fn submit(db: &DbInstance, models: Vec<SymbolModel>) {
        if let Err(SubmitError::Invalid(e)) = submit_symbol_models(db, &HashEmbedder, SubmitArgs { models }) {
            panic!("{e:?}");
        }
    }

    const JWT: &str = r#"
pub fn verify_token(raw: &str) -> u64 {
    let key = get_key(raw);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
    hmac(key, now.as_secs())
}
fn get_key(kid: &str) -> u64 { kid.len() as u64 }
fn hmac(k: u64, v: u64) -> u64 { k ^ v }
fn auth_middleware(h: &str) -> u64 { verify_token(h) }
fn ws_upgrade(ticket: &str) -> u64 { verify_token(ticket) }
"#;

    fn jwt_world() -> (tempfile::TempDir, DbInstance) {
        let (dir, db) = repo(&[("src/auth/jwt.rs", JWT)]);
        let v = id(&db, "verify_token");
        let strict = StrictContractInput {
            invariants_assumed: vec!["caller already stripped the Bearer prefix".into(), "KeyRing is populated".into()],
            invariants_enforced: vec!["constant-time signature compare".into()],
            state_mutations: vec![],
            error_surface: vec!["PANIC HAZARD: unwrap on SystemTime".into()],
            concurrency_assumptions: vec![],
            trust_boundary: true,
            taint_sources: vec!["raw".into()],
            taint_sinks: vec![],
            audit_smells: vec!["[HIGH] alg header trusted".into()],
            risk_score: 8,
        };
        submit(
            &db,
            vec![
                SymbolModel {
                    strict_contract: Some(strict),
                    resolved_edges: vec![
                        edge(&id(&db, "get_key"), true, "passes untrusted kid"),
                        edge(&id(&db, "hmac"), false, "signing input"),
                    ],
                    ..model(&v, "Parses a JWS and verifies it")
                },
                model(&id(&db, "get_key"), "Looks up a key by kid"),
                SymbolModel {
                    resolved_edges: vec![edge(&v, true, "strips Bearer then passes slice")],
                    ..model(&id(&db, "auth_middleware"), "Attaches claims to requests")
                },
                SymbolModel {
                    resolved_edges: vec![edge(&v, false, "passes ticket")],
                    ..model(&id(&db, "ws_upgrade"), "Upgrades to websocket")
                },
            ],
        );
        architecture::submit_module_rollup(
            &db,
            &HashEmbedder,
            RollupArgs {
                module_path: "src/auth".into(),
                parent_module_path: None,
                architectural_role: "Stateless session authentication".into(),
                key_abstractions: vec![],
                cross_module_contracts: vec![],
                known_architectural_risks: vec!["no key rotation".into()],
            },
        )
        .unwrap()
        .ok()
        .unwrap();
        architecture::record_execution_flow(
            &db,
            &HashEmbedder,
            FlowArgs {
                flow_id: "api_request_auth_guard".into(),
                name: "Auth guard".into(),
                entrypoint_symbol_ids: vec![id(&db, "auth_middleware")],
                terminal_sink_symbol_ids: vec![id(&db, "hmac")],
                narrative_summary: "validates bearer".into(),
                state_lifecycle_summary: "none".into(),
                security_boundaries_crossed: vec![],
                steps: vec![
                    FlowStepInput {
                        symbol_id: id(&db, "auth_middleware"),
                        role_in_flow: "extracts header".into(),
                        critical_assumptions_at_step: String::new(),
                    },
                    FlowStepInput {
                        symbol_id: v,
                        role_in_flow: "Validates Bearer header token".into(),
                        critical_assumptions_at_step: String::new(),
                    },
                ],
            },
        )
        .unwrap()
        .ok()
        .unwrap();
        (dir, db)
    }

    fn zoom(db: &DbInstance, dir: &Path, ids: &[String], opts: ZoomOptions) -> String {
        render_zoom(db, dir, ids, &opts).unwrap()
    }

    #[test]
    fn zoom_renders_every_section_of_the_wire_format_in_order() {
        let (dir, db) = jwt_world();
        let v = id(&db, "verify_token");
        let md = zoom(&db, dir.path(), std::slice::from_ref(&v), ZoomOptions::default());

        let positions: Vec<usize> = [
            format!("# SYMBOL ZOOM: `{v}`"),
            "**Status:** `STRICT_MAPPED` (AST Fresh ✓)".into(),
            "**Risk Score:** `8/10`".into(),
            "**Trust Boundary:** `TRUE`".into(),
            "## [TIER 3: THE LARGER PICTURE]".into(),
            "* **Module (`src/auth`):** Stateless session authentication".into(),
            "`flow::api_request_auth_guard` (Step 2 of 2) — *Validates Bearer header token*".into(),
            "## [DISTANCE 0: SEMANTIC CONTRACT & AUDIT MODEL]".into(),
            "`!` caller already stripped the Bearer prefix".into(),
            "PANIC HAZARD: unwrap on SystemTime".into(),
            "## [DISTANCE 0: SOURCE CODE (`src/auth/jwt.rs:2-6`)]".into(),
            "pub fn verify_token(raw: &str) -> u64 {".into(),
            "## [DISTANCE 1: UPSTREAM CALLERS (Who calls this? — Showing 1–2 of 2)]".into(),
            "## [DISTANCE 1: DOWNSTREAM CALLEES (What does this call? — Showing 1–2 of 2)]".into(),
        ]
        .iter()
        .map(|needle| md.find(needle.as_str()).unwrap_or_else(|| panic!("missing `{needle}` in:\n{md}")))
        .collect();
        assert!(positions.windows(2).all(|w| w[0] < w[1]), "sections out of order:\n{md}");
        assert!(md.contains("unchecked_unwrap"));
        assert!(md.contains("Known module risks: no key rotation"));
        assert!(md.contains("2 transitive callers, 2 transitive callees"));
    }

    #[test]
    fn neighbours_are_taint_first_with_edge_semantics_and_callee_assumptions() {
        let (dir, db) = jwt_world();
        let v = id(&db, "verify_token");
        let md = zoom(&db, dir.path(), &[v], ZoomOptions { include_raw_source: false, ..Default::default() });
        // auth_middleware (tainted) must rank above ws_upgrade
        let m = md.find("auth_middleware").unwrap();
        let w = md.find("ws_upgrade").unwrap();
        assert!(m < w);
        assert!(md.contains("*Edge Semantics:* strips Bearer then passes slice **[Taint Propagated: TRUE]**"));
        assert!(md.contains("*Caller Intent:* Attaches claims to requests"));
        assert!(md.contains("*Callee Intent:* Looks up a key by kid"));
        assert!(!md.contains("SOURCE CODE"));
        // the tainted get_key edge also sorts before hmac
        assert!(md.find("get_key").unwrap() < md.find("::hmac#").unwrap());
    }

    #[test]
    fn unmapped_symbol_shows_static_facts_and_stale_models_are_flagged() {
        let (dir, db) = jwt_world();
        let md = zoom(&db, dir.path(), &[id(&db, "hmac")], ZoomOptions::default());
        assert!(
            md.contains("`UNMAPPED`")
                && md.contains("*Not yet mapped.*")
                && md.contains("fn hmac(k: u64, v: u64) -> u64")
        );

        // edit the code under a mapped symbol and resync -> STALE banner
        std::fs::write(dir.path().join("src/auth/jwt.rs"), JWT.replace("kid.len() as u64", "kid.len() as u64 + 1"))
            .unwrap();
        crate::ingestion::sync::sync_repository(&db, dir.path()).unwrap();
        let md = zoom(&db, dir.path(), &[id(&db, "get_key")], ZoomOptions::default());
        assert!(md.contains("`STALE` (AST STALE ✗"), "{md}");
    }

    #[test]
    fn hub_callers_paginate_deterministically_without_gaps_or_repeats() {
        let mut src = String::from("fn hub() {}\n");
        for i in 0..30 {
            src.push_str(&format!("fn caller_{i:02}() {{ hub(); }}\n"));
        }
        let (dir, db) = repo(&[("src/lib.rs", &src)]);
        let hub = id(&db, "hub");

        let page1 = zoom(
            &db,
            dir.path(),
            std::slice::from_ref(&hub),
            ZoomOptions { include_raw_source: false, ..Default::default() },
        );
        assert!(page1.contains("UPSTREAM CALLERS (Who calls this? — Showing 1–12 of 30)"));
        assert!(page1.contains("[+18 additional callers omitted. Call inspect_symbol_zoom with neighbor_cursor=\"caller:offset:12\" to view next page.]"));

        let mut seen: Vec<String> = Vec::new();
        let mut cursor = None;
        loop {
            let md = zoom(
                &db,
                dir.path(),
                std::slice::from_ref(&hub),
                ZoomOptions { include_raw_source: false, cursor, ..Default::default() },
            );
            for line in md.lines().filter(|l| l.contains("**`src/lib.rs::::caller_")) {
                seen.push(line.split('`').nth(1).unwrap().to_string());
            }
            match md.split("neighbor_cursor=\"").nth(1).map(|r| r.split('"').next().unwrap().to_string()) {
                Some(c) => cursor = Some(Cursor::parse(&c).unwrap()),
                None => break,
            }
        }
        assert_eq!(seen.len(), 30);
        let mut sorted = seen.clone();
        sorted.sort();
        assert_eq!(seen, sorted, "equal-rank neighbours must come out in id order");
        sorted.dedup();
        assert_eq!(sorted.len(), 30);
    }

    #[test]
    fn argument_errors_are_explicit() {
        let (dir, db) = jwt_world();
        let v = id(&db, "verify_token");
        let err = |ids: Vec<String>, cursor| {
            render_zoom(&db, dir.path(), &ids, &ZoomOptions { cursor, ..Default::default() }).unwrap_err().to_string()
        };
        assert!(err(vec![], None).contains("at least one"));
        assert!(err(vec!["nope".into()], None).contains("unknown symbol_id"));
        assert!(err(vec![v.clone(); 6], None).contains("at most 5"));
        let c = Some(Cursor { direction: Direction::Caller, offset: 0 });
        assert!(err(vec![v.clone(), id(&db, "hmac")], c).contains("single symbol"));
    }

    #[test]
    fn several_symbols_render_as_separate_views() {
        let (dir, db) = jwt_world();
        let md = zoom(&db, dir.path(), &[id(&db, "hmac"), id(&db, "get_key")], ZoomOptions::default());
        assert_eq!(md.matches("# SYMBOL ZOOM:").count(), 2);
        assert!(md.contains("\n---\n"));
    }
}
