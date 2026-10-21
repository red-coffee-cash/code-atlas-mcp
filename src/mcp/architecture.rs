//! Tier 3 writers: `submit_module_rollup` and `record_execution_flow` (spec §3.2, §7.1).

use std::collections::HashSet;

use anyhow::Result;
use cozo::DbInstance;
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};

use crate::coordinator::embeddings::Embedder;
use crate::db::mutations::{self, FlowRow, RollupRow};
use crate::db::queries::{self, ValidationError};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RollupArgs {
    /// Directory or file path relative to the repo root, e.g. `src/auth`.
    pub module_path: String,
    /// Defaults to the enclosing directory.
    pub parent_module_path: Option<String>,
    pub architectural_role: String,
    pub key_abstractions: Vec<String>,
    pub cross_module_contracts: Vec<String>,
    pub known_architectural_risks: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct RollupReport {
    pub module_path: String,
    pub parent_module_path: String,
    pub symbols_in_module: usize,
    /// fn/method symbols under the module that are not mapped yet; the rollup was still saved.
    pub unmapped_symbols_in_module: usize,
}

fn normalize_path(p: &str) -> String {
    p.trim().trim_start_matches("./").trim_matches('/').to_string()
}

fn invalid(field: &str, message: impl Into<String>) -> Vec<ValidationError> {
    vec![ValidationError { symbol_id: String::new(), field: field.to_string(), message: message.into() }]
}

pub fn submit_module_rollup(
    db: &DbInstance,
    embedder: &dyn Embedder,
    args: RollupArgs,
) -> Result<std::result::Result<RollupReport, Vec<ValidationError>>> {
    let module_path = normalize_path(&args.module_path);
    if module_path.is_empty() {
        return Ok(Err(invalid("module_path", "must be a non-empty path relative to the repo root")));
    }
    if args.architectural_role.trim().is_empty() {
        return Ok(Err(invalid("architectural_role", "must not be empty")));
    }
    let in_module = |p: &str| p == module_path || p.starts_with(&format!("{module_path}/"));
    let symbols: Vec<_> = queries::all_symbols(db)?.into_iter().filter(|s| in_module(&s.file_path)).collect();
    if symbols.is_empty() && !queries::file_hashes(db)?.keys().any(|p| in_module(p)) {
        return Ok(Err(invalid(
            "module_path",
            format!("no indexed files under `{module_path}`; sync first or fix the path"),
        )));
    }

    let parent = match args.parent_module_path {
        Some(p) => normalize_path(&p),
        None => module_path.rsplit_once('/').map_or(String::new(), |(d, _)| d.to_string()),
    };
    let text = format!(
        "{module_path}\n{}\n{}\n{}\n{}",
        args.architectural_role,
        args.key_abstractions.join("; "),
        args.cross_module_contracts.join("; "),
        args.known_architectural_risks.join("; ")
    );
    let embedding = embedder.embed(&[text])?.remove(0);

    mutations::write_module_rollup(
        db,
        &RollupRow {
            module_path: module_path.clone(),
            parent_module_path: parent.clone(),
            architectural_role: args.architectural_role,
            key_abstractions: args.key_abstractions,
            cross_module_contracts: args.cross_module_contracts,
            known_architectural_risks: args.known_architectural_risks,
            embedding,
        },
    )?;
    Ok(Ok(RollupReport {
        module_path,
        parent_module_path: parent,
        symbols_in_module: symbols.len(),
        unmapped_symbols_in_module: symbols.iter().filter(|s| s.is_callable() && !s.is_mapped()).count(),
    }))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FlowArgs {
    /// e.g. `flow::checkout_payment_settlement` (the `flow::` prefix is added if missing).
    pub flow_id: String,
    pub name: String,
    pub entrypoint_symbol_ids: Vec<String>,
    pub terminal_sink_symbol_ids: Vec<String>,
    /// End-to-end data transformation and control narrative.
    pub narrative_summary: String,
    /// How state mutates from start to finish.
    pub state_lifecycle_summary: String,
    pub security_boundaries_crossed: Vec<String>,
    /// Ordered steps; the first is step 1.
    pub steps: Vec<FlowStepInput>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FlowStepInput {
    pub symbol_id: String,
    pub role_in_flow: String,
    pub critical_assumptions_at_step: String,
}

#[derive(Debug, Serialize)]
pub struct FlowReport {
    pub flow_id: String,
    pub steps: usize,
    pub replaced_existing: bool,
}

pub fn record_execution_flow(
    db: &DbInstance,
    embedder: &dyn Embedder,
    args: FlowArgs,
) -> Result<std::result::Result<FlowReport, Vec<ValidationError>>> {
    let mut errs = Vec::new();
    let flow_id = {
        let id = args.flow_id.trim();
        if id.starts_with("flow::") { id.to_string() } else { format!("flow::{id}") }
    };
    if flow_id == "flow::" {
        errs.push(ValidationError {
            symbol_id: String::new(),
            field: "flow_id".into(),
            message: "must not be empty".into(),
        });
    }
    if args.steps.is_empty() {
        errs.push(ValidationError {
            symbol_id: String::new(),
            field: "steps".into(),
            message: "at least one step is required".into(),
        });
    }
    if args.narrative_summary.trim().is_empty() {
        errs.push(ValidationError {
            symbol_id: String::new(),
            field: "narrative_summary".into(),
            message: "must not be empty".into(),
        });
    }

    let mut referenced: Vec<String> = args.steps.iter().map(|s| s.symbol_id.clone()).collect();
    referenced.extend(args.entrypoint_symbol_ids.iter().cloned());
    referenced.extend(args.terminal_sink_symbol_ids.iter().cloned());
    let known = queries::symbols_by_ids(db, &referenced)?;
    let mut seen = HashSet::new();
    for (i, s) in args.steps.iter().enumerate() {
        if !seen.insert(s.symbol_id.as_str()) {
            errs.push(ValidationError {
                symbol_id: s.symbol_id.clone(),
                field: format!("steps[{i}]"),
                message: "a symbol may appear at most once per flow".into(),
            });
        }
    }
    let mut unknown: Vec<&String> = referenced.iter().filter(|id| !known.contains_key(*id)).collect();
    unknown.sort();
    unknown.dedup();
    for id in unknown {
        errs.push(ValidationError {
            symbol_id: id.clone(),
            field: "symbol_id".into(),
            message: "unknown symbol_id".into(),
        });
    }
    if !errs.is_empty() {
        return Ok(Err(errs));
    }

    let replaced_existing = !queries::rows_with(
        db,
        "?[f] := f = $f, *execution_flow{flow_id: f}",
        std::iter::once(("f".to_string(), cozo::DataValue::from(flow_id.as_str()))).collect(),
    )?
    .is_empty();

    let text = format!("{}\n{}\n{}", args.name, args.narrative_summary, args.state_lifecycle_summary);
    let embedding = embedder.embed(&[text])?.remove(0);
    let steps = args
        .steps
        .into_iter()
        .map(|s| (s.symbol_id, s.role_in_flow, s.critical_assumptions_at_step))
        .collect::<Vec<_>>();
    let step_count = steps.len();

    mutations::write_execution_flow(
        db,
        &FlowRow {
            flow_id: flow_id.clone(),
            name: args.name,
            entrypoint_symbol_ids: args.entrypoint_symbol_ids,
            terminal_sink_symbol_ids: args.terminal_sink_symbol_ids,
            narrative_summary: args.narrative_summary,
            state_lifecycle_summary: args.state_lifecycle_summary,
            security_boundaries_crossed: args.security_boundaries_crossed,
            embedding,
            steps,
        },
    )?;
    Ok(Ok(FlowReport { flow_id, steps: step_count, replaced_existing }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinator::embeddings::HashEmbedder;
    use crate::mcp::batch::tests::repo;

    fn id(db: &DbInstance, name: &str) -> String {
        queries::all_symbols(db).unwrap().into_iter().find(|s| s.name == name).unwrap().symbol_id
    }

    fn rollup(path: &str) -> RollupArgs {
        RollupArgs {
            module_path: path.into(),
            parent_module_path: None,
            architectural_role: "session authentication".into(),
            key_abstractions: vec!["JwtService".into()],
            cross_module_contracts: vec!["callers strip Bearer".into()],
            known_architectural_risks: vec![],
        }
    }

    #[test]
    fn rollup_validates_path_reports_unmapped_and_upserts() {
        let (_d, db) = repo(&[("src/auth/jwt.rs", "fn verify() {}"), ("src/other.rs", "fn x() {}")]);
        let bad = submit_module_rollup(&db, &HashEmbedder, rollup("src/missing")).unwrap();
        assert!(bad.is_err());

        let ok = submit_module_rollup(&db, &HashEmbedder, rollup("./src/auth/")).unwrap().ok().unwrap();
        assert_eq!(ok.module_path, "src/auth");
        assert_eq!(ok.parent_module_path, "src");
        assert_eq!((ok.symbols_in_module, ok.unmapped_symbols_in_module), (1, 1));

        submit_module_rollup(&db, &HashEmbedder, RollupArgs { architectural_role: "v2".into(), ..rollup("src/auth") })
            .unwrap()
            .ok()
            .unwrap();
        let rows = queries::rows(&db, "?[r] := *module_rollup{architectural_role: r}").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].get_str(), Some("v2"));
    }

    fn flow(db: &DbInstance, steps: &[&str]) -> FlowArgs {
        FlowArgs {
            flow_id: "checkout".into(),
            name: "Checkout".into(),
            entrypoint_symbol_ids: vec![id(db, steps[0])],
            terminal_sink_symbol_ids: vec![id(db, steps[steps.len() - 1])],
            narrative_summary: "request flows to the charge".into(),
            state_lifecycle_summary: "order goes pending to paid".into(),
            security_boundaries_crossed: vec!["http->db".into()],
            steps: steps
                .iter()
                .map(|n| FlowStepInput {
                    symbol_id: id(db, n),
                    role_in_flow: format!("{n} role"),
                    critical_assumptions_at_step: String::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn flow_records_ordered_steps_prefixes_id_and_replaces_on_rerecord() {
        let (_d, db) = repo(&[("src/lib.rs", "fn handler() {}\nfn charge() {}\nfn persist() {}")]);
        let r = record_execution_flow(&db, &HashEmbedder, flow(&db, &["handler", "charge", "persist"]))
            .unwrap()
            .ok()
            .unwrap();
        assert_eq!((r.flow_id.as_str(), r.steps, r.replaced_existing), ("flow::checkout", 3, false));
        let rows = queries::rows(
            &db,
            "?[o, s] := *flow_step{flow_id: 'flow::checkout', step_order: o, symbol_id: s} :order o",
        )
        .unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows[0][1].get_str().unwrap().contains("::handler#"));
        assert!(rows[2][1].get_str().unwrap().contains("::persist#"));

        let r = record_execution_flow(&db, &HashEmbedder, flow(&db, &["handler", "persist"])).unwrap().ok().unwrap();
        assert!(r.replaced_existing);
        assert_eq!(queries::rows(&db, "?[s] := *flow_step{flow_id: 'flow::checkout', symbol_id: s}").unwrap().len(), 2);
    }

    #[test]
    fn flow_rejects_unknown_duplicate_and_empty() {
        let (_d, db) = repo(&[("src/lib.rs", "fn a() {}")]);
        let mut f = flow(&db, &["a"]);
        f.steps.push(FlowStepInput {
            symbol_id: id(&db, "a"),
            role_in_flow: "again".into(),
            critical_assumptions_at_step: String::new(),
        });
        f.steps.push(FlowStepInput {
            symbol_id: "ghost".into(),
            role_in_flow: "x".into(),
            critical_assumptions_at_step: String::new(),
        });
        let errs = record_execution_flow(&db, &HashEmbedder, f).unwrap().err().unwrap();
        assert!(errs.iter().any(|e| e.message.contains("at most once")));
        assert!(errs.iter().any(|e| e.symbol_id == "ghost"));

        let empty = FlowArgs { steps: vec![], narrative_summary: " ".into(), ..flow(&db, &["a"]) };
        let errs = record_execution_flow(&db, &HashEmbedder, empty).unwrap().err().unwrap();
        assert!(errs.iter().any(|e| e.field == "steps") && errs.iter().any(|e| e.field == "narrative_summary"));
    }
}
