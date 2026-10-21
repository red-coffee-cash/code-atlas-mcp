//! Read-side helpers over the stored relations.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::Result;
use cozo::{DataValue, DbInstance, ScriptMutability};

use super::schema::cozo_err;
use super::{as_ints, as_strs, str_of};

pub fn rows(db: &DbInstance, script: &str) -> Result<Vec<Vec<DataValue>>> {
    rows_with(db, script, BTreeMap::new())
}

pub fn rows_with(db: &DbInstance, script: &str, params: BTreeMap<String, DataValue>) -> Result<Vec<Vec<DataValue>>> {
    Ok(db.run_script(script, params, ScriptMutability::Immutable).map_err(cozo_err)?.rows)
}

#[derive(Debug, Clone)]
pub struct StoredEdge {
    pub from: String,
    pub to: String,
    pub kind: String,
    pub confidence: String,
    pub lines: Vec<i64>,
    pub snippet: String,
    pub semantics: String,
    pub taint: bool,
}

pub fn stored_edges(db: &DbInstance) -> Result<Vec<StoredEdge>> {
    let r = rows(
        db,
        "?[f, t, k, c, l, s, e, d] := *symbol_edge{from_symbol_id: f, to_symbol_id: t, edge_kind: k, \
         confidence: c, call_site_lines: l, call_expression_snippet: s, edge_semantics: e, data_taint_propagated: d}",
    )?;
    Ok(r.iter().map(|r| edge_from_row(r)).collect())
}

pub fn symbol_ids(db: &DbInstance) -> Result<HashSet<String>> {
    Ok(rows(db, "?[id] := *symbol_node{symbol_id: id}")?.iter().map(|r| str_of(&r[0])).collect())
}

/// symbol_id -> the AST hash the stored mental model was written against.
pub fn annotated_hashes(db: &DbInstance) -> Result<HashMap<String, String>> {
    Ok(rows(db, "?[id, h] := *symbol_semantics{symbol_id: id, annotated_ast_blake3: h}")?
        .iter()
        .map(|r| (str_of(&r[0]), str_of(&r[1])))
        .collect())
}

pub fn contract_ids(db: &DbInstance) -> Result<HashSet<String>> {
    Ok(rows(db, "?[id] := *strict_audit_contract{symbol_id: id}")?.iter().map(|r| str_of(&r[0])).collect())
}

pub fn file_hashes(db: &DbInstance) -> Result<HashMap<String, String>> {
    Ok(rows(db, "?[p, h] := *file_node{file_path: p, file_blake3: h}")?
        .iter()
        .map(|r| (str_of(&r[0]), str_of(&r[1])))
        .collect())
}

/// `(from, to)` pairs of live CALLS edges between existing symbols (spec §5.1 step 1).
pub fn call_edges(db: &DbInstance) -> Result<Vec<(String, String)>> {
    Ok(rows(
        db,
        "?[f, t] := *symbol_edge{from_symbol_id: f, to_symbol_id: t, edge_kind: 'CALLS', confidence}, \
         confidence != 'LLM_PRUNED', *symbol_node{symbol_id: f}, *symbol_node{symbol_id: t}",
    )?
    .iter()
    .map(|r| (str_of(&r[0]), str_of(&r[1])))
    .collect())
}

#[derive(Debug, Clone)]
pub struct SymbolInfo {
    pub symbol_id: String,
    pub qualified_name: String,
    #[cfg_attr(not(test), allow(dead_code))] // read by tests; kept as part of the record
    pub name: String,
    pub kind: String,
    pub visibility: String,
    pub file_path: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: i64,
    pub end_line: i64,
    pub signature: String,
    pub doc_comment: String,
    pub ast_blake3: String,
    pub complexity: i64,
    pub risk_flags: Vec<String>,
    pub requires_strict_contract: bool,
    pub scc_group_id: i64,
    pub topological_level: i64,
    pub mapping_status: String,
    pub is_entrypoint: bool,
}

impl SymbolInfo {
    pub fn is_mapped(&self) -> bool {
        matches!(self.mapping_status.as_str(), "LIGHTWEIGHT_MAPPED" | "STRICT_MAPPED")
    }

    pub fn is_callable(&self) -> bool {
        matches!(self.kind.as_str(), "fn" | "method")
    }

    /// Test code by location or naming (`tests/`, `*_test.rs`, `*.spec.ts`, `mod tests`, ...).
    /// Heuristic, language-agnostic; used to keep tests out of the mapping queue and coverage.
    pub fn is_test(&self) -> bool {
        let path = self.file_path.as_str();
        let file = path.rsplit('/').next().unwrap_or(path);
        path.split('/').any(|seg| matches!(seg, "tests" | "test" | "__tests__" | "spec" | "e2e"))
            || file.starts_with("test_")
            || file.contains("_test.")
            || file.contains(".test.")
            || file.contains(".spec.")
            || file.contains("_spec.")
            || file == "conftest.py"
            || self.qualified_name.split("::").any(|seg| matches!(seg, "tests" | "test"))
    }
}

const SYMBOL_COLS: &str = "symbol_id, qualified_name, name, kind, visibility, file_path, start_byte, end_byte, \
    start_line, end_line, signature, doc_comment, ast_blake3, cyclomatic_complexity, risk_flags, \
    requires_strict_contract, scc_group_id, topological_level, mapping_status, is_entrypoint";

fn symbol_from_row(r: &[DataValue]) -> SymbolInfo {
    let int = |i: usize| r[i].get_int().unwrap_or(0);
    SymbolInfo {
        symbol_id: str_of(&r[0]),
        qualified_name: str_of(&r[1]),
        name: str_of(&r[2]),
        kind: str_of(&r[3]),
        visibility: str_of(&r[4]),
        file_path: str_of(&r[5]),
        start_byte: int(6) as usize,
        end_byte: int(7) as usize,
        start_line: int(8),
        end_line: int(9),
        signature: str_of(&r[10]),
        doc_comment: str_of(&r[11]),
        ast_blake3: str_of(&r[12]),
        complexity: int(13),
        risk_flags: as_strs(&r[14]),
        requires_strict_contract: r[15].get_bool().unwrap_or(false),
        scc_group_id: int(16),
        topological_level: int(17),
        mapping_status: str_of(&r[18]),
        is_entrypoint: r[19].get_bool().unwrap_or(false),
    }
}

pub fn all_symbols(db: &DbInstance) -> Result<Vec<SymbolInfo>> {
    let script = format!("?[{SYMBOL_COLS}] := *symbol_node{{{SYMBOL_COLS}}} :order symbol_id");
    Ok(rows(db, &script)?.iter().map(|r| symbol_from_row(r)).collect())
}

pub fn symbols_by_ids(db: &DbInstance, ids: &[String]) -> Result<HashMap<String, SymbolInfo>> {
    let script = format!("?[{SYMBOL_COLS}] := symbol_id in $ids, *symbol_node{{{SYMBOL_COLS}}}");
    let mut params = BTreeMap::new();
    params.insert("ids".to_string(), super::dv_strs(ids.iter().cloned()));
    Ok(rows_with(db, &script, params)?.iter().map(|r| symbol_from_row(r)).map(|s| (s.symbol_id.clone(), s)).collect())
}

#[derive(Debug, Clone)]
pub struct Semantics {
    pub annotated_ast_blake3: String,
    pub intent_summary: String,
    pub mechanism_prose: String,
    pub domain_tags: Vec<String>,
}

pub fn semantics_by_ids(db: &DbInstance, ids: &[String]) -> Result<HashMap<String, Semantics>> {
    let mut params = BTreeMap::new();
    params.insert("ids".to_string(), super::dv_strs(ids.iter().cloned()));
    Ok(rows_with(
        db,
        "?[id, i, m, t, h] := id in $ids, *symbol_semantics{symbol_id: id, intent_summary: i, mechanism_prose: m, domain_tags: t, annotated_ast_blake3: h}",
        params,
    )?
    .iter()
    .map(|r| {
        (
            str_of(&r[0]),
            Semantics {
                intent_summary: str_of(&r[1]),
                mechanism_prose: str_of(&r[2]),
                domain_tags: as_strs(&r[3]),
                annotated_ast_blake3: str_of(&r[4]),
            },
        )
    })
    .collect())
}

#[derive(Debug, Clone, Default)]
pub struct Contract {
    pub invariants_assumed: Vec<String>,
    pub invariants_enforced: Vec<String>,
    pub state_mutations: Vec<String>,
    pub error_surface: Vec<String>,
    pub concurrency_assumptions: Vec<String>,
    pub trust_boundary: bool,
    pub taint_sources: Vec<String>,
    pub taint_sinks: Vec<String>,
    pub audit_smells: Vec<String>,
    pub risk_score: i64,
}

pub fn contracts_by_ids(db: &DbInstance, ids: &[String]) -> Result<HashMap<String, Contract>> {
    let mut params = BTreeMap::new();
    params.insert("ids".to_string(), super::dv_strs(ids.iter().cloned()));
    Ok(rows_with(
        db,
        "?[id, a, e, m, x, c, tb, ts, tk, s, r] := id in $ids, *strict_audit_contract{symbol_id: id, \
         invariants_assumed: a, invariants_enforced: e, state_mutations: m, error_surface: x, \
         concurrency_assumptions: c, trust_boundary: tb, taint_sources: ts, taint_sinks: tk, \
         audit_smells: s, risk_score: r}",
        params,
    )?
    .iter()
    .map(|r| {
        (
            str_of(&r[0]),
            Contract {
                invariants_assumed: as_strs(&r[1]),
                invariants_enforced: as_strs(&r[2]),
                state_mutations: as_strs(&r[3]),
                error_surface: as_strs(&r[4]),
                concurrency_assumptions: as_strs(&r[5]),
                trust_boundary: r[6].get_bool().unwrap_or(false),
                taint_sources: as_strs(&r[7]),
                taint_sinks: as_strs(&r[8]),
                audit_smells: as_strs(&r[9]),
                risk_score: r[10].get_int().unwrap_or(0),
            },
        )
    })
    .collect())
}

/// Stored edges leaving any of `from_ids` (all kinds and confidences).
pub fn edges_from(db: &DbInstance, from_ids: &[String]) -> Result<Vec<StoredEdge>> {
    let mut params = BTreeMap::new();
    params.insert("ids".to_string(), super::dv_strs(from_ids.iter().cloned()));
    let r = rows_with(
        db,
        "?[f, t, k, c, l, s, e, d] := f in $ids, *symbol_edge{from_symbol_id: f, to_symbol_id: t, edge_kind: k, \
         confidence: c, call_site_lines: l, call_expression_snippet: s, edge_semantics: e, data_taint_propagated: d}",
        params,
    )?;
    Ok(r.iter().map(|r| edge_from_row(r)).collect())
}

fn edge_from_row(r: &[DataValue]) -> StoredEdge {
    StoredEdge {
        from: str_of(&r[0]),
        to: str_of(&r[1]),
        kind: str_of(&r[2]),
        confidence: str_of(&r[3]),
        lines: as_ints(&r[4]),
        snippet: str_of(&r[5]),
        semantics: str_of(&r[6]),
        taint: r[7].get_bool().unwrap_or(false),
    }
}

/// A rejected field in a tool submission, returned to the agent so it can fix and resubmit.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct ValidationError {
    pub symbol_id: String,
    pub field: String,
    pub message: String,
}

/// Stored edges arriving at any of `to_ids`.
pub fn edges_to(db: &DbInstance, to_ids: &[String]) -> Result<Vec<StoredEdge>> {
    let mut params = BTreeMap::new();
    params.insert("ids".to_string(), super::dv_strs(to_ids.iter().cloned()));
    let r = rows_with(
        db,
        "?[f, t, k, c, l, s, e, d] := t in $ids, *symbol_edge{from_symbol_id: f, to_symbol_id: t, edge_kind: k, \
         confidence: c, call_site_lines: l, call_expression_snippet: s, edge_semantics: e, data_taint_propagated: d}",
        params,
    )?;
    Ok(r.iter().map(|r| edge_from_row(r)).collect())
}

#[derive(Debug, Clone)]
pub struct Rollup {
    pub module_path: String,
    pub parent_module_path: String,
    pub architectural_role: String,
    pub key_abstractions: Vec<String>,
    pub cross_module_contracts: Vec<String>,
    pub known_architectural_risks: Vec<String>,
}

pub fn rollups(db: &DbInstance) -> Result<Vec<Rollup>> {
    Ok(rows(
        db,
        "?[m, p, r, k, c, x] := *module_rollup{module_path: m, parent_module_path: p, architectural_role: r, \
         key_abstractions: k, cross_module_contracts: c, known_architectural_risks: x} :order m",
    )?
    .iter()
    .map(|r| Rollup {
        module_path: str_of(&r[0]),
        parent_module_path: str_of(&r[1]),
        architectural_role: str_of(&r[2]),
        key_abstractions: as_strs(&r[3]),
        cross_module_contracts: as_strs(&r[4]),
        known_architectural_risks: as_strs(&r[5]),
    })
    .collect())
}

#[derive(Debug, Clone)]
pub struct FlowMembership {
    pub flow_id: String,
    pub step_order: i64,
    pub total_steps: i64,
    pub role_in_flow: String,
}

pub fn flow_memberships(db: &DbInstance, symbol_id: &str) -> Result<Vec<FlowMembership>> {
    let mut params = BTreeMap::new();
    params.insert("sid".to_string(), DataValue::from(symbol_id));
    let r = rows_with(
        db,
        "total[flow_id, count(symbol_id)] := *flow_step{flow_id, symbol_id} \
         ?[flow_id, o, n, role] := sid = $sid, *flow_step{flow_id, symbol_id: sid, step_order: o, role_in_flow: role}, \
           *execution_flow{flow_id}, total[flow_id, n] :order flow_id",
        params,
    )?;
    Ok(r.iter()
        .map(|r| FlowMembership {
            flow_id: str_of(&r[0]),
            step_order: r[1].get_int().unwrap_or(0),
            total_steps: r[2].get_int().unwrap_or(0),
            role_in_flow: str_of(&r[3]),
        })
        .collect())
}

pub fn all_contracts(db: &DbInstance) -> Result<HashMap<String, Contract>> {
    let ids: Vec<String> =
        rows(db, "?[id] := *strict_audit_contract{symbol_id: id}")?.iter().map(|r| str_of(&r[0])).collect();
    contracts_by_ids(db, &ids)
}

/// One row of spec §9.2 Query B.
#[derive(Debug, Clone)]
pub struct AssumptionPair {
    pub caller: String,
    pub callee: String,
    pub taint: bool,
    pub edge_semantics: String,
    pub callee_assumptions: Vec<String>,
    pub caller_enforced: Vec<String>,
    pub caller_smells: Vec<String>,
    pub callee_risk: i64,
}

/// Spec §9.2 Query B, extended with the taint flag and caller smells so Rust can re-rank.
/// `cap` bounds the rows pulled back before ranking.
pub fn assumption_pairs(db: &DbInstance, min_risk: i64, cap: usize) -> Result<Vec<AssumptionPair>> {
    let mut params = BTreeMap::new();
    params.insert("min_risk_score".to_string(), DataValue::from(min_risk));
    params.insert("cap".to_string(), DataValue::from(cap as i64));
    let r = rows_with(
        db,
        r#"?[caller_id, callee_id, taint, edge_semantics, callee_assumptions, caller_enforced, caller_smells, callee_risk] :=
            *symbol_edge{from_symbol_id: caller_id, to_symbol_id: callee_id, confidence, edge_semantics,
                         data_taint_propagated: taint},
            confidence != "LLM_PRUNED",
            *strict_audit_contract{symbol_id: callee_id, invariants_assumed: callee_assumptions, risk_score: callee_risk},
            length(callee_assumptions) > 0,
            callee_risk >= $min_risk_score,
            *strict_audit_contract{symbol_id: caller_id, invariants_enforced: caller_enforced, audit_smells: caller_smells}
        :order -callee_risk, caller_id, callee_id
        :limit $cap"#,
        params,
    )?;
    Ok(r.iter()
        .map(|r| AssumptionPair {
            caller: str_of(&r[0]),
            callee: str_of(&r[1]),
            taint: r[2].get_bool().unwrap_or(false),
            edge_semantics: str_of(&r[3]),
            callee_assumptions: as_strs(&r[4]),
            caller_enforced: as_strs(&r[5]),
            caller_smells: as_strs(&r[6]),
            callee_risk: r[7].get_int().unwrap_or(0),
        })
        .collect())
}
