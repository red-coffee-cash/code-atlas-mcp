//! Atomic writers. Each public fn is a single Cozo transaction.

use std::collections::BTreeMap;

use anyhow::Result;
use cozo::{DataValue, DbInstance, ScriptMutability};

use super::schema::cozo_err;
use super::{dv_ints, dv_rows, dv_strs};
use crate::ingestion::model::ParsedSymbol;

pub struct FileRow {
    pub path: String,
    pub language: &'static str,
    pub blake3: String,
    pub parent_module_path: String,
    pub line_count: i64,
    pub indexed_at_ms: i64,
}

pub struct SymbolRow {
    pub sym: ParsedSymbol,
    pub mapping_status: &'static str,
}

pub struct EdgeRow {
    pub from: String,
    pub to: String,
    pub kind: String,
    pub confidence: String,
    pub lines: Vec<i64>,
    pub snippet: String,
    pub semantics: String,
    pub taint: bool,
}

/// Everything one sync produces. Tier-1 rows are upserted, rows for vanished files/symbols are
/// removed (with their dependent semantics/contracts/flow steps), and the edge set is replaced.
pub struct Skeleton {
    pub files: Vec<FileRow>,
    pub symbols: Vec<SymbolRow>,
    pub edges: Vec<EdgeRow>,
    pub removed_files: Vec<String>,
    pub removed_symbols: Vec<String>,
}

const WRITE_SKELETON: &str = r#"
{ ?[file_path] <- $removed_files :rm file_node { file_path } }
{ ?[symbol_id] <- $removed_symbols :rm symbol_node { symbol_id } }
{ ?[from_symbol_id, to_symbol_id, edge_kind] := *symbol_edge{from_symbol_id, to_symbol_id, edge_kind}
  :rm symbol_edge { from_symbol_id, to_symbol_id, edge_kind } }
{ ?[symbol_id] := *symbol_semantics{symbol_id}, not *symbol_node{symbol_id}
  :rm symbol_semantics { symbol_id } }
{ ?[symbol_id] := *strict_audit_contract{symbol_id}, not *symbol_node{symbol_id}
  :rm strict_audit_contract { symbol_id } }
{ ?[flow_id, symbol_id] := *flow_step{flow_id, symbol_id}, not *symbol_node{symbol_id}
  :rm flow_step { flow_id, symbol_id } }
{ ?[file_path, language, file_blake3, parent_module_path, line_count, indexed_at_ms] <- $files
  :put file_node { file_path => language, file_blake3, parent_module_path, line_count, indexed_at_ms } }
{ ?[symbol_id, qualified_name, name, kind, visibility, file_path, start_byte, end_byte, start_line,
    end_line, signature, doc_comment, ast_blake3, cyclomatic_complexity, risk_flags,
    requires_strict_contract, scc_group_id, topological_level, mapping_status, is_entrypoint] <- $symbols
  :put symbol_node { symbol_id => qualified_name, name, kind, visibility, file_path, start_byte,
    end_byte, start_line, end_line, signature, doc_comment, ast_blake3, cyclomatic_complexity,
    risk_flags, requires_strict_contract, scc_group_id, topological_level, mapping_status, is_entrypoint } }
{ ?[from_symbol_id, to_symbol_id, edge_kind, confidence, call_site_lines, call_expression_snippet,
    edge_semantics, data_taint_propagated] <- $edges
  :put symbol_edge { from_symbol_id, to_symbol_id, edge_kind => confidence, call_site_lines,
    call_expression_snippet, edge_semantics, data_taint_propagated } }
"#;

pub fn write_skeleton(db: &DbInstance, sk: &Skeleton) -> Result<()> {
    let i = |n: usize| DataValue::from(n as i64);
    let files = sk.files.iter().map(|f| {
        vec![
            DataValue::from(f.path.as_str()),
            DataValue::from(f.language),
            DataValue::from(f.blake3.as_str()),
            DataValue::from(f.parent_module_path.as_str()),
            DataValue::from(f.line_count),
            DataValue::from(f.indexed_at_ms),
        ]
    });
    let symbols = sk.symbols.iter().map(|r| {
        let s = &r.sym;
        vec![
            DataValue::from(s.symbol_id.as_str()),
            DataValue::from(s.qualified_name.as_str()),
            DataValue::from(s.name.as_str()),
            DataValue::from(s.kind),
            DataValue::from(s.visibility),
            DataValue::from(s.file_path.as_str()),
            i(s.start_byte),
            i(s.end_byte),
            i(s.start_line),
            i(s.end_line),
            DataValue::from(s.signature.as_str()),
            DataValue::from(s.doc_comment.as_str()),
            DataValue::from(s.ast_blake3.as_str()),
            DataValue::from(s.cyclomatic_complexity as i64),
            dv_strs(s.risk_flags.iter().cloned()),
            DataValue::from(s.requires_strict_contract),
            DataValue::from(-1i64), // recomputed by coordinator::scc after the write
            DataValue::from(-1i64),
            DataValue::from(r.mapping_status),
            DataValue::from(s.is_entrypoint),
        ]
    });
    let edges = sk.edges.iter().map(|e| {
        vec![
            DataValue::from(e.from.as_str()),
            DataValue::from(e.to.as_str()),
            DataValue::from(e.kind.as_str()),
            DataValue::from(e.confidence.as_str()),
            dv_ints(e.lines.iter().copied()),
            DataValue::from(e.snippet.as_str()),
            DataValue::from(e.semantics.as_str()),
            DataValue::from(e.taint),
        ]
    });
    let single = |ids: &[String]| dv_rows(ids.iter().map(|s| vec![DataValue::from(s.as_str())]).collect());

    let mut params = BTreeMap::new();
    params.insert("removed_files".to_string(), single(&sk.removed_files));
    params.insert("removed_symbols".to_string(), single(&sk.removed_symbols));
    params.insert("files".to_string(), dv_rows(files.collect()));
    params.insert("symbols".to_string(), dv_rows(symbols.collect()));
    params.insert("edges".to_string(), dv_rows(edges.collect()));

    db.run_script(WRITE_SKELETON, params, ScriptMutability::Mutable).map_err(cozo_err)?;
    Ok(())
}

/// Stores SCC group and topological level per symbol.
pub fn write_topology(db: &DbInstance, rows: Vec<(String, i64, i64)>) -> Result<()> {
    let rows = dv_rows(
        rows.into_iter().map(|(id, g, l)| vec![DataValue::from(id), DataValue::from(g), DataValue::from(l)]).collect(),
    );
    let mut params = BTreeMap::new();
    params.insert("rows".to_string(), rows);
    db.run_script(
        "?[symbol_id, scc_group_id, topological_level] <- $rows \
         :update symbol_node { symbol_id => scc_group_id, topological_level }",
        params,
        ScriptMutability::Mutable,
    )
    .map_err(cozo_err)?;
    Ok(())
}

pub struct SemanticsRow {
    pub symbol_id: String,
    pub intent_summary: String,
    pub mechanism_prose: String,
    pub domain_tags: Vec<String>,
    pub annotated_ast_blake3: String,
    pub embedding: Vec<f32>,
}

/// One Cartographer submission. Embeddings are already computed so the transaction stays short.
pub struct ModelsWrite {
    pub semantics: Vec<SemanticsRow>,
    pub contracts: Vec<(String, super::queries::Contract)>,
    pub contracts_removed: Vec<String>,
    pub statuses: Vec<(String, &'static str)>,
    pub edges: Vec<EdgeRow>,
}

const WRITE_MODELS: &str = r#"
{ ?[symbol_id, intent_summary, mechanism_prose, domain_tags, annotated_ast_blake3, embedding] <- $semantics
  :put symbol_semantics { symbol_id => intent_summary, mechanism_prose, domain_tags,
    annotated_ast_blake3, embedding } }
{ ?[symbol_id] <- $contracts_removed :rm strict_audit_contract { symbol_id } }
{ ?[symbol_id, invariants_assumed, invariants_enforced, state_mutations, error_surface,
    concurrency_assumptions, trust_boundary, taint_sources, taint_sinks, audit_smells, risk_score] <- $contracts
  :put strict_audit_contract { symbol_id => invariants_assumed, invariants_enforced, state_mutations,
    error_surface, concurrency_assumptions, trust_boundary, taint_sources, taint_sinks, audit_smells,
    risk_score } }
{ ?[symbol_id, mapping_status] <- $statuses :update symbol_node { symbol_id => mapping_status } }
{ ?[from_symbol_id, to_symbol_id, edge_kind, confidence, call_site_lines, call_expression_snippet,
    edge_semantics, data_taint_propagated] <- $edges
  :put symbol_edge { from_symbol_id, to_symbol_id, edge_kind => confidence, call_site_lines,
    call_expression_snippet, edge_semantics, data_taint_propagated } }
"#;

fn edge_row_values(e: &EdgeRow) -> Vec<DataValue> {
    vec![
        DataValue::from(e.from.as_str()),
        DataValue::from(e.to.as_str()),
        DataValue::from(e.kind.as_str()),
        DataValue::from(e.confidence.as_str()),
        dv_ints(e.lines.iter().copied()),
        DataValue::from(e.snippet.as_str()),
        DataValue::from(e.semantics.as_str()),
        DataValue::from(e.taint),
    ]
}

pub fn write_symbol_models(db: &DbInstance, w: &ModelsWrite) -> Result<()> {
    let sem = w.semantics.iter().map(|s| {
        vec![
            DataValue::from(s.symbol_id.as_str()),
            DataValue::from(s.intent_summary.as_str()),
            DataValue::from(s.mechanism_prose.as_str()),
            dv_strs(s.domain_tags.iter().cloned()),
            DataValue::from(s.annotated_ast_blake3.as_str()),
            super::dv_floats(&s.embedding),
        ]
    });
    let con = w.contracts.iter().map(|(id, c)| {
        vec![
            DataValue::from(id.as_str()),
            dv_strs(c.invariants_assumed.iter().cloned()),
            dv_strs(c.invariants_enforced.iter().cloned()),
            dv_strs(c.state_mutations.iter().cloned()),
            dv_strs(c.error_surface.iter().cloned()),
            dv_strs(c.concurrency_assumptions.iter().cloned()),
            DataValue::from(c.trust_boundary),
            dv_strs(c.taint_sources.iter().cloned()),
            dv_strs(c.taint_sinks.iter().cloned()),
            dv_strs(c.audit_smells.iter().cloned()),
            DataValue::from(c.risk_score),
        ]
    });
    let single = |ids: &[String]| dv_rows(ids.iter().map(|s| vec![DataValue::from(s.as_str())]).collect());
    let statuses = w.statuses.iter().map(|(id, st)| vec![DataValue::from(id.as_str()), DataValue::from(*st)]);

    let mut params = BTreeMap::new();
    params.insert("semantics".to_string(), dv_rows(sem.collect()));
    params.insert("contracts".to_string(), dv_rows(con.collect()));
    params.insert("contracts_removed".to_string(), single(&w.contracts_removed));
    params.insert("statuses".to_string(), dv_rows(statuses.collect()));
    params.insert("edges".to_string(), dv_rows(w.edges.iter().map(edge_row_values).collect()));
    db.run_script(WRITE_MODELS, params, ScriptMutability::Mutable).map_err(cozo_err)?;
    Ok(())
}

pub struct RollupRow {
    pub module_path: String,
    pub parent_module_path: String,
    pub architectural_role: String,
    pub key_abstractions: Vec<String>,
    pub cross_module_contracts: Vec<String>,
    pub known_architectural_risks: Vec<String>,
    pub embedding: Vec<f32>,
}

pub fn write_module_rollup(db: &DbInstance, r: &RollupRow) -> Result<()> {
    let mut params = BTreeMap::new();
    params.insert(
        "row".to_string(),
        dv_rows(vec![vec![
            DataValue::from(r.module_path.as_str()),
            DataValue::from(r.parent_module_path.as_str()),
            DataValue::from(r.architectural_role.as_str()),
            dv_strs(r.key_abstractions.iter().cloned()),
            dv_strs(r.cross_module_contracts.iter().cloned()),
            dv_strs(r.known_architectural_risks.iter().cloned()),
            super::dv_floats(&r.embedding),
        ]]),
    );
    db.run_script(
        "?[module_path, parent_module_path, architectural_role, key_abstractions, cross_module_contracts, \
          known_architectural_risks, embedding] <- $row \
         :put module_rollup { module_path => parent_module_path, architectural_role, key_abstractions, \
          cross_module_contracts, known_architectural_risks, embedding }",
        params,
        ScriptMutability::Mutable,
    )
    .map_err(cozo_err)?;
    Ok(())
}

pub struct FlowRow {
    pub flow_id: String,
    pub name: String,
    pub entrypoint_symbol_ids: Vec<String>,
    pub terminal_sink_symbol_ids: Vec<String>,
    pub narrative_summary: String,
    pub state_lifecycle_summary: String,
    pub security_boundaries_crossed: Vec<String>,
    pub embedding: Vec<f32>,
    /// (symbol_id, role_in_flow, critical_assumptions_at_step), in step order.
    pub steps: Vec<(String, String, String)>,
}

/// Replaces any existing flow with the same id, steps included.
pub fn write_execution_flow(db: &DbInstance, f: &FlowRow) -> Result<()> {
    let mut params = BTreeMap::new();
    params.insert("flow_id".to_string(), DataValue::from(f.flow_id.as_str()));
    params.insert(
        "flow".to_string(),
        dv_rows(vec![vec![
            DataValue::from(f.flow_id.as_str()),
            DataValue::from(f.name.as_str()),
            dv_strs(f.entrypoint_symbol_ids.iter().cloned()),
            dv_strs(f.terminal_sink_symbol_ids.iter().cloned()),
            DataValue::from(f.narrative_summary.as_str()),
            DataValue::from(f.state_lifecycle_summary.as_str()),
            dv_strs(f.security_boundaries_crossed.iter().cloned()),
            super::dv_floats(&f.embedding),
        ]]),
    );
    params.insert(
        "steps".to_string(),
        dv_rows(
            f.steps
                .iter()
                .enumerate()
                .map(|(i, (sid, role, assumptions))| {
                    vec![
                        DataValue::from(f.flow_id.as_str()),
                        DataValue::from(sid.as_str()),
                        DataValue::from(i as i64 + 1),
                        DataValue::from(role.as_str()),
                        DataValue::from(assumptions.as_str()),
                    ]
                })
                .collect(),
        ),
    );
    db.run_script(
        r#"
{ ?[flow_id, symbol_id] := flow_id = $flow_id, *flow_step{flow_id, symbol_id}
  :rm flow_step { flow_id, symbol_id } }
{ ?[flow_id, name, entrypoint_symbol_ids, terminal_sink_symbol_ids, narrative_summary,
    state_lifecycle_summary, security_boundaries_crossed, embedding] <- $flow
  :put execution_flow { flow_id => name, entrypoint_symbol_ids, terminal_sink_symbol_ids,
    narrative_summary, state_lifecycle_summary, security_boundaries_crossed, embedding } }
{ ?[flow_id, symbol_id, step_order, role_in_flow, critical_assumptions_at_step] <- $steps
  :put flow_step { flow_id, symbol_id => step_order, role_in_flow, critical_assumptions_at_step } }
"#,
        params,
        ScriptMutability::Mutable,
    )
    .map_err(cozo_err)?;
    Ok(())
}
