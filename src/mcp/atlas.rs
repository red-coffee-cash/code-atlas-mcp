//! `get_macro_atlas` (spec §7.2): the Tier 3 big picture plus mapping coverage.

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;

use anyhow::Result;
use cozo::DbInstance;
use rmcp::schemars::{self, JsonSchema};
use serde::Deserialize;

use crate::coordinator::graph::AtlasGraph;
use crate::db::queries;

const DEFAULT_ENTRYPOINTS: usize = 25;
const FLOWS_SHOWN: usize = 25;
const READY_FOR_ROLLUP_SHOWN: usize = 10;

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct AtlasArgs {
    /// Restrict coverage, entrypoints and rollups to this path prefix.
    pub scope_path_prefix: Option<String>,
    /// Entrypoints to list; default 25.
    pub max_entrypoints: Option<usize>,
}

pub fn macro_atlas(db: &DbInstance, args: &AtlasArgs) -> Result<String> {
    let g = AtlasGraph::load(db)?;
    let in_scope = |path: &str| args.scope_path_prefix.as_deref().is_none_or(|p| path.starts_with(p));
    let all_in_scope: Vec<_> = g.symbols.values().filter(|s| in_scope(&s.file_path)).collect();
    let tests_excluded = all_in_scope.iter().filter(|s| s.is_callable() && s.is_test()).count();
    let symbols: Vec<_> = all_in_scope.into_iter().filter(|s| !s.is_test()).collect();
    let callable: Vec<_> = symbols.iter().filter(|s| s.is_callable()).collect();

    let count = |status: &str| callable.iter().filter(|s| s.mapping_status == status).count();
    let (strict, light, stale, unmapped) =
        (count("STRICT_MAPPED"), count("LIGHTWEIGHT_MAPPED"), count("STALE"), count("UNMAPPED"));
    let mapped = strict + light;
    let pct = if callable.is_empty() { 0.0 } else { 100.0 * mapped as f64 / callable.len() as f64 };
    let heuristic = g
        .edges
        .iter()
        .filter(|e| {
            e.confidence == "STATIC_HEURISTIC" && g.symbols.get(&e.from).is_some_and(|s| in_scope(&s.file_path))
        })
        .count();
    let high_risk = g
        .contracts
        .iter()
        .filter(|(id, c)| c.risk_score >= 6 && g.symbols.get(*id).is_some_and(|s| in_scope(&s.file_path)))
        .count();

    let rollups: Vec<_> = queries::rollups(db)?.into_iter().filter(|r| in_scope(&r.module_path)).collect();
    let flow_rows = queries::rows(
        db,
        "n[flow_id, count(symbol_id)] := *flow_step{flow_id, symbol_id} \
         ?[id, name, steps, ep, sink, bounds, narrative] := *execution_flow{flow_id: id, name, entrypoint_symbol_ids: ep, \
            terminal_sink_symbol_ids: sink, security_boundaries_crossed: bounds, narrative_summary: narrative}, n[id, steps] :order id",
    )?;

    let mut out = String::new();
    let _ = writeln!(
        out,
        "# MACRO ATLAS{}\n",
        args.scope_path_prefix.as_deref().map_or(String::new(), |p| format!(" — `{p}`"))
    );
    let _ = writeln!(out, "## Mapping coverage");
    let _ = writeln!(
        out,
        "* **Callable symbols:** {} · **mapped:** {mapped} ({pct:.0}%) = {strict} strict + {light} lightweight · **stale:** {stale} · **unmapped:** {unmapped}",
        callable.len()
    );
    let _ = writeln!(
        out,
        "* **Unverified heuristic edges:** {heuristic} · **high-risk contracts (≥ 6):** {high_risk} · **module rollups:** {} · **execution flows:** {}",
        rollups.len(),
        flow_rows.len()
    );
    if tests_excluded > 0 {
        let _ = writeln!(
            out,
            "* {tests_excluded} test functions are excluded from coverage and the mapping queue (pass include_tests to get_next_cartography_batch to map them)."
        );
    }
    if unmapped + stale > 0 {
        let _ = writeln!(
            out,
            "* Mapping is incomplete: audit results cover only mapped symbols. Continue with `get_next_cartography_batch`."
        );
    }

    let _ = writeln!(out, "\n## Module rollups");
    if rollups.is_empty() {
        let _ = writeln!(out, "None recorded yet.");
    }
    let paths: HashSet<&str> = rollups.iter().map(|r| r.module_path.as_str()).collect();
    let depth = |p: &str| p.matches('/').count();
    let mut sorted: Vec<_> = rollups.iter().collect();
    sorted.sort_by(|a, b| a.module_path.cmp(&b.module_path));
    for r in sorted {
        let nested = paths.contains(r.parent_module_path.as_str()) && !r.parent_module_path.is_empty();
        let indent = "  ".repeat(if nested { depth(&r.module_path).min(4) } else { 0 });
        let _ = writeln!(out, "{indent}* **`{}`** — {}", r.module_path, r.architectural_role);
        if !r.key_abstractions.is_empty() {
            let _ = writeln!(out, "{indent}  * abstractions: {}", r.key_abstractions.join(", "));
        }
        if !r.cross_module_contracts.is_empty() {
            let _ = writeln!(out, "{indent}  * contracts: {}", r.cross_module_contracts.join("; "));
        }
        if !r.known_architectural_risks.is_empty() {
            let _ = writeln!(out, "{indent}  * risks: {}", r.known_architectural_risks.join("; "));
        }
    }

    // Directories whose callable symbols are all mapped but that have no rollup yet.
    let mut dirs: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for s in &callable {
        let dir = s.file_path.rsplit_once('/').map_or(String::new(), |(d, _)| d.to_string());
        let e = dirs.entry(dir).or_default();
        e.0 += 1;
        e.1 += usize::from(s.is_mapped());
    }
    let ready: Vec<_> = dirs
        .iter()
        .filter(|(d, (total, done))| !d.is_empty() && total == done && !paths.contains(d.as_str()))
        .take(READY_FOR_ROLLUP_SHOWN)
        .collect();
    if !ready.is_empty() {
        let _ = writeln!(
            out,
            "\n**Ready for `submit_module_rollup` (fully mapped, no rollup yet):** {}",
            ready.iter().map(|(d, (n, _))| format!("`{d}` ({n} symbols)")).collect::<Vec<_>>().join(", ")
        );
    }

    let _ = writeln!(out, "\n## Execution flows");
    if flow_rows.is_empty() {
        let _ =
            writeln!(out, "None recorded yet. Use `record_execution_flow` after mapping an entrypoint's call cone.");
    }
    let short = |id: &str| g.symbols.get(id).map_or(id.to_string(), |s| s.qualified_name.clone());
    for r in flow_rows.iter().take(FLOWS_SHOWN) {
        let s = |i: usize| r[i].get_str().unwrap_or("").to_string();
        let list = |i: usize| crate::db::as_strs(&r[i]);
        let _ = writeln!(
            out,
            "* **`{}`** {} — {} steps · {} → {}",
            s(0),
            s(1),
            r[2].get_int().unwrap_or(0),
            list(3).iter().map(|x| short(x)).collect::<Vec<_>>().join(", "),
            list(4).iter().map(|x| short(x)).collect::<Vec<_>>().join(", ")
        );
        if !list(5).is_empty() {
            let _ = writeln!(out, "  * crosses: {}", list(5).join(", "));
        }
        let _ = writeln!(out, "  * {}", s(6));
    }

    let mut entries: Vec<_> = symbols.iter().filter(|s| s.is_entrypoint).collect();
    entries.sort_by(|a, b| g.risk(&b.symbol_id).cmp(&g.risk(&a.symbol_id)).then(a.symbol_id.cmp(&b.symbol_id)));
    let cap = args.max_entrypoints.unwrap_or(DEFAULT_ENTRYPOINTS);
    let _ = writeln!(out, "\n## Entrypoints ({})", entries.len());
    for s in entries.iter().take(cap) {
        let risk = g.contracts.get(&s.symbol_id).map_or(String::new(), |c| format!(" · risk {}/10", c.risk_score));
        let _ = writeln!(out, "* `{}` — {}{risk}", s.symbol_id, s.mapping_status);
    }
    if entries.len() > cap {
        let _ = writeln!(out, "* … {} more (raise max_entrypoints)", entries.len() - cap);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinator::embeddings::HashEmbedder;
    use crate::mcp::architecture::{self, RollupArgs};
    use crate::mcp::batch::tests::repo;
    use crate::mcp::submit::*;

    #[test]
    fn coverage_entrypoints_and_rollup_hints() {
        let (_d, db) =
            repo(&[("src/auth/jwt.rs", "fn verify() {}\nfn check() {}"), ("src/main.rs", "fn main() { verify(); }")]);
        let all = queries::all_symbols(&db).unwrap();
        let id = |n: &str| all.iter().find(|s| s.name == n).unwrap().symbol_id.clone();
        let lite = |n: &str| SymbolModel {
            symbol_id: id(n),
            intent_summary: "i".into(),
            mechanism_prose: "m".into(),
            domain_tags: vec![],
            promote_to_strict_contract: false,
            strict_contract: None,
            resolved_edges: vec![],
        };
        let r = submit_symbol_models(&db, &HashEmbedder, SubmitArgs { models: vec![lite("verify"), lite("check")] });
        assert!(r.is_ok());

        let md = macro_atlas(&db, &AtlasArgs::default()).unwrap();
        assert!(md.contains("**Callable symbols:** 3 · **mapped:** 2 (67%) = 0 strict + 2 lightweight · **stale:** 0 · **unmapped:** 1"), "{md}");
        assert!(md.contains("Mapping is incomplete"));
        assert!(md.contains("Ready for `submit_module_rollup`") && md.contains("`src/auth` (2 symbols)"));
        assert!(md.contains("## Entrypoints (1)") && md.contains("src/main.rs::::main#fn"));
        assert!(md.contains("None recorded yet.")); // no rollups

        architecture::submit_module_rollup(
            &db,
            &HashEmbedder,
            RollupArgs {
                module_path: "src/auth".into(),
                parent_module_path: None,
                architectural_role: "auth".into(),
                key_abstractions: vec!["Jwt".into()],
                cross_module_contracts: vec![],
                known_architectural_risks: vec!["no rotation".into()],
            },
        )
        .unwrap()
        .ok()
        .unwrap();
        let md = macro_atlas(&db, &AtlasArgs::default()).unwrap();
        assert!(md.contains("**`src/auth`** — auth") && md.contains("risks: no rotation"));
        assert!(!md.contains("Ready for `submit_module_rollup`"));

        let scoped =
            macro_atlas(&db, &AtlasArgs { scope_path_prefix: Some("src/auth".into()), ..Default::default() }).unwrap();
        assert!(
            scoped.contains("**Callable symbols:** 2 · **mapped:** 2 (100%)") && scoped.contains("## Entrypoints (0)")
        );
    }
}
