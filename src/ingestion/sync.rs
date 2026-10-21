//! `sync_repository_skeleton`: scan, parse, link and persist the Tier-1/Tier-2 skeleton, keeping
//! whatever the Cartographer has already written (spec §4.2, §7.1).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use cozo::DbInstance;
use serde::Serialize;

use super::linker::{self, Edge};
use super::model::ParsedFile;
use super::parser::parse_file;
use super::walker::{self, SourceFile};
use crate::coordinator::scc;
use crate::db::mutations::{self, EdgeRow, FileRow, Skeleton, SymbolRow};
use crate::db::queries::{self, StoredEdge};

#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub struct SyncReport {
    pub files_scanned: usize,
    pub files_parsed: usize,
    /// Files in a language that has no parser yet.
    pub files_unsupported: usize,
    pub files_failed: usize,
    pub symbols_total: usize,
    pub symbols_new: usize,
    pub symbols_stale: usize,
    pub symbols_removed: usize,
    pub edges_total: usize,
    pub edges_heuristic: usize,
    pub scc_count: usize,
    pub cyclic_scc_count: usize,
    pub max_topological_level: i64,
}

pub fn sync_repository(db: &DbInstance, repo_root: &Path) -> Result<SyncReport> {
    let sources = walker::scan(repo_root)?;
    let mut report = SyncReport { files_scanned: sources.len(), ..Default::default() };

    let (mut parsed, failed, unsupported) = parse_all(&sources);
    report.files_parsed = parsed.len();
    report.files_failed = failed;
    report.files_unsupported = unsupported;

    let linked = linker::link(&mut parsed);

    let old_symbols = queries::symbol_ids(db)?;
    let old_files = queries::file_hashes(db)?;
    let annotated = queries::annotated_hashes(db)?;
    let contracts = queries::contract_ids(db)?;
    let old_edges = queries::stored_edges(db)?;

    let mut symbols = Vec::new();
    let mut stale: HashSet<String> = HashSet::new();
    for f in &parsed {
        for s in &f.symbols {
            let status = match annotated.get(&s.symbol_id) {
                None => "UNMAPPED",
                Some(h) if *h != s.ast_blake3 => {
                    stale.insert(s.symbol_id.clone());
                    "STALE"
                }
                Some(_) if contracts.contains(&s.symbol_id) => "STRICT_MAPPED",
                Some(_) => "LIGHTWEIGHT_MAPPED",
            };
            if !old_symbols.contains(&s.symbol_id) {
                report.symbols_new += 1;
            }
            symbols.push(SymbolRow { sym: s.clone(), mapping_status: status });
        }
    }
    report.symbols_total = symbols.len();
    report.symbols_stale = stale.len();

    let live: HashSet<&str> = symbols.iter().map(|s| s.sym.symbol_id.as_str()).collect();
    let removed_symbols: Vec<String> = old_symbols.iter().filter(|id| !live.contains(id.as_str())).cloned().collect();
    report.symbols_removed = removed_symbols.len();

    let new_paths: HashSet<&str> = parsed.iter().map(|f| f.rel_path.as_str()).collect();
    let removed_files: Vec<String> = old_files.keys().filter(|p| !new_paths.contains(p.as_str())).cloned().collect();

    let edges = merge_edges(linked, old_edges, &live, &stale);
    report.edges_total = edges.len();
    report.edges_heuristic = edges.iter().filter(|e| e.confidence == "STATIC_HEURISTIC").count();

    let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
    let by_path: HashMap<&str, &SourceFile> = sources.iter().map(|s| (s.rel_path.as_str(), s)).collect();
    let files = parsed
        .iter()
        .map(|f| {
            let src = by_path[f.rel_path.as_str()];
            FileRow {
                path: f.rel_path.clone(),
                language: f.language.as_str(),
                blake3: src.blake3.clone(),
                parent_module_path: f.rel_path.rsplit_once('/').map_or("", |(d, _)| d).to_string(),
                line_count: src.line_count as i64,
                indexed_at_ms: now_ms,
            }
        })
        .collect();

    mutations::write_skeleton(db, &Skeleton { files, symbols, edges, removed_files, removed_symbols })?;

    let topo = scc::recompute(db)?;
    report.scc_count = topo.scc_count;
    report.cyclic_scc_count = topo.cyclic_scc_count;
    report.max_topological_level = topo.max_level;
    Ok(report)
}

/// Parses in parallel; returns files in input order plus failure and unsupported counts.
fn parse_all(sources: &[SourceFile]) -> (Vec<ParsedFile>, usize, usize) {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(16);
    let chunk = sources.len().div_ceil(threads).max(1);

    let results: Vec<Vec<Result<Option<ParsedFile>>>> = std::thread::scope(|scope| {
        let handles: Vec<_> =
            sources.chunks(chunk).map(|c| scope.spawn(move || c.iter().map(parse_file).collect::<Vec<_>>())).collect();
        handles.into_iter().map(|h| h.join().expect("parser thread panicked")).collect()
    });

    let (mut parsed, mut failed, mut unsupported) = (Vec::new(), 0, 0);
    for r in results.into_iter().flatten() {
        match r {
            Ok(Some(f)) => parsed.push(f),
            Ok(None) => unsupported += 1,
            Err(e) => {
                tracing::warn!("parse failed: {e:#}");
                failed += 1;
            }
        }
    }
    (parsed, failed, unsupported)
}

/// Combines freshly linked static edges with what is already stored:
/// * LLM verdicts survive while the caller's code is unchanged;
/// * when the caller went STALE its verdicts reset to the static confidence so they are re-checked
///   (the old edge semantics stay as a hint);
/// * LLM-verified edges the linker no longer derives are kept while both endpoints exist.
fn merge_edges(linked: Vec<Edge>, old: Vec<StoredEdge>, live: &HashSet<&str>, stale: &HashSet<String>) -> Vec<EdgeRow> {
    let mut old_by_key: HashMap<(String, String, String), StoredEdge> =
        old.into_iter().map(|e| ((e.from.clone(), e.to.clone(), e.kind.clone()), e)).collect();

    let is_llm = |c: &str| matches!(c, "LLM_VERIFIED" | "LLM_PRUNED" | "LSP_EXACT");
    let mut out = Vec::new();

    for e in linked {
        let key = (e.from.clone(), e.to.clone(), e.kind.to_string());
        let prior = old_by_key.remove(&key);
        let (confidence, semantics, taint) = match prior {
            Some(p) if is_llm(&p.confidence) && !stale.contains(&e.from) => (p.confidence, p.semantics, p.taint),
            Some(p) => (e.confidence.to_string(), p.semantics, p.taint),
            None => (e.confidence.to_string(), String::new(), false),
        };
        out.push(EdgeRow {
            from: e.from,
            to: e.to,
            kind: e.kind.to_string(),
            confidence,
            lines: e.lines.iter().map(|&l| l as i64).collect(),
            snippet: e.snippet,
            semantics,
            taint,
        });
    }

    for (_, p) in old_by_key {
        let keep = matches!(p.confidence.as_str(), "LLM_VERIFIED" | "LSP_EXACT")
            && live.contains(p.from.as_str())
            && live.contains(p.to.as_str())
            && !stale.contains(&p.from);
        if keep {
            out.push(EdgeRow {
                from: p.from,
                to: p.to,
                kind: p.kind,
                confidence: p.confidence,
                lines: p.lines,
                snippet: p.snippet,
                semantics: p.semantics,
                taint: p.taint,
            });
        }
    }
    out.sort_by(|a, b| (&a.from, &a.to, &a.kind).cmp(&(&b.from, &b.to, &b.kind)));
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use cozo::{DataValue, ScriptMutability};

    use super::*;
    use crate::db::schema;

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn run(db: &DbInstance, script: &str) -> Vec<Vec<DataValue>> {
        db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable).unwrap().rows
    }

    fn id_of(db: &DbInstance, name: &str) -> String {
        let r = run(db, &format!("?[id] := *symbol_node{{symbol_id: id, name: '{name}'}}"));
        r[0][0].get_str().unwrap().to_string()
    }

    fn status_of(db: &DbInstance, name: &str) -> String {
        let r = run(db, &format!("?[s] := *symbol_node{{name: '{name}', mapping_status: s}}"));
        r[0][0].get_str().unwrap().to_string()
    }

    /// Plays the Cartographer: stores semantics (and a contract) for a symbol at its current hash.
    fn map_symbol(db: &DbInstance, name: &str, strict: bool) {
        let id = id_of(db, name);
        let h = run(db, &format!("?[h] := *symbol_node{{name: '{name}', ast_blake3: h}}"))[0][0]
            .get_str()
            .unwrap()
            .to_string();
        let zeros = vec!["0.0"; 384].join(",");
        run(
            db,
            &format!(
                "?[symbol_id, intent_summary, mechanism_prose, domain_tags, annotated_ast_blake3, embedding] \
                 <- [['{id}', 'i', 'm', [], '{h}', vec([{zeros}])]] \
                 :put symbol_semantics {{symbol_id => intent_summary, mechanism_prose, domain_tags, annotated_ast_blake3, embedding}}"
            ),
        );
        if strict {
            run(
                db,
                &format!(
                    "?[symbol_id, invariants_assumed, invariants_enforced, state_mutations, error_surface, \
                     concurrency_assumptions, trust_boundary, taint_sources, taint_sinks, audit_smells, risk_score] \
                     <- [['{id}', [], [], [], [], [], false, [], [], [], 3]] \
                     :put strict_audit_contract {{symbol_id => invariants_assumed, invariants_enforced, state_mutations, \
                     error_surface, concurrency_assumptions, trust_boundary, taint_sources, taint_sinks, audit_smells, risk_score}}"
                ),
            );
        }
    }

    const SRC: &str = "fn leaf() {}\nfn mid() { leaf(); }\nfn top() { mid(); }\n";

    #[test]
    fn first_sync_populates_skeleton_edges_and_topology() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/lib.rs", SRC);
        write(dir.path(), "README.md", "not source");
        let db = schema::open_in_memory().unwrap();

        let r = sync_repository(&db, dir.path()).unwrap();
        assert_eq!(r.files_scanned, 1);
        assert_eq!(r.files_parsed, 1);
        assert_eq!(r.files_unsupported, 0);
        assert_eq!((r.symbols_total, r.symbols_new, r.edges_total), (3, 3, 2));
        assert_eq!(r.max_topological_level, 2);

        let lv = run(&db, "?[n, l, s] := *symbol_node{name: n, topological_level: l, mapping_status: s}");
        let get = |n: &str| lv.iter().find(|r| r[0].get_str() == Some(n)).unwrap();
        assert_eq!(get("leaf")[1].get_int(), Some(0));
        assert_eq!(get("top")[1].get_int(), Some(2));
        assert_eq!(get("top")[2].get_str(), Some("UNMAPPED"));
        assert_eq!(run(&db, "?[c] := *file_node{line_count: c}")[0][0].get_int(), Some(3));
    }

    #[test]
    fn resync_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/lib.rs", SRC);
        let db = schema::open_in_memory().unwrap();
        sync_repository(&db, dir.path()).unwrap();
        let r = sync_repository(&db, dir.path()).unwrap();
        assert_eq!((r.symbols_total, r.symbols_new, r.symbols_stale, r.symbols_removed), (3, 0, 0, 0));
        assert_eq!(r.edges_total, 2);
    }

    #[test]
    fn code_change_marks_mapped_symbol_stale_but_formatting_does_not() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/lib.rs", SRC);
        let db = schema::open_in_memory().unwrap();
        sync_repository(&db, dir.path()).unwrap();
        map_symbol(&db, "mid", false);
        map_symbol(&db, "leaf", true);

        write(dir.path(), "src/lib.rs", "// comment\nfn leaf() {}\nfn mid() {\n    leaf( );\n}\nfn top() { mid(); }\n");
        let r = sync_repository(&db, dir.path()).unwrap();
        assert_eq!(r.symbols_stale, 0);
        assert_eq!(status_of(&db, "mid"), "LIGHTWEIGHT_MAPPED");
        assert_eq!(status_of(&db, "leaf"), "STRICT_MAPPED");

        write(dir.path(), "src/lib.rs", "fn leaf() {}\nfn mid() { leaf(); leaf(); }\nfn top() { mid(); }\n");
        let r = sync_repository(&db, dir.path()).unwrap();
        assert_eq!(r.symbols_stale, 1);
        assert_eq!(status_of(&db, "mid"), "STALE");
        assert_eq!(status_of(&db, "leaf"), "STRICT_MAPPED");
        assert_eq!(status_of(&db, "top"), "UNMAPPED");

        // Reverting the code restores the mapped status without re-annotation.
        write(dir.path(), "src/lib.rs", SRC);
        sync_repository(&db, dir.path()).unwrap();
        assert_eq!(status_of(&db, "mid"), "LIGHTWEIGHT_MAPPED");
    }

    #[test]
    fn removed_symbols_cascade_to_semantics_and_edges() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/lib.rs", SRC);
        write(dir.path(), "src/extra.rs", "fn gone() {}");
        let db = schema::open_in_memory().unwrap();
        sync_repository(&db, dir.path()).unwrap();
        map_symbol(&db, "gone", true);

        std::fs::remove_file(dir.path().join("src/extra.rs")).unwrap();
        let r = sync_repository(&db, dir.path()).unwrap();
        assert_eq!(r.symbols_removed, 1);
        assert!(run(&db, "?[x] := *symbol_semantics{symbol_id: x}").is_empty());
        assert!(run(&db, "?[x] := *strict_audit_contract{symbol_id: x}").is_empty());
        assert_eq!(run(&db, "?[p] := *file_node{file_path: p}").len(), 1);
    }

    #[test]
    fn llm_verdicts_survive_resync_until_the_caller_changes() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/lib.rs", SRC);
        let db = schema::open_in_memory().unwrap();
        sync_repository(&db, dir.path()).unwrap();

        let (mid, leaf) = (id_of(&db, "mid"), id_of(&db, "leaf"));
        run(
            &db,
            &format!(
                "?[from_symbol_id, to_symbol_id, edge_kind, confidence, call_site_lines, call_expression_snippet, \
                 edge_semantics, data_taint_propagated] \
                 <- [['{mid}', '{leaf}', 'CALLS', 'LLM_VERIFIED', [2], 'leaf()', 'passes the id', true]] \
                 :put symbol_edge {{from_symbol_id, to_symbol_id, edge_kind => confidence, call_site_lines, \
                 call_expression_snippet, edge_semantics, data_taint_propagated}}"
            ),
        );
        sync_repository(&db, dir.path()).unwrap();
        let edge = |db: &DbInstance| {
            run(
                db,
                "?[c, e, d] := *symbol_edge{from_symbol_id: f, confidence: c, edge_semantics: e, data_taint_propagated: d}, \
                     *symbol_node{symbol_id: f, name: 'mid'}",
            )
        };
        let e = edge(&db);
        assert_eq!(e[0][0].get_str(), Some("LLM_VERIFIED"));
        assert_eq!(e[0][1].get_str(), Some("passes the id"));
        assert_eq!(e[0][2].get_bool(), Some(true));

        // The caller changes while it has a stored mental model: the verdict resets for re-checking.
        map_symbol(&db, "mid", false);
        write(dir.path(), "src/lib.rs", "fn leaf() {}\nfn mid() { leaf(); leaf(); }\nfn top() { mid(); }\n");
        sync_repository(&db, dir.path()).unwrap();
        let e = edge(&db);
        assert_eq!(e[0][0].get_str(), Some("STATIC_EXACT"));
        assert_eq!(e[0][1].get_str(), Some("passes the id"));
    }
}
