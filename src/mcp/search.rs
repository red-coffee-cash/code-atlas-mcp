//! `semantic_atlas_search` (spec §7.2): hybrid HNSW vector + full-text search over symbols,
//! module rollups and execution flows, fused with reciprocal rank fusion.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

use anyhow::Result;
use cozo::{DataValue, DbInstance};
use rmcp::schemars::{self, JsonSchema};
use serde::Deserialize;

use crate::coordinator::embeddings::Embedder;
use crate::db::{dv_floats, queries};

const DEFAULT_LIMIT: usize = 10;
const MAX_LIMIT: usize = 30;
const RRF_K: f64 = 60.0;
/// Candidates pulled from each index before fusion and filtering.
const CANDIDATES: usize = 60;

const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "do", "does", "for", "from", "how", "in", "is", "it", "of", "on",
    "or", "the", "to", "what", "when", "where", "which", "who", "why", "with",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SearchScope {
    Symbols,
    Modules,
    Flows,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchArgs {
    /// Conceptual question or keywords, e.g. "where are JWT expiration timestamps validated?"
    pub query: String,
    /// Which tiers to search; default all.
    pub scopes: Option<Vec<SearchScope>>,
    /// Results per tier; default 10, max 30.
    pub limit: Option<usize>,
    /// Restrict symbol results to this path prefix.
    pub scope_path_prefix: Option<String>,
}

/// `OR`-joined alphanumeric terms; Cozo FTS ANDs bare words and rejects punctuation.
fn fts_query(q: &str) -> Option<String> {
    let mut terms: Vec<String> = Vec::new();
    for t in q.split(|c: char| !c.is_alphanumeric()) {
        let t = t.to_lowercase();
        if t.len() > 1 && !STOPWORDS.contains(&t.as_str()) && !terms.contains(&t) {
            terms.push(t);
        }
    }
    (!terms.is_empty()).then(|| terms.join(" OR "))
}

fn ranked(rows: Vec<Vec<DataValue>>) -> Vec<String> {
    rows.iter().filter_map(|r| r[0].get_str().map(str::to_owned)).collect()
}

fn vec_hits(db: &DbInstance, index: &str, key: &str, qvec: &[f32]) -> Result<Vec<String>> {
    let mut p = BTreeMap::new();
    p.insert("q".to_string(), dv_floats(qvec));
    p.insert("k".to_string(), DataValue::from(CANDIDATES as i64));
    let script = format!(
        "?[id, dist] := ~{index}{{ {key}: id | query: vec($q), k: $k, ef: 100, bind_distance: dist }} :order dist"
    );
    Ok(ranked(queries::rows_with(db, &script, p)?))
}

/// Reciprocal rank fusion; returns (id, score, sources) best first.
fn fuse(lists: &[(&'static str, Vec<String>)]) -> Vec<(String, f64, Vec<&'static str>)> {
    let mut acc: HashMap<&str, (f64, Vec<&'static str>)> = HashMap::new();
    for (name, list) in lists {
        for (rank, id) in list.iter().enumerate() {
            let e = acc.entry(id.as_str()).or_default();
            e.0 += 1.0 / (RRF_K + rank as f64 + 1.0);
            e.1.push(name);
        }
    }
    let mut out: Vec<_> = acc.into_iter().map(|(id, (s, src))| (id.to_string(), s, src)).collect();
    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
    out
}

pub fn semantic_atlas_search(db: &DbInstance, embedder: &dyn Embedder, args: &SearchArgs) -> Result<String> {
    let limit = args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let scopes =
        args.scopes.clone().unwrap_or_else(|| vec![SearchScope::Symbols, SearchScope::Modules, SearchScope::Flows]);
    let qvec = embedder.embed(std::slice::from_ref(&args.query))?.remove(0);

    let mut out = String::new();
    let _ = writeln!(out, "# ATLAS SEARCH: \"{}\"\n", args.query.trim());

    if scopes.contains(&SearchScope::Symbols) {
        let semantic = vec_hits(db, "symbol_semantics:vec_idx", "symbol_id", &qvec)?;
        let keyword = match fts_query(&args.query) {
            Some(text) => {
                let mut p = BTreeMap::new();
                p.insert("t".to_string(), DataValue::from(text));
                p.insert("k".to_string(), DataValue::from(CANDIDATES as i64));
                ranked(queries::rows_with(
                    db,
                    "?[id, s] := ~symbol_semantics:fts_idx{ symbol_id: id | query: $t, k: $k, bind_score: s } :order -s",
                    p,
                )?)
            }
            None => Vec::new(),
        };
        let fused = fuse(&[("semantic", semantic), ("keyword", keyword)]);
        let ids: Vec<String> = fused.iter().map(|(id, _, _)| id.clone()).collect();
        let syms = queries::symbols_by_ids(db, &ids)?;
        let sems = queries::semantics_by_ids(db, &ids)?;

        let hits: Vec<_> = fused
            .iter()
            .filter(|(id, _, _)| {
                syms.get(id)
                    .is_some_and(|s| args.scope_path_prefix.as_deref().is_none_or(|p| s.file_path.starts_with(p)))
            })
            .take(limit)
            .collect();
        let _ = writeln!(out, "## Symbols ({})", hits.len());
        for (i, (id, score, src)) in hits.iter().enumerate() {
            let (s, m) = (&syms[id], &sems[id]);
            let _ = writeln!(
                out,
                "{}. **`{id}`** — {} · {}:{}-{} · {} · score {score:.4} ({})",
                i + 1,
                s.kind,
                s.file_path,
                s.start_line,
                s.end_line,
                s.mapping_status,
                src.join("+")
            );
            let _ = writeln!(out, "   * {}", m.intent_summary);
            if !m.domain_tags.is_empty() {
                let _ = writeln!(out, "   * tags: {}", m.domain_tags.join(", "));
            }
        }
        if hits.is_empty() {
            let _ = writeln!(out, "No mapped symbols match. Only symbols with a stored mental model are searchable.");
        }
        out.push('\n');
    }

    if scopes.contains(&SearchScope::Modules) {
        let ids = vec_hits(db, "module_rollup:module_vec_idx", "module_path", &qvec)?;
        let rollups: HashMap<String, queries::Rollup> =
            queries::rollups(db)?.into_iter().map(|r| (r.module_path.clone(), r)).collect();
        let hits: Vec<_> = ids.iter().filter_map(|id| rollups.get(id)).take(limit).collect();
        let _ = writeln!(out, "## Modules ({})", hits.len());
        for (i, r) in hits.iter().enumerate() {
            let _ = writeln!(out, "{}. **`{}`** — {}", i + 1, r.module_path, r.architectural_role);
            if !r.key_abstractions.is_empty() {
                let _ = writeln!(out, "   * abstractions: {}", r.key_abstractions.join(", "));
            }
        }
        out.push('\n');
    }

    if scopes.contains(&SearchScope::Flows) {
        let ids = vec_hits(db, "execution_flow:flow_vec_idx", "flow_id", &qvec)?;
        let rows = queries::rows(
            db,
            "?[id, name, narrative] := *execution_flow{flow_id: id, name, narrative_summary: narrative}",
        )?;
        let flows: HashMap<String, (String, String)> = rows
            .iter()
            .map(|r| {
                (
                    r[0].get_str().unwrap_or("").to_string(),
                    (r[1].get_str().unwrap_or("").to_string(), r[2].get_str().unwrap_or("").to_string()),
                )
            })
            .collect();
        let hits: Vec<_> = ids.iter().filter_map(|id| flows.get(id).map(|f| (id, f))).take(limit).collect();
        let _ = writeln!(out, "## Execution flows ({})", hits.len());
        for (i, (id, (name, narrative))) in hits.iter().enumerate() {
            let _ = writeln!(out, "{}. **`{id}`** {name}\n   * {narrative}", i + 1);
        }
    }
    Ok(out)
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

    fn world() -> (tempfile::TempDir, DbInstance) {
        let (d, db) = repo(&[
            ("src/auth/jwt.rs", "fn check_expiry() {}\nfn parse_header() {}"),
            ("src/billing/charge.rs", "fn charge_card() {}"),
        ]);
        let m = |name: &str, intent: &str, prose: &str| SymbolModel {
            symbol_id: id(&db, name),
            intent_summary: intent.into(),
            mechanism_prose: prose.into(),
            domain_tags: vec!["demo".into()],
            promote_to_strict_contract: false,
            strict_contract: None,
            resolved_edges: vec![],
        };
        let models = vec![
            m(
                "check_expiry",
                "Rejects tokens past their exp claim",
                "validates the JWT expiration timestamp against the clock",
            ),
            m("parse_header", "Splits the authorization header", "parses the bearer scheme and token segments"),
            m("charge_card", "Charges the customer card", "calls the payment gateway and records the invoice"),
        ];
        if let Err(SubmitError::Invalid(e)) = submit_symbol_models(&db, &HashEmbedder, SubmitArgs { models }) {
            panic!("{e:?}");
        }
        architecture::submit_module_rollup(
            &db,
            &HashEmbedder,
            RollupArgs {
                module_path: "src/billing".into(),
                parent_module_path: None,
                architectural_role: "payment processing and invoicing".into(),
                key_abstractions: vec!["Invoice".into()],
                cross_module_contracts: vec![],
                known_architectural_risks: vec![],
            },
        )
        .unwrap()
        .ok()
        .unwrap();
        architecture::record_execution_flow(
            &db,
            &HashEmbedder,
            FlowArgs {
                flow_id: "checkout".into(),
                name: "Checkout settlement".into(),
                entrypoint_symbol_ids: vec![id(&db, "charge_card")],
                terminal_sink_symbol_ids: vec![id(&db, "charge_card")],
                narrative_summary: "customer card is charged and the invoice recorded".into(),
                state_lifecycle_summary: "pending to paid".into(),
                security_boundaries_crossed: vec![],
                steps: vec![FlowStepInput {
                    symbol_id: id(&db, "charge_card"),
                    role_in_flow: "charges".into(),
                    critical_assumptions_at_step: String::new(),
                }],
            },
        )
        .unwrap()
        .ok()
        .unwrap();
        (d, db)
    }

    fn search(db: &DbInstance, q: &str, f: impl FnOnce(&mut SearchArgs)) -> String {
        let mut a = SearchArgs { query: q.into(), scopes: None, limit: None, scope_path_prefix: None };
        f(&mut a);
        semantic_atlas_search(db, &HashEmbedder, &a).unwrap()
    }

    #[test]
    fn conceptual_question_finds_the_right_symbol_first() {
        let (_d, db) = world();
        let md = search(&db, "where are JWT expiration timestamps validated?", |_| {});
        let first = md.lines().find(|l| l.starts_with("1. ")).unwrap();
        assert!(first.contains("::check_expiry#"), "{md}");
        assert!(first.contains("semantic+keyword"), "{first}");
        assert!(md.contains("Rejects tokens past their exp claim"));
    }

    #[test]
    fn modules_flows_and_path_scoping() {
        let (_d, db) = world();
        let md = search(&db, "payment invoice card", |_| {});
        assert!(md.contains("**`src/billing`** — payment processing and invoicing"));
        assert!(md.contains("**`flow::checkout`** Checkout settlement"));

        let scoped = search(&db, "payment invoice card", |a| {
            a.scope_path_prefix = Some("src/auth".into());
            a.scopes = Some(vec![SearchScope::Symbols]);
        });
        assert!(!scoped.contains("charge_card") && !scoped.contains("## Modules"));
    }

    #[test]
    fn punctuation_stopwords_and_empty_corpus_are_safe() {
        assert_eq!(fts_query("where is the JWT-expiration? (really)").as_deref(), Some("jwt OR expiration OR really"));
        assert_eq!(fts_query("the of and"), None);
        let (_d, db) = repo(&[("src/a.rs", "fn a() {}")]);
        let md = search(&db, "???", |_| {});
        assert!(md.contains("No mapped symbols match"));
    }
}
