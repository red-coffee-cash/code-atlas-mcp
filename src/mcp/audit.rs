//! `query_audit_surface` and `trace_connective_paths` (spec §7.2, §9).
//!
//! Taint reachability and path tracing run as bounded graph traversals in Rust rather than the
//! spec's path-accumulating recursive Datalog (§9.1): that formulation enumerates every simple
//! path and cannot be capped inside Cozo, so a dense hub would blow up. Here each source expands
//! every node once (one shortest path per source/sink pair), with explicit result and expansion
//! caps and a `truncated` marker. Query B (§9.2) is still Datalog, re-ranked in Rust.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;

use anyhow::{Result, bail};
use cozo::DbInstance;
use rmcp::schemars::{self, JsonSchema};
use serde::Deserialize;

use crate::coordinator::graph::AtlasGraph;
use crate::db::queries::{self, StoredEdge, SymbolInfo};

const MAX_HOPS: usize = 8;
const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 100;
/// Node expansions allowed per query before the result is flagged truncated.
const EXPANSION_BUDGET: usize = 200_000;
const PATH_CAP: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AuditMode {
    HighRiskContracts,
    TrustBoundaryToSinkPaths,
    UncheckedAssumptionHotspots,
    AmbiguousUnverifiedEdges,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct AuditArgs {
    /// Defaults to HIGH_RISK_CONTRACTS.
    pub query_mode: Option<AuditMode>,
    /// Default 6.
    pub min_risk_score: Option<i64>,
    /// Symbols must carry all of these static risk flags (e.g. `unchecked_unwrap`).
    pub required_risk_flags: Option<Vec<String>>,
    /// Taint-path depth; default 4, max 8.
    pub max_hop_depth: Option<usize>,
    pub scope_path_prefix: Option<String>,
    /// Default 20, max 100.
    pub limit: Option<usize>,
}

fn passes(args: &AuditArgs, s: &SymbolInfo) -> bool {
    args.scope_path_prefix.as_deref().is_none_or(|p| s.file_path.starts_with(p))
        && args.required_risk_flags.as_deref().unwrap_or(&[]).iter().all(|f| s.risk_flags.contains(f))
}

fn intent(db_sem: &HashMap<String, queries::Semantics>, id: &str) -> String {
    db_sem.get(id).map_or("(unmapped)".to_string(), |s| s.intent_summary.clone())
}

fn take_summary(items: &[String], n: usize) -> String {
    let shown: Vec<&str> = items.iter().take(n).map(String::as_str).collect();
    let more = items.len().saturating_sub(n);
    format!("{}{}", shown.join("; "), if more > 0 { format!(" (+{more} more)") } else { String::new() })
}

pub fn query_audit_surface(db: &DbInstance, args: &AuditArgs) -> Result<String> {
    let mode = args.query_mode.unwrap_or(AuditMode::HighRiskContracts);
    let limit = args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let min_risk = args.min_risk_score.unwrap_or(6);
    let g = AtlasGraph::load(db)?;
    let ids: Vec<String> = g.symbols.keys().cloned().collect();
    let sem = queries::semantics_by_ids(db, &ids)?;

    let mut out = String::new();
    match mode {
        AuditMode::HighRiskContracts => {
            let reachable = g.reachable_from_entrypoints();
            let mut rows: Vec<(i64, &SymbolInfo)> = g
                .contracts
                .iter()
                .filter(|(_, c)| c.risk_score >= min_risk)
                .filter_map(|(id, c)| g.symbols.get(id).map(|s| (c, s)))
                .filter(|(_, s)| passes(args, s))
                .map(|(c, s)| {
                    let score = c.risk_score * 10
                        + if c.trust_boundary { 5 } else { 0 }
                        + if reachable.contains(&s.symbol_id) { 5 } else { 0 }
                        + g.call_in_degree(&s.symbol_id).min(10) as i64;
                    (score, s)
                })
                .collect();
            rows.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.symbol_id.cmp(&b.1.symbol_id)));
            let total = rows.len();
            let _ = writeln!(
                out,
                "# AUDIT SURFACE — HIGH_RISK_CONTRACTS (risk ≥ {min_risk}): {} of {total}\n",
                total.min(limit)
            );
            for (i, (score, s)) in rows.into_iter().take(limit).enumerate() {
                let c = &g.contracts[&s.symbol_id];
                let _ = writeln!(out, "{}. **`{}`** — risk {}/10 · rank {score}", i + 1, s.symbol_id, c.risk_score);
                let _ = writeln!(
                    out,
                    "   * {}:{}-{} · trust boundary: {} · reachable from entrypoint: {} · callers: {} · flags {:?}",
                    s.file_path,
                    s.start_line,
                    s.end_line,
                    c.trust_boundary,
                    reachable.contains(&s.symbol_id),
                    g.call_in_degree(&s.symbol_id),
                    s.risk_flags
                );
                let _ = writeln!(out, "   * intent: {}", intent(&sem, &s.symbol_id));
                if !c.audit_smells.is_empty() {
                    let _ = writeln!(out, "   * smells: {}", take_summary(&c.audit_smells, 3));
                }
                if !c.invariants_assumed.is_empty() {
                    let _ = writeln!(out, "   * assumes: {}", take_summary(&c.invariants_assumed, 3));
                }
            }
            if total == 0 {
                let _ = writeln!(
                    out,
                    "No strict contracts at or above that risk score. Mapping may be incomplete; check get_macro_atlas coverage."
                );
            }
        }

        AuditMode::TrustBoundaryToSinkPaths => {
            let hops = args.max_hop_depth.unwrap_or(4).clamp(1, MAX_HOPS);
            let mut sources: Vec<&String> = g
                .contracts
                .iter()
                .filter(|(_, c)| c.trust_boundary)
                .map(|(id, _)| id)
                .filter(|id| g.symbols.get(*id).is_some_and(|s| passes(args, s)))
                .collect();
            sources.sort();
            let found = taint_paths(&g, &sources, hops, limit);
            let _ = writeln!(
                out,
                "# AUDIT SURFACE — TRUST_BOUNDARY_TO_SINK_PATHS (≤ {hops} hops): {} paths from {} trust boundaries{}\n",
                found.paths.len(),
                sources.len(),
                if found.truncated { " — TRUNCATED (raise limit or narrow scope_path_prefix)" } else { "" }
            );
            for (i, p) in found.paths.iter().enumerate() {
                let sink = p.nodes.last().expect("path has nodes");
                let c = &g.contracts[sink];
                let _ = writeln!(out, "{}. **{} hops** `{}` → `{sink}`", i + 1, p.edges.len(), p.nodes[0]);
                render_chain(&mut out, &g, &sem, p, "   ");
                if !c.taint_sinks.is_empty() {
                    let _ = writeln!(out, "   * sink operations: {}", take_summary(&c.taint_sinks, 3));
                }
                if !c.state_mutations.is_empty() {
                    let _ = writeln!(out, "   * state mutations: {}", take_summary(&c.state_mutations, 3));
                }
                if !c.audit_smells.is_empty() {
                    let _ = writeln!(out, "   * smells at sink: {}", take_summary(&c.audit_smells, 3));
                }
            }
            if found.paths.is_empty() {
                let _ = writeln!(
                    out,
                    "No taint-propagating path found. This needs contracts with `trust_boundary` and edges marked `data_taint_propagated`; check coverage with get_macro_atlas."
                );
            }
        }

        AuditMode::UncheckedAssumptionHotspots => {
            let mut pairs = queries::assumption_pairs(db, min_risk, limit * 10)?;
            pairs.retain(|p| g.symbols.get(&p.caller).is_some_and(|s| passes(args, s)));
            let rank = |p: &queries::AssumptionPair| {
                p.callee_risk * 10
                    + if p.taint { 5 } else { 0 }
                    + if p.caller_enforced.is_empty() { 3 } else { 0 }
                    + if p.caller_smells.is_empty() { 0 } else { 2 }
            };
            pairs.sort_by(|a, b| rank(b).cmp(&rank(a)).then((&a.caller, &a.callee).cmp(&(&b.caller, &b.callee))));
            let _ = writeln!(
                out,
                "# AUDIT SURFACE — UNCHECKED_ASSUMPTION_HOTSPOTS (callee risk ≥ {min_risk}): {}\n",
                pairs.len().min(limit)
            );
            let _ = writeln!(
                out,
                "Each pair: the callee assumes preconditions it does not check; compare them with what the caller enforces. \
                 Ranked by callee risk, then taint-carrying edge, then callers that enforce nothing.\n"
            );
            for (i, p) in pairs.iter().take(limit).enumerate() {
                let _ = writeln!(
                    out,
                    "{}. `{}` → **`{}`** — callee risk {}/10{}",
                    i + 1,
                    p.caller,
                    p.callee,
                    p.callee_risk,
                    if p.taint { " · **[Taint Propagated]**" } else { "" }
                );
                let _ = writeln!(out, "   * callee assumes: {}", take_summary(&p.callee_assumptions, 4));
                let _ = writeln!(
                    out,
                    "   * caller enforces: {}",
                    if p.caller_enforced.is_empty() {
                        "nothing recorded".to_string()
                    } else {
                        take_summary(&p.caller_enforced, 4)
                    }
                );
                if !p.edge_semantics.is_empty() {
                    let _ = writeln!(out, "   * edge: {}", p.edge_semantics);
                }
                if !p.caller_smells.is_empty() {
                    let _ = writeln!(out, "   * caller smells: {}", take_summary(&p.caller_smells, 3));
                }
            }
            if pairs.is_empty() {
                let _ = writeln!(out, "None found.");
            }
        }

        AuditMode::AmbiguousUnverifiedEdges => {
            let reachable = g.reachable_from_entrypoints();
            // One call site with several candidate callees -> one finding.
            let mut groups: HashMap<(&str, &str), Vec<&StoredEdge>> = HashMap::new();
            for e in g.edges.iter().filter(|e| e.confidence == "STATIC_HEURISTIC" && e.kind == "CALLS") {
                groups.entry((e.from.as_str(), e.snippet.as_str())).or_default().push(e);
            }
            let mut rows: Vec<(i64, &str, &str, Vec<&StoredEdge>)> = groups
                .into_iter()
                .filter(|((from, _), _)| g.symbols.get(*from).is_some_and(|s| passes(args, s)))
                .map(|((from, snippet), es)| {
                    let caller = &g.symbols[from];
                    let score = if reachable.contains(from) { 20 } else { 0 }
                        + if g.is_trust_boundary(from) { 10 } else { 0 }
                        + g.risk(from) * 2
                        + caller.complexity.min(10);
                    (score, from, snippet, es)
                })
                .collect();
            rows.sort_by(|a, b| b.0.cmp(&a.0).then(a.3.len().cmp(&b.3.len())).then((a.1, a.2).cmp(&(b.1, b.2))));
            let _ =
                writeln!(out, "# AUDIT SURFACE — AMBIGUOUS_UNVERIFIED_EDGES: {} call sites\n", rows.len().min(limit));
            let _ = writeln!(
                out,
                "Heuristic edges no Cartographer verdict has resolved. Ranked by exposure (entrypoint-reachable, trust boundary, caller risk).\n"
            );
            for (i, (score, from, snippet, mut es)) in rows.into_iter().take(limit).enumerate() {
                es.sort_by(|a, b| a.to.cmp(&b.to));
                let _ =
                    writeln!(out, "{}. `{from}` calls `{snippet}` — {} candidates · exposure {score}", i + 1, es.len());
                for e in es {
                    let sig = g.symbols.get(&e.to).map_or("", |s| s.signature.as_str());
                    let _ = writeln!(out, "   * `{}` — `{sig}`", e.to);
                }
            }
        }
    }
    Ok(out)
}

pub struct Path {
    pub nodes: Vec<String>,
    pub edges: Vec<usize>,
}

pub struct TaintResult {
    pub paths: Vec<Path>,
    pub truncated: bool,
}

/// For every source, a BFS over taint-propagating edges; each reached sink yields its shortest
/// path. Each node is expanded at most once per source, so cost is O(sources × (V + E)).
pub fn taint_paths(g: &AtlasGraph, sources: &[&String], max_hops: usize, limit: usize) -> TaintResult {
    let mut found: Vec<Path> = Vec::new();
    let mut expansions = 0usize;
    let mut truncated = false;

    'sources: for &src in sources {
        let mut parent: HashMap<&str, (&str, usize)> = HashMap::new();
        let mut depth: HashMap<&str, usize> = HashMap::from([(src.as_str(), 0)]);
        let mut queue: VecDeque<&str> = VecDeque::from([src.as_str()]);
        while let Some(n) = queue.pop_front() {
            let d = depth[n];
            if d >= max_hops {
                continue;
            }
            expansions += 1;
            if expansions > EXPANSION_BUDGET {
                truncated = true;
                break 'sources;
            }
            for &ei in g.out.get(n).into_iter().flatten() {
                let e = &g.edges[ei];
                if !e.taint || depth.contains_key(e.to.as_str()) {
                    continue;
                }
                depth.insert(e.to.as_str(), d + 1);
                parent.insert(e.to.as_str(), (n, ei));
                queue.push_back(e.to.as_str());
                if g.is_sink(&e.to) {
                    let (mut nodes, mut edges) = (vec![e.to.clone()], Vec::new());
                    let mut cur = e.to.as_str();
                    while let Some(&(p, pe)) = parent.get(cur) {
                        nodes.push(p.to_string());
                        edges.push(pe);
                        cur = p;
                    }
                    nodes.reverse();
                    edges.reverse();
                    found.push(Path { nodes, edges });
                }
            }
        }
    }

    found.sort_by(|a, b| a.edges.len().cmp(&b.edges.len()).then(a.nodes.cmp(&b.nodes)));
    if found.len() > limit {
        truncated = true;
        found.truncate(limit);
    }
    TaintResult { paths: found, truncated }
}

fn render_chain(out: &mut String, g: &AtlasGraph, sem: &HashMap<String, queries::Semantics>, p: &Path, indent: &str) {
    for (i, node) in p.nodes.iter().enumerate() {
        let risk = g.contracts.get(node).map_or(String::new(), |c| format!(" (risk {}/10)", c.risk_score));
        let _ = writeln!(out, "{indent}* `{node}`{risk} — {}", intent(sem, node));
        if let Some(&ei) = p.edges.get(i) {
            let e = &g.edges[ei];
            let lines = match e.lines.as_slice() {
                [] => String::new(),
                ls => format!(" (Lines {})", ls.iter().map(i64::to_string).collect::<Vec<_>>().join(", ")),
            };
            let _ = writeln!(
                out,
                "{indent}  ↓ `{}` `[{}]`{lines}{}{}",
                e.kind,
                e.confidence,
                if e.semantics.is_empty() { String::new() } else { format!(" {}", e.semantics) },
                if e.taint { " **[Taint Propagated: TRUE]**" } else { "" }
            );
        }
    }
}

// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TraceMode {
    /// Fewest hops over CALLS edges.
    Shortest,
    /// Paths through the riskiest contracts first.
    HighestRisk,
    /// Only edges with data_taint_propagated.
    Taint,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct TraceArgs {
    /// Start of the path. Omit to start from any trust-boundary symbol.
    pub source_symbol_id: Option<String>,
    /// End of the path. Omit to end at any symbol with sinks or state mutations.
    pub target_symbol_id: Option<String>,
    /// Defaults to SHORTEST, or TAINT when neither endpoint is given.
    pub mode: Option<TraceMode>,
    /// Default 6, max 10.
    pub max_hop_depth: Option<usize>,
    /// Number of alternative paths to return; default 3, max 10.
    pub limit: Option<usize>,
}

pub fn trace_connective_paths(db: &DbInstance, args: &TraceArgs) -> Result<String> {
    let g = AtlasGraph::load(db)?;
    for id in [&args.source_symbol_id, &args.target_symbol_id].into_iter().flatten() {
        if !g.symbols.contains_key(id) {
            bail!("unknown symbol_id `{id}`");
        }
    }
    let mode = args.mode.unwrap_or(if args.source_symbol_id.is_none() && args.target_symbol_id.is_none() {
        TraceMode::Taint
    } else {
        TraceMode::Shortest
    });
    let hops = args.max_hop_depth.unwrap_or(6).clamp(1, 10);
    let limit = args.limit.unwrap_or(3).clamp(1, 10);

    let mut boundaries: Vec<String> =
        g.contracts.iter().filter(|(_, c)| c.trust_boundary).map(|(i, _)| i.clone()).collect();
    boundaries.sort();
    let sources: Vec<String> = match &args.source_symbol_id {
        Some(s) => vec![s.clone()],
        None if boundaries.is_empty() => bail!("no symbol is marked trust_boundary yet; pass source_symbol_id"),
        None => boundaries,
    };
    let targets: HashSet<String> = match &args.target_symbol_id {
        Some(t) => HashSet::from([t.clone()]),
        None => g.symbols.keys().filter(|id| g.is_sink(id)).cloned().collect(),
    };
    if targets.is_empty() {
        bail!("no symbol records taint_sinks or state_mutations yet; pass target_symbol_id");
    }

    let accept = |e: &StoredEdge| match mode {
        TraceMode::Taint => e.taint,
        _ => e.kind == "CALLS",
    };
    let (mut paths, truncated) = enumerate_paths(&g, &sources, &targets, accept, hops);

    let risk_sum = |p: &Path| p.nodes.iter().map(|n| g.risk(n)).sum::<i64>();
    match mode {
        TraceMode::HighestRisk => paths.sort_by(|a, b| {
            risk_sum(b).cmp(&risk_sum(a)).then(a.edges.len().cmp(&b.edges.len())).then(a.nodes.cmp(&b.nodes))
        }),
        _ => paths.sort_by(|a, b| {
            a.edges.len().cmp(&b.edges.len()).then(risk_sum(b).cmp(&risk_sum(a))).then(a.nodes.cmp(&b.nodes))
        }),
    }
    let total = paths.len();
    paths.truncate(limit);

    let ids: Vec<String> = paths.iter().flat_map(|p| p.nodes.iter().cloned()).collect();
    let sem = queries::semantics_by_ids(db, &ids)?;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# CONNECTIVE PATHS — {mode:?} (≤ {hops} hops): {} of {total} found{}\n",
        paths.len(),
        if truncated { " — search truncated, more paths may exist" } else { "" }
    );
    for (i, p) in paths.iter().enumerate() {
        let _ = writeln!(out, "## Path {} — {} hops · risk Σ {}", i + 1, p.edges.len(), risk_sum(p));
        render_chain(&mut out, &g, &sem, p, "");
        out.push('\n');
    }
    if paths.is_empty() {
        let _ = writeln!(
            out,
            "No path within {hops} hops. Try a larger max_hop_depth, mode SHORTEST (taint edges are sparse), or check that edges were verified."
        );
    }
    Ok(out)
}

/// All simple paths from any source to any target within `max_hops`, pruned by backward
/// distance so every explored branch can still reach a target. Bounded by `PATH_CAP` paths and
/// `EXPANSION_BUDGET` expansions.
fn enumerate_paths(
    g: &AtlasGraph,
    sources: &[String],
    targets: &HashSet<String>,
    accept: impl Fn(&StoredEdge) -> bool,
    max_hops: usize,
) -> (Vec<Path>, bool) {
    // dist[n] = fewest accepted hops from n to any target
    let mut dist: HashMap<&str, usize> = targets.iter().map(|t| (t.as_str(), 0)).collect();
    let mut queue: VecDeque<&str> = targets.iter().map(String::as_str).collect();
    while let Some(n) = queue.pop_front() {
        let d = dist[n];
        for &ei in g.inn.get(n).into_iter().flatten() {
            let e = &g.edges[ei];
            if accept(e) && !dist.contains_key(e.from.as_str()) {
                dist.insert(e.from.as_str(), d + 1);
                queue.push_back(e.from.as_str());
            }
        }
    }

    let mut paths = Vec::new();
    let (mut expansions, mut truncated) = (0usize, false);
    'sources: for src in sources {
        if !dist.contains_key(src.as_str()) {
            continue;
        }
        // iterative DFS: stack of (node, next-edge cursor)
        let mut nodes = vec![src.as_str()];
        let mut edges: Vec<usize> = Vec::new();
        let mut cursors = vec![0usize];
        while let Some(&n) = nodes.last() {
            let cur = cursors.last_mut().expect("cursor per node");
            let adj = g.out.get(n).map_or(&[][..], Vec::as_slice);
            if *cur >= adj.len() {
                nodes.pop();
                cursors.pop();
                edges.pop();
                continue;
            }
            let ei = adj[*cur];
            *cur += 1;
            let e = &g.edges[ei];
            if !accept(e) || nodes.contains(&e.to.as_str()) {
                continue;
            }
            let used = edges.len() + 1;
            let Some(&rest) = dist.get(e.to.as_str()) else { continue };
            if used + rest > max_hops && !targets.contains(&e.to) {
                continue;
            }
            expansions += 1;
            if expansions > EXPANSION_BUDGET || paths.len() >= PATH_CAP {
                truncated = true;
                break 'sources;
            }
            if targets.contains(&e.to) {
                if used <= max_hops {
                    let mut ns: Vec<String> = nodes.iter().map(|s| s.to_string()).collect();
                    ns.push(e.to.clone());
                    let mut es = edges.clone();
                    es.push(ei);
                    paths.push(Path { nodes: ns, edges: es });
                }
                continue; // stop at the first target on a branch
            }
            nodes.push(e.to.as_str());
            cursors.push(0);
            edges.push(ei);
        }
    }
    (paths, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinator::embeddings::HashEmbedder;
    use crate::mcp::batch::tests::repo;
    use crate::mcp::submit::*;

    fn id(db: &DbInstance, name: &str) -> String {
        queries::all_symbols(db).unwrap().into_iter().find(|s| s.name == name).unwrap().symbol_id
    }

    fn contract(
        risk: i64,
        boundary: bool,
        assumed: &[&str],
        enforced: &[&str],
        sinks: &[&str],
        muts: &[&str],
    ) -> StrictContractInput {
        let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        StrictContractInput {
            invariants_assumed: v(assumed),
            invariants_enforced: v(enforced),
            state_mutations: v(muts),
            error_surface: v(&["fails"]),
            concurrency_assumptions: vec![],
            trust_boundary: boundary,
            taint_sources: vec![],
            taint_sinks: v(sinks),
            audit_smells: v(&["smell"]),
            risk_score: risk,
        }
    }

    fn edge(to: &str, taint: bool) -> ResolvedEdge {
        ResolvedEdge {
            to_symbol_id: to.into(),
            edge_kind: EdgeKind::Calls,
            resolution: Resolution::LlmVerified,
            edge_semantics: format!("passes data{}", if taint { " (tainted)" } else { "" }),
            data_taint_propagated: taint,
        }
    }

    fn model(id: &str, c: Option<StrictContractInput>, edges: Vec<ResolvedEdge>) -> SymbolModel {
        SymbolModel {
            symbol_id: id.into(),
            intent_summary: format!("intent of {}", id.rsplit("::").next().unwrap()),
            mechanism_prose: "m".into(),
            domain_tags: vec![],
            promote_to_strict_contract: c.is_some(),
            strict_contract: c,
            resolved_edges: edges,
        }
    }

    fn submit(db: &DbInstance, models: Vec<SymbolModel>) {
        if let Err(SubmitError::Invalid(e)) = submit_symbol_models(db, &HashEmbedder, SubmitArgs { models }) {
            panic!("{e:?}");
        }
    }

    const APP: &str = "fn handle() { parse(); decoy(); }\nfn parse() { build_query(); }\nfn build_query() { db_exec(); }\n\
                       fn db_exec() {}\nfn decoy() { db_exec(); }\n";

    fn app() -> (tempfile::TempDir, DbInstance) {
        let (d, db) = repo(&[("src/app.rs", APP)]);
        let (h, p, b, x, dc) =
            (id(&db, "handle"), id(&db, "parse"), id(&db, "build_query"), id(&db, "db_exec"), id(&db, "decoy"));
        submit(
            &db,
            vec![
                model(&h, Some(contract(7, true, &[], &[], &[], &[])), vec![edge(&p, true), edge(&dc, false)]),
                model(&p, Some(contract(5, false, &["input is utf8"], &[], &[], &[])), vec![edge(&b, true)]),
                model(&b, Some(contract(6, false, &["no quotes"], &["length check"], &[], &[])), vec![edge(&x, true)]),
                model(&x, Some(contract(9, false, &["query is trusted"], &[], &["raw sql"], &["writes rows"])), vec![]),
                model(&dc, None, vec![edge(&x, false)]),
            ],
        );
        (d, db)
    }

    fn audit(db: &DbInstance, mode: AuditMode, f: impl FnOnce(&mut AuditArgs)) -> String {
        let mut a = AuditArgs { query_mode: Some(mode), ..Default::default() };
        f(&mut a);
        query_audit_surface(db, &a).unwrap()
    }

    #[test]
    fn taint_path_follows_only_tainted_edges_and_reports_sink_details() {
        let (_d, db) = app();
        let md = audit(&db, AuditMode::TrustBoundaryToSinkPaths, |_| {});
        assert!(md.contains("1 paths from 1 trust boundaries"), "{md}");
        assert!(md.contains("**3 hops**"));
        for hop in ["::handle#", "::parse#", "::build_query#", "::db_exec#"] {
            assert!(md.contains(hop), "{hop} missing:\n{md}");
        }
        assert!(!md.contains("::decoy#"), "untainted edge must not be followed");
        assert!(md.contains("sink operations: raw sql") && md.contains("state mutations: writes rows"));
        assert!(md.contains("**[Taint Propagated: TRUE]**"));

        let short = audit(&db, AuditMode::TrustBoundaryToSinkPaths, |a| a.max_hop_depth = Some(2));
        assert!(short.contains("0 paths"));
    }

    #[test]
    fn high_risk_contracts_filter_and_rank() {
        let (_d, db) = app();
        let md = audit(&db, AuditMode::HighRiskContracts, |_| {});
        let order: Vec<usize> =
            ["::db_exec#", "::handle#", "::build_query#"].iter().map(|n| md.find(n).unwrap()).collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{md}"); // 9, 7, 6
        assert!(!md.contains("::parse#")); // risk 5 < default 6
        let low = audit(&db, AuditMode::HighRiskContracts, |a| a.min_risk_score = Some(1));
        assert!(low.contains("::parse#"));
        let scoped = audit(&db, AuditMode::HighRiskContracts, |a| a.scope_path_prefix = Some("src/other".into()));
        assert!(scoped.contains("0 of 0"));
        let flagged = audit(&db, AuditMode::HighRiskContracts, |a| {
            a.required_risk_flags = Some(vec!["unsafe_usage".into()]);
            a.min_risk_score = Some(1)
        });
        assert!(flagged.contains("0 of 0"));
    }

    #[test]
    fn unchecked_assumptions_rank_unenforcing_tainted_callers_first() {
        let (_d, db) = app();
        let md = audit(&db, AuditMode::UncheckedAssumptionHotspots, |a| a.min_risk_score = Some(5));
        // db_exec(9) is called by build_query (tainted, enforces a length check) -> first by callee risk
        let first_pair = md.lines().find(|l| l.starts_with("1. ")).unwrap();
        assert!(first_pair.contains("::build_query#") && first_pair.contains("::db_exec#"), "{md}");
        assert!(md.contains("callee assumes: query is trusted"));
        assert!(md.contains("caller enforces: length check"));
        assert!(md.contains("caller enforces: nothing recorded"));
    }

    #[test]
    fn ambiguous_edges_group_by_call_site() {
        let src = "struct A; struct B;\nimpl A { fn run(&self) {} }\nimpl B { fn run(&self) {} }\nfn caller(x: A) { x.run(); }\nfn main() { caller(A); }";
        let (_d, db) = repo(&[("src/lib.rs", src)]);
        let md = audit(&db, AuditMode::AmbiguousUnverifiedEdges, |_| {});
        assert!(md.contains("1 call sites"), "{md}");
        assert!(md.contains("2 candidates"));
        assert!(md.contains("::A::run#") && md.contains("::B::run#"));
    }

    #[test]
    fn trace_modes_pick_different_paths_in_a_diamond() {
        let src = "fn top() { safe(); risky(); }\nfn safe() { sink(); }\nfn risky() { mid(); }\nfn mid() { sink(); }\nfn sink() {}\n";
        let (_d, db) = repo(&[("src/d.rs", src)]);
        let (t, s, r, m, k) = (id(&db, "top"), id(&db, "safe"), id(&db, "risky"), id(&db, "mid"), id(&db, "sink"));
        submit(
            &db,
            vec![
                model(&t, None, vec![]),
                model(&s, Some(contract(1, false, &["a"], &[], &[], &[])), vec![]),
                model(&r, Some(contract(9, false, &["a"], &[], &[], &[])), vec![]),
                model(&m, Some(contract(8, false, &["a"], &[], &[], &[])), vec![]),
                model(&k, Some(contract(2, false, &["a"], &[], &["x"], &[])), vec![]),
            ],
        );
        let trace = |mode| {
            trace_connective_paths(
                &db,
                &TraceArgs {
                    source_symbol_id: Some(t.clone()),
                    target_symbol_id: Some(k.clone()),
                    mode: Some(mode),
                    limit: Some(1),
                    ..Default::default()
                },
            )
            .unwrap()
        };
        let shortest = trace(TraceMode::Shortest);
        assert!(
            shortest.contains("2 hops") && shortest.contains("::safe#") && !shortest.contains("::risky#"),
            "{shortest}"
        );
        let risky = trace(TraceMode::HighestRisk);
        assert!(risky.contains("3 hops") && risky.contains("::risky#") && risky.contains("::mid#"), "{risky}");

        let all = trace_connective_paths(
            &db,
            &TraceArgs {
                source_symbol_id: Some(t.clone()),
                target_symbol_id: Some(k.clone()),
                limit: Some(10),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(all.contains("2 of 2 found"));
        // endpoints omitted and nothing tainted: explicit message, not a crash
        let none = trace_connective_paths(&db, &TraceArgs::default()).unwrap_err().to_string();
        assert!(none.contains("trust_boundary"), "{none}");
        assert!(
            trace_connective_paths(&db, &TraceArgs { source_symbol_id: Some("nope".into()), ..Default::default() })
                .unwrap_err()
                .to_string()
                .contains("unknown")
        );
    }

    #[test]
    fn trace_from_trust_boundaries_defaults_to_taint_mode() {
        let (_d, db) = app();
        let md = trace_connective_paths(&db, &TraceArgs::default()).unwrap();
        assert!(md.contains("Taint") && md.contains("1 of 1 found") && md.contains("::db_exec#"), "{md}");
    }

    // ---- bounded traversal on a graph with ~16 million simple paths ------------------------

    fn synthetic(layers: usize, width: usize) -> AtlasGraph {
        let sym = |id: &str| SymbolInfo {
            symbol_id: id.into(),
            qualified_name: id.into(),
            name: id.into(),
            kind: "fn".into(),
            visibility: "pub".into(),
            file_path: "x.rs".into(),
            start_byte: 0,
            end_byte: 0,
            start_line: 1,
            end_line: 1,
            signature: String::new(),
            doc_comment: String::new(),
            ast_blake3: String::new(),
            complexity: 1,
            risk_flags: vec![],
            requires_strict_contract: false,
            scc_group_id: 0,
            topological_level: 0,
            mapping_status: "UNMAPPED".into(),
            is_entrypoint: false,
        };
        let name = |l: usize, i: usize| format!("n{l}_{i}");
        let mut symbols = HashMap::new();
        let mut edges = Vec::new();
        for l in 0..layers {
            for i in 0..width {
                symbols.insert(name(l, i), sym(&name(l, i)));
                if l + 1 < layers {
                    for j in 0..width {
                        edges.push(StoredEdge {
                            from: name(l, i),
                            to: name(l + 1, j),
                            kind: "CALLS".into(),
                            confidence: "STATIC_EXACT".into(),
                            lines: vec![],
                            snippet: String::new(),
                            semantics: String::new(),
                            taint: true,
                        });
                    }
                }
            }
        }
        symbols.insert("src".into(), sym("src"));
        symbols.insert("sink".into(), sym("sink"));
        for i in 0..width {
            edges.push(StoredEdge {
                from: "src".into(),
                to: name(0, i),
                kind: "CALLS".into(),
                confidence: "STATIC_EXACT".into(),
                lines: vec![],
                snippet: String::new(),
                semantics: String::new(),
                taint: true,
            });
            edges.push(StoredEdge {
                from: name(layers - 1, i),
                to: "sink".into(),
                kind: "CALLS".into(),
                confidence: "STATIC_EXACT".into(),
                lines: vec![],
                snippet: String::new(),
                semantics: String::new(),
                taint: true,
            });
        }
        let mut contracts = HashMap::new();
        contracts.insert("sink".to_string(), queries::Contract { taint_sinks: vec!["x".into()], ..Default::default() });
        contracts.insert("src".to_string(), queries::Contract { trust_boundary: true, ..Default::default() });
        let (mut out, mut inn): (HashMap<String, Vec<usize>>, HashMap<String, Vec<usize>>) = Default::default();
        for (i, e) in edges.iter().enumerate() {
            out.entry(e.from.clone()).or_default().push(i);
            inn.entry(e.to.clone()).or_default().push(i);
        }
        AtlasGraph { symbols, contracts, edges, out, inn }
    }

    #[test]
    fn dense_layered_graph_cannot_blow_up_either_traversal() {
        let g = synthetic(12, 4); // 4^12 ≈ 16.7M simple src->sink paths
        let t0 = std::time::Instant::now();

        let src = "src".to_string();
        let taint = taint_paths(&g, &[&src], 8, 20);
        assert_eq!(taint.paths.len(), 0, "sink is 13 hops away, beyond max 8");
        let taint = taint_paths(&g, &[&src], 8, 20);
        assert!(taint.paths.is_empty());

        let g2 = synthetic(6, 4);
        let taint = taint_paths(&g2, &[&src], 8, 20);
        assert_eq!(taint.paths.len(), 1); // one shortest path per (source, sink)
        assert_eq!(taint.paths[0].edges.len(), 7);

        let (paths, truncated) =
            enumerate_paths(&g, &["src".to_string()], &HashSet::from(["sink".to_string()]), |_| true, 14);
        assert!(truncated, "path cap must trip");
        assert_eq!(paths.len(), PATH_CAP);
        assert!(t0.elapsed().as_secs() < 5, "took {:?}", t0.elapsed());
    }
}
