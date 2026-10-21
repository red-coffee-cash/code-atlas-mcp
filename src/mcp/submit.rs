//! `submit_symbol_models` (spec §7.1, §10.1, §10.4): validate, embed outside the transaction,
//! then write semantics, contracts and edge verdicts atomically.

use std::collections::{HashMap, HashSet};

use anyhow::anyhow;
use cozo::DbInstance;
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};

use crate::coordinator::embeddings::Embedder;
use crate::coordinator::scc;
use crate::db::mutations::{self, EdgeRow, ModelsWrite, SemanticsRow};
use crate::db::queries::{self, Contract, StoredEdge, SymbolInfo};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SubmitArgs {
    pub models: Vec<SymbolModel>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SymbolModel {
    pub symbol_id: String,
    /// 1-2 sentence behavioral truth of what this code actually does.
    pub intent_summary: String,
    /// Detailed explanation of internal mechanics, control flow, and how it interacts with neighbors.
    pub mechanism_prose: String,
    pub domain_tags: Vec<String>,
    #[serde(default)]
    pub promote_to_strict_contract: bool,
    /// Required if the symbol has requires_strict_contract=true or promote_to_strict_contract=true.
    pub strict_contract: Option<StrictContractInput>,
    pub resolved_edges: Vec<ResolvedEdge>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StrictContractInput {
    /// Unchecked assumptions made about inputs, state, or caller behavior.
    pub invariants_assumed: Vec<String>,
    /// Explicit validations/guards enforced by this code.
    pub invariants_enforced: Vec<String>,
    pub state_mutations: Vec<String>,
    pub error_surface: Vec<String>,
    #[serde(default)]
    pub concurrency_assumptions: Vec<String>,
    pub trust_boundary: bool,
    #[serde(default)]
    pub taint_sources: Vec<String>,
    #[serde(default)]
    pub taint_sinks: Vec<String>,
    /// Concrete bug hypotheses, edge-case blindspots, or invariant mismatches.
    pub audit_smells: Vec<String>,
    /// 1 (benign) to 10 (severe/fragile).
    pub risk_score: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EdgeKind {
    Calls,
    PassesType,
    ReturnsType,
    MutatesState,
    ReadsState,
    Implements,
}

impl EdgeKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Calls => "CALLS",
            Self::PassesType => "PASSES_TYPE",
            Self::ReturnsType => "RETURNS_TYPE",
            Self::MutatesState => "MUTATES_STATE",
            Self::ReadsState => "READS_STATE",
            Self::Implements => "IMPLEMENTS",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Resolution {
    LlmVerified,
    LlmPruned,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ResolvedEdge {
    pub to_symbol_id: String,
    pub edge_kind: EdgeKind,
    pub resolution: Resolution,
    /// Why this connection exists and what data/state crosses it.
    pub edge_semantics: String,
    #[serde(default)]
    pub data_taint_propagated: bool,
}

pub use crate::db::queries::ValidationError;

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct UnresolvedEdge {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Serialize, Default)]
pub struct SubmitReport {
    pub accepted: usize,
    pub strict_contracts_written: usize,
    pub edges_verified: usize,
    pub edges_pruned: usize,
    pub edges_created: usize,
    /// Heuristic edges leaving the submitted symbols that no verdict covered.
    pub unresolved_heuristic_edges: Vec<UnresolvedEdge>,
    pub topology_recomputed: bool,
    pub embedder: String,
}

pub enum SubmitError {
    /// Nothing was written; fix and resubmit the whole payload.
    Invalid(Vec<ValidationError>),
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for SubmitError {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(e)
    }
}

fn why_strict(s: &SymbolInfo, promoted: bool) -> String {
    let mut reasons = Vec::new();
    if s.complexity >= 5 {
        reasons.push(format!("cyclomatic complexity {}", s.complexity));
    }
    if !s.risk_flags.is_empty() {
        reasons.push(format!("risk_flags {:?}", s.risk_flags));
    }
    if promoted {
        reasons.push("promote_to_strict_contract was set".to_string());
    }
    if reasons.is_empty() {
        reasons.push("external entrypoint".to_string());
    }
    reasons.join(", ")
}

fn validate(
    args: &SubmitArgs,
    symbols: &HashMap<String, SymbolInfo>,
    existing: &HashMap<(String, String, String), StoredEdge>,
) -> Vec<ValidationError> {
    let mut errs = Vec::new();
    let mut seen = HashSet::new();
    let mut err = |id: &str, field: &str, msg: String| {
        errs.push(ValidationError { symbol_id: id.to_string(), field: field.to_string(), message: msg })
    };

    if args.models.is_empty() {
        err("", "models", "at least one model is required".into());
    }
    for m in &args.models {
        let id = m.symbol_id.as_str();
        if !seen.insert(id) {
            err(id, "symbol_id", "appears more than once in this submission".into());
            continue;
        }
        let Some(sym) = symbols.get(id) else {
            err(id, "symbol_id", "unknown symbol_id; use the exact ids from get_next_cartography_batch".into());
            continue;
        };
        if m.intent_summary.trim().is_empty() {
            err(id, "intent_summary", "must not be empty".into());
        }
        if m.mechanism_prose.trim().is_empty() {
            err(id, "mechanism_prose", "must not be empty".into());
        }

        let needs_contract = sym.requires_strict_contract || m.promote_to_strict_contract;
        match &m.strict_contract {
            None if needs_contract => err(
                id,
                "strict_contract",
                format!("required for this symbol ({})", why_strict(sym, m.promote_to_strict_contract)),
            ),
            Some(c) => {
                if !(1..=10).contains(&c.risk_score) {
                    err(id, "strict_contract.risk_score", format!("{} is outside 1..=10", c.risk_score));
                }
                if !sym.risk_flags.is_empty() && c.invariants_assumed.is_empty() && c.error_surface.is_empty() {
                    err(
                        id,
                        "strict_contract",
                        format!(
                            "static analysis flagged {:?} but both invariants_assumed and error_surface are empty; \
                             account for each flagged hazard (e.g. the panic path of an unwrap, the preconditions an \
                             unsafe block relies on)",
                            sym.risk_flags
                        ),
                    );
                }
            }
            None => {}
        }

        for (i, e) in m.resolved_edges.iter().enumerate() {
            let field = format!("resolved_edges[{i}]");
            if !symbols.contains_key(&e.to_symbol_id) {
                err(id, &field, format!("unknown to_symbol_id `{}`", e.to_symbol_id));
                continue;
            }
            let key = (m.symbol_id.clone(), e.to_symbol_id.clone(), e.edge_kind.as_str().to_string());
            if e.resolution == Resolution::LlmPruned && !existing.contains_key(&key) {
                err(
                    id,
                    &field,
                    format!("cannot prune {} -> {}: no such {} edge exists", id, e.to_symbol_id, e.edge_kind.as_str()),
                );
            }
        }
    }
    errs
}

pub fn submit_symbol_models(
    db: &DbInstance,
    embedder: &dyn Embedder,
    args: SubmitArgs,
) -> Result<SubmitReport, SubmitError> {
    let ids: Vec<String> = args.models.iter().map(|m| m.symbol_id.clone()).collect();
    let mut lookup_ids = ids.clone();
    lookup_ids.extend(args.models.iter().flat_map(|m| m.resolved_edges.iter().map(|e| e.to_symbol_id.clone())));
    let symbols = queries::symbols_by_ids(db, &lookup_ids)?;
    let existing_edges = queries::edges_from(db, &ids)?;
    let existing: HashMap<(String, String, String), StoredEdge> =
        existing_edges.iter().cloned().map(|e| ((e.from.clone(), e.to.clone(), e.kind.clone()), e)).collect();

    let errors = validate(&args, &symbols, &existing);
    if !errors.is_empty() {
        return Err(SubmitError::Invalid(errors));
    }

    // Embeddings first, outside any write transaction (spec §10.4).
    let texts: Vec<String> = args
        .models
        .iter()
        .map(|m| format!("{}\n{}\n{}", symbols[&m.symbol_id].signature, m.intent_summary, m.mechanism_prose))
        .collect();
    let vectors = embedder.embed(&texts)?;
    if vectors.len() != texts.len() {
        return Err(anyhow!("embedder returned {} vectors for {} texts", vectors.len(), texts.len()).into());
    }

    let mut report =
        SubmitReport { accepted: args.models.len(), embedder: embedder.name().to_string(), ..Default::default() };
    let mut write = ModelsWrite {
        semantics: Vec::new(),
        contracts: Vec::new(),
        contracts_removed: Vec::new(),
        statuses: Vec::new(),
        edges: Vec::new(),
    };
    let mut resolved_keys: HashSet<(String, String)> = HashSet::new();
    let mut topology_dirty = false;

    for (m, embedding) in args.models.into_iter().zip(vectors) {
        let sym = &symbols[&m.symbol_id];
        write.semantics.push(SemanticsRow {
            symbol_id: m.symbol_id.clone(),
            intent_summary: m.intent_summary,
            mechanism_prose: m.mechanism_prose,
            domain_tags: m.domain_tags,
            annotated_ast_blake3: sym.ast_blake3.clone(),
            embedding,
        });

        match m.strict_contract {
            Some(c) => {
                report.strict_contracts_written += 1;
                write.statuses.push((m.symbol_id.clone(), "STRICT_MAPPED"));
                write.contracts.push((
                    m.symbol_id.clone(),
                    Contract {
                        invariants_assumed: c.invariants_assumed,
                        invariants_enforced: c.invariants_enforced,
                        state_mutations: c.state_mutations,
                        error_surface: c.error_surface,
                        concurrency_assumptions: c.concurrency_assumptions,
                        trust_boundary: c.trust_boundary,
                        taint_sources: c.taint_sources,
                        taint_sinks: c.taint_sinks,
                        audit_smells: c.audit_smells,
                        risk_score: c.risk_score,
                    },
                ));
            }
            None => {
                write.statuses.push((m.symbol_id.clone(), "LIGHTWEIGHT_MAPPED"));
                // A stale contract from an earlier mapping must not outlive a model that dropped it.
                write.contracts_removed.push(m.symbol_id.clone());
            }
        }

        for e in m.resolved_edges {
            let kind = e.edge_kind.as_str();
            let key = (m.symbol_id.clone(), e.to_symbol_id.clone(), kind.to_string());
            resolved_keys.insert((key.0.clone(), key.1.clone()));
            let prior = existing.get(&key);
            let verified = e.resolution == Resolution::LlmVerified;
            match (verified, prior.is_some()) {
                (true, true) => report.edges_verified += 1,
                (true, false) => {
                    report.edges_created += 1;
                    topology_dirty |= e.edge_kind == EdgeKind::Calls;
                }
                (false, _) => {
                    report.edges_pruned += 1;
                    topology_dirty |= e.edge_kind == EdgeKind::Calls;
                }
            }
            write.edges.push(EdgeRow {
                from: m.symbol_id.clone(),
                to: e.to_symbol_id,
                kind: kind.to_string(),
                confidence: if verified { "LLM_VERIFIED" } else { "LLM_PRUNED" }.to_string(),
                lines: prior.map(|p| p.lines.clone()).unwrap_or_default(),
                snippet: prior.map(|p| p.snippet.clone()).unwrap_or_default(),
                semantics: e.edge_semantics,
                taint: e.data_taint_propagated,
            });
        }
    }

    report.unresolved_heuristic_edges = existing_edges
        .iter()
        .filter(|e| e.confidence == "STATIC_HEURISTIC" && !resolved_keys.contains(&(e.from.clone(), e.to.clone())))
        .map(|e| UnresolvedEdge { from: e.from.clone(), to: e.to.clone() })
        .collect();
    report.unresolved_heuristic_edges.sort_by(|a, b| (&a.from, &a.to).cmp(&(&b.from, &b.to)));

    mutations::write_symbol_models(db, &write)?;
    if topology_dirty {
        scc::recompute(db)?;
        report.topology_recomputed = true;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinator::embeddings::HashEmbedder;
    use crate::mcp::batch::tests::repo;

    fn model(id: &str) -> SymbolModel {
        SymbolModel {
            symbol_id: id.to_string(),
            intent_summary: "does a thing".into(),
            mechanism_prose: "calls helpers in order".into(),
            domain_tags: vec!["demo".into()],
            promote_to_strict_contract: false,
            strict_contract: None,
            resolved_edges: vec![],
        }
    }

    fn contract() -> StrictContractInput {
        StrictContractInput {
            invariants_assumed: vec!["input non-empty".into()],
            invariants_enforced: vec![],
            state_mutations: vec![],
            error_surface: vec!["panics on None".into()],
            concurrency_assumptions: vec![],
            trust_boundary: true,
            taint_sources: vec!["p".into()],
            taint_sinks: vec![],
            audit_smells: vec!["unchecked deref".into()],
            risk_score: 7,
        }
    }

    fn id(db: &DbInstance, name: &str) -> String {
        queries::all_symbols(db).unwrap().into_iter().find(|s| s.name == name).unwrap().symbol_id
    }

    fn submit(db: &DbInstance, models: Vec<SymbolModel>) -> Result<SubmitReport, SubmitError> {
        submit_symbol_models(db, &HashEmbedder, SubmitArgs { models })
    }

    fn invalid(r: Result<SubmitReport, SubmitError>) -> Vec<ValidationError> {
        match r {
            Err(SubmitError::Invalid(e)) => e,
            Err(SubmitError::Internal(e)) => panic!("internal: {e:#}"),
            Ok(_) => panic!("expected validation errors"),
        }
    }

    #[test]
    fn happy_path_writes_semantics_status_and_embedding_and_is_searchable() {
        let (_d, db) = repo(&[("src/lib.rs", "fn leaf() {}\nfn mid() { leaf(); }\n")]);
        let leaf = id(&db, "leaf");
        let r = submit(&db, vec![model(&leaf)]).ok().unwrap();
        assert_eq!(r.accepted, 1);
        assert_eq!(r.embedder, "hash-fallback");
        let s = queries::all_symbols(&db).unwrap().into_iter().find(|s| s.name == "leaf").unwrap();
        assert_eq!(s.mapping_status, "LIGHTWEIGHT_MAPPED");
        let sem = queries::semantics_by_ids(&db, std::slice::from_ref(&leaf)).unwrap();
        assert_eq!(sem[&leaf].intent_summary, "does a thing");
        let hits =
            queries::rows(&db, "?[id] := ~symbol_semantics:fts_idx{ symbol_id: id | query: 'helpers', k: 3 }").unwrap();
        assert_eq!(hits.len(), 1);
        // resubmitting replaces the row (HNSW index update path)
        submit(&db, vec![SymbolModel { intent_summary: "changed".into(), ..model(&leaf) }]).ok().unwrap();
        assert_eq!(
            queries::semantics_by_ids(&db, std::slice::from_ref(&leaf)).unwrap()[&leaf].intent_summary,
            "changed"
        );
    }

    #[test]
    fn risky_symbol_without_contract_is_rejected_with_reasons_and_nothing_is_written() {
        let (_d, db) = repo(&[("src/lib.rs", "fn ok() {}\nfn risky(p: *const u8) -> u8 { unsafe { *p } }\n")]);
        let (ok, risky) = (id(&db, "ok"), id(&db, "risky"));
        let errs = invalid(submit(&db, vec![model(&ok), model(&risky)]));
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].symbol_id, risky);
        assert_eq!(errs[0].field, "strict_contract");
        assert!(errs[0].message.contains("unsafe_usage"), "{}", errs[0].message);
        // atomic: the valid model was not written either
        assert!(queries::semantics_by_ids(&db, &[ok]).unwrap().is_empty());
    }

    #[test]
    fn empty_hazard_accounting_is_rejected_but_filled_contract_is_accepted() {
        let (_d, db) = repo(&[("src/lib.rs", "fn risky(p: *const u8) -> u8 { unsafe { *p } }\n")]);
        let risky = id(&db, "risky");
        let hollow = StrictContractInput { invariants_assumed: vec![], error_surface: vec![], ..contract() };
        let errs = invalid(submit(&db, vec![SymbolModel { strict_contract: Some(hollow), ..model(&risky) }]));
        assert!(errs[0].message.contains("account for each flagged hazard"));

        let r = submit(&db, vec![SymbolModel { strict_contract: Some(contract()), ..model(&risky) }]).ok().unwrap();
        assert_eq!(r.strict_contracts_written, 1);
        let c = queries::contracts_by_ids(&db, std::slice::from_ref(&risky)).unwrap();
        assert_eq!(c[&risky].risk_score, 7);
        assert!(c[&risky].trust_boundary);
        let s = queries::all_symbols(&db).unwrap().into_iter().find(|s| s.name == "risky").unwrap();
        assert_eq!(s.mapping_status, "STRICT_MAPPED");
    }

    #[test]
    fn other_validation_failures_are_reported_together() {
        let (_d, db) = repo(&[("src/lib.rs", "fn f() {}\nfn g() { f(); }\n")]);
        let (f, g) = (id(&db, "f"), id(&db, "g"));
        let bad_score = StrictContractInput { risk_score: 11, ..contract() };
        let errs = invalid(submit(
            &db,
            vec![
                model("nope"),
                SymbolModel { intent_summary: " ".into(), strict_contract: Some(bad_score), ..model(&f) },
                model(&f),
                SymbolModel {
                    resolved_edges: vec![
                        ResolvedEdge {
                            to_symbol_id: "ghost".into(),
                            edge_kind: EdgeKind::Calls,
                            resolution: Resolution::LlmVerified,
                            edge_semantics: "x".into(),
                            data_taint_propagated: false,
                        },
                        ResolvedEdge {
                            to_symbol_id: f.clone(),
                            edge_kind: EdgeKind::ReadsState,
                            resolution: Resolution::LlmPruned,
                            edge_semantics: "x".into(),
                            data_taint_propagated: false,
                        },
                    ],
                    ..model(&g)
                },
            ],
        ));
        let fields: Vec<_> = errs.iter().map(|e| e.field.as_str()).collect();
        assert!(fields.contains(&"symbol_id"));
        assert!(fields.contains(&"intent_summary"));
        assert!(fields.contains(&"strict_contract.risk_score"));
        assert!(fields.contains(&"resolved_edges[0]"));
        assert!(fields.contains(&"resolved_edges[1]"));
        assert!(errs.iter().any(|e| e.message.contains("more than once")));
    }

    #[test]
    fn edge_verdicts_update_confidence_prune_recomputes_topology_and_taint_is_stored() {
        let src = "struct A; struct B;\nimpl A { fn run(&self) {} }\nimpl B { fn run(&self) {} }\n\
                   fn caller(x: A) { x.run(); }\nfn other() {}\n";
        let (_d, db) = repo(&[("src/lib.rs", src)]);
        let caller = id(&db, "caller");
        let runs: Vec<String> =
            queries::all_symbols(&db).unwrap().into_iter().filter(|s| s.name == "run").map(|s| s.symbol_id).collect();
        let (run_a, run_b) = (
            runs.iter().find(|r| r.contains("::A::")).unwrap().clone(),
            runs.iter().find(|r| r.contains("::B::")).unwrap().clone(),
        );

        // Only verify one of the two heuristic candidates: the other is reported unresolved.
        let edge = |to: &str, res, taint| ResolvedEdge {
            to_symbol_id: to.to_string(),
            edge_kind: EdgeKind::Calls,
            resolution: res,
            edge_semantics: "x is an A".into(),
            data_taint_propagated: taint,
        };
        let r = submit(
            &db,
            vec![SymbolModel { resolved_edges: vec![edge(&run_a, Resolution::LlmVerified, true)], ..model(&caller) }],
        )
        .ok()
        .unwrap();
        assert_eq!((r.edges_verified, r.edges_pruned, r.edges_created), (1, 0, 0));
        assert_eq!(r.unresolved_heuristic_edges, vec![UnresolvedEdge { from: caller.clone(), to: run_b.clone() }]);
        assert!(!r.topology_recomputed);

        let r = submit(
            &db,
            vec![SymbolModel {
                resolved_edges: vec![
                    edge(&run_a, Resolution::LlmVerified, true),
                    edge(&run_b, Resolution::LlmPruned, false),
                ],
                ..model(&caller)
            }],
        )
        .ok()
        .unwrap();
        assert_eq!(r.edges_pruned, 1);
        assert!(r.unresolved_heuristic_edges.is_empty());
        assert!(r.topology_recomputed);

        let edges = queries::edges_from(&db, std::slice::from_ref(&caller)).unwrap();
        let get = |to: &str| edges.iter().find(|e| e.to == to && e.kind == "CALLS").unwrap();
        assert_eq!(get(&run_a).confidence, "LLM_VERIFIED");
        assert!(get(&run_a).taint);
        assert_eq!(get(&run_a).semantics, "x is an A");
        assert_eq!(get(&run_a).snippet, "x.run()");
        assert_eq!(get(&run_b).confidence, "LLM_PRUNED");
        // pruned edges no longer count as dependencies
        assert!(!crate::db::queries::call_edges(&db).unwrap().contains(&(caller.clone(), run_b)));
    }

    #[test]
    fn llm_can_add_an_edge_the_linker_missed_and_dropping_a_contract_removes_it() {
        let (_d, db) = repo(&[("src/lib.rs", "fn a() {}\nfn b() {}\n")]);
        let (a, b) = (id(&db, "a"), id(&db, "b"));
        let dynamic = ResolvedEdge {
            to_symbol_id: b.clone(),
            edge_kind: EdgeKind::Calls,
            resolution: Resolution::LlmVerified,
            edge_semantics: "dispatched via trait object".into(),
            data_taint_propagated: false,
        };
        let r = submit(
            &db,
            vec![SymbolModel {
                resolved_edges: vec![dynamic],
                promote_to_strict_contract: true,
                strict_contract: Some(contract()),
                ..model(&a)
            }],
        )
        .ok()
        .unwrap();
        assert_eq!(r.edges_created, 1);
        assert!(r.topology_recomputed);
        let lvl =
            |n: &str| queries::all_symbols(&db).unwrap().into_iter().find(|s| s.name == n).unwrap().topological_level;
        assert_eq!((lvl("a"), lvl("b")), (1, 0));

        submit(&db, vec![model(&a)]).ok().unwrap();
        assert!(queries::contracts_by_ids(&db, std::slice::from_ref(&a)).unwrap().is_empty());
    }
}
