//! `get_next_cartography_batch` (spec §5.1, §7.1, §10.1): bottom-up topological scheduling with
//! callee contracts injected so each symbol is mapped against what its callees *actually* do.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::path::Path;

use anyhow::Result;
use cozo::DbInstance;
use rmcp::schemars::{self, JsonSchema};
use serde::Deserialize;

use crate::db::queries::{self, Contract, Semantics, StoredEdge, SymbolInfo};

const DEFAULT_BATCH: usize = 5;
const MAX_BATCH: usize = 15;
/// Source lines shown per symbol in a batch.
const MAX_SOURCE_LINES: usize = 400;

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct BatchArgs {
    /// Number of symbols to return (default 5, max 15). A mutually recursive group is never split.
    pub batch_size: Option<usize>,
    /// Restrict mapping to symbols under this directory or file prefix.
    pub scope_path_prefix: Option<String>,
    /// Restrict the queue to the transitive call cone beneath this symbol_id.
    pub root_entrypoint_id: Option<String>,
    /// Prefer unmapped symbols with risk_flags or high complexity.
    pub prioritize_high_risk: Option<bool>,
    /// Also queue structs/enums/traits/consts (default: only fn and method).
    pub include_types: Option<bool>,
    /// Also queue test code (default: excluded).
    pub include_tests: Option<bool>,
}

/// The tool returns `markdown`; the counts are asserted by tests and mirrored in the header text.
#[derive(Debug)]
#[cfg_attr(not(test), allow(dead_code))]
pub struct Batch {
    pub markdown: String,
    pub symbol_ids: Vec<String>,
    #[allow(dead_code)]
    pub ready_groups: usize,
    pub blocked_groups: usize,
    pub remaining: usize,
}

pub fn next_batch(db: &DbInstance, repo_root: &Path, args: &BatchArgs) -> Result<Batch> {
    let batch_size = args.batch_size.unwrap_or(DEFAULT_BATCH).clamp(1, MAX_BATCH);
    let symbols = queries::all_symbols(db)?;
    let by_id: HashMap<&str, &SymbolInfo> = symbols.iter().map(|s| (s.symbol_id.as_str(), s)).collect();
    let call_edges = queries::call_edges(db)?;

    let mut callees: HashMap<&str, Vec<&str>> = HashMap::new();
    for (f, t) in &call_edges {
        callees.entry(f.as_str()).or_default().push(t.as_str());
    }

    let cone = match &args.root_entrypoint_id {
        Some(root) if by_id.contains_key(root.as_str()) => Some(call_cone(root, &callees)),
        Some(root) => anyhow::bail!("root_entrypoint_id `{root}` is not a known symbol_id"),
        None => None,
    };
    let in_scope = |s: &SymbolInfo| args.scope_path_prefix.as_deref().is_none_or(|p| s.file_path.starts_with(p));
    let include_types = args.include_types.unwrap_or(false);
    let include_tests = args.include_tests.unwrap_or(false);

    // Queue members: unmapped/stale symbols in scope (and in the cone, if one was requested).
    let queued: Vec<&SymbolInfo> = symbols
        .iter()
        .filter(|s| !s.is_mapped())
        .filter(|s| include_types || s.is_callable())
        .filter(|s| include_tests || !s.is_test())
        .filter(|s| in_scope(s))
        .filter(|s| cone.as_ref().is_none_or(|c| c.contains(s.symbol_id.as_str())))
        .collect();

    // Group by SCC; a symbol without a computed group is its own group.
    let mut groups: BTreeMap<i64, Vec<&SymbolInfo>> = BTreeMap::new();
    let mut next_solo = i64::MIN;
    for s in &queued {
        let key = if s.scc_group_id >= 0 {
            s.scc_group_id
        } else {
            next_solo += 1;
            next_solo
        };
        groups.entry(key).or_default().push(s);
    }

    let group_of = |id: &str| by_id.get(id).map_or(-1, |s| s.scc_group_id);
    let satisfied = |callee: &SymbolInfo| callee.is_mapped() || !in_scope(callee) || !callee.is_callable();

    let mut ready: Vec<(i64, Vec<&SymbolInfo>)> = Vec::new();
    let mut blocked = 0;
    for (g, members) in groups {
        let is_ready = members.iter().all(|m| {
            callees.get(m.symbol_id.as_str()).is_none_or(|cs| {
                cs.iter().all(|c| {
                    (group_of(c) == m.scc_group_id && m.scc_group_id >= 0)
                        || by_id.get(c).is_none_or(|callee| satisfied(callee))
                })
            })
        });
        if is_ready {
            ready.push((g, members));
        } else {
            blocked += 1;
        }
    }

    let prioritize = args.prioritize_high_risk.unwrap_or(false);
    ready.sort_by_key(|(_, m)| {
        let level = m.iter().map(|s| s.topological_level).max().unwrap_or(0);
        let risky = m.iter().any(|s| !s.risk_flags.is_empty() || s.requires_strict_contract);
        let complexity = m.iter().map(|s| s.complexity).max().unwrap_or(0);
        let first = m.iter().map(|s| s.symbol_id.as_str()).min().unwrap_or("").to_string();
        if prioritize {
            (std::cmp::Reverse((risky, complexity)), level, first)
        } else {
            (std::cmp::Reverse((false, 0)), level, first)
        }
    });

    let ready_groups = ready.len();
    let mut chosen: Vec<&SymbolInfo> = Vec::new();
    for (_, mut members) in ready {
        if !chosen.is_empty() && chosen.len() + members.len() > batch_size {
            if chosen.len() >= batch_size {
                break;
            }
            continue;
        }
        members.sort_by(|a, b| a.symbol_id.cmp(&b.symbol_id));
        chosen.extend(members);
        if chosen.len() >= batch_size {
            break;
        }
    }

    let ids: Vec<String> = chosen.iter().map(|s| s.symbol_id.clone()).collect();
    let markdown = render(db, repo_root, &chosen, &by_id, &queued, blocked)?;
    Ok(Batch { markdown, symbol_ids: ids, ready_groups, blocked_groups: blocked, remaining: queued.len() })
}

/// The symbol and everything transitively reachable through live CALLS edges.
fn call_cone<'a>(root: &'a str, callees: &HashMap<&'a str, Vec<&'a str>>) -> HashSet<&'a str> {
    let mut seen = HashSet::from([root]);
    let mut queue = VecDeque::from([root]);
    while let Some(n) = queue.pop_front() {
        for &c in callees.get(n).into_iter().flatten() {
            if seen.insert(c) {
                queue.push_back(c);
            }
        }
    }
    seen
}

fn render(
    db: &DbInstance,
    repo_root: &Path,
    chosen: &[&SymbolInfo],
    by_id: &HashMap<&str, &SymbolInfo>,
    queued: &[&SymbolInfo],
    blocked: usize,
) -> Result<String> {
    let mut out = String::new();
    if chosen.is_empty() {
        let _ = writeln!(
            out,
            "# CARTOGRAPHY BATCH — empty\n\nNo symbols are ready: {} still queued, {} groups blocked on unmapped callees. \
             If nothing is queued the mapping is complete; otherwise call `sync_repository_skeleton` and retry.",
            queued.len(),
            blocked
        );
        return Ok(out);
    }

    let ids: Vec<String> = chosen.iter().map(|s| s.symbol_id.clone()).collect();
    let edges = queries::edges_from(db, &ids)?;
    let mut related: Vec<String> = ids.clone();
    related.extend(edges.iter().map(|e| e.to.clone()));
    related.sort();
    related.dedup();
    let semantics = queries::semantics_by_ids(db, &related)?;
    let contracts = queries::contracts_by_ids(db, &related)?;
    let mut edges_by_from: HashMap<&str, Vec<&StoredEdge>> = HashMap::new();
    for e in &edges {
        edges_by_from.entry(e.from.as_str()).or_default().push(e);
    }
    let file_hashes = queries::file_hashes(db)?;

    let levels = chosen.iter().map(|s| s.topological_level);
    let _ = writeln!(
        out,
        "# CARTOGRAPHY BATCH — {} symbols (topological level {}–{})\n\n\
         {} symbols still queued · {} groups blocked on unmapped callees.\n\
         Submit one model per symbol via `submit_symbol_models`. Treat callee contracts below as \
         ground truth for what callees do and what they assume; verify against the source before \
         trusting them, and verify or prune every `STATIC_HEURISTIC` edge.\n\
         Also set `promote_to_strict_contract` (with a `strict_contract`) for any symbol that handles untrusted input, \
         enforces a privilege check, or performs a sensitive operation (SQL, shell, deserialization, file or memory writes), \
         even when it is marked simple. Mark edges that carry attacker-influenced data with `data_taint_propagated`.\n",
        chosen.len(),
        levels.clone().min().unwrap_or(0),
        levels.max().unwrap_or(0),
        queued.len(),
        blocked
    );

    let mut source_cache: HashMap<String, Option<String>> = HashMap::new();
    for (n, s) in chosen.iter().enumerate() {
        let group_size = chosen.iter().filter(|o| s.scc_group_id >= 0 && o.scc_group_id == s.scc_group_id).count();
        let _ = writeln!(out, "## {}. `{}`", n + 1, s.symbol_id);
        let _ = writeln!(
            out,
            "**Kind:** {} · **Visibility:** {} · **Location:** {}:{}-{} · **Status:** {} · **Complexity:** {} · **Level:** {}{}",
            s.kind,
            s.visibility,
            s.file_path,
            s.start_line,
            s.end_line,
            s.mapping_status,
            s.complexity,
            s.topological_level,
            if group_size > 1 {
                format!(" · **MUTUALLY RECURSIVE with {} others in this batch — map them together**", group_size - 1)
            } else {
                String::new()
            }
        );
        let _ = writeln!(out, "**Risk flags:** {:?}", s.risk_flags);
        if s.requires_strict_contract {
            let _ = writeln!(
                out,
                "**STRICT CONTRACT REQUIRED** (complexity ≥ 5, risk flags, or external entrypoint): include `strict_contract`{}.",
                if s.risk_flags.is_empty() {
                    String::new()
                } else {
                    format!(" and account for {:?} in `invariants_assumed` / `error_surface`", s.risk_flags)
                }
            );
        }
        if !s.doc_comment.is_empty() {
            let _ = writeln!(out, "**Doc:** {}", s.doc_comment.replace('\n', " "));
        }
        let _ = writeln!(out, "**Signature:** `{}`\n", s.signature);

        let source = source_cache
            .entry(s.file_path.clone())
            .or_insert_with(|| read_checked(repo_root, &s.file_path, file_hashes.get(&s.file_path)));
        match source.as_deref().and_then(|t| t.get(s.start_byte..s.end_byte)) {
            Some(code) => {
                let lang = s.file_path.rsplit('.').next().unwrap_or("");
                let (shown, omitted) = clip_source(code, MAX_SOURCE_LINES);
                let _ = writeln!(out, "```{lang}\n{shown}\n```");
                if omitted > 0 {
                    let _ = writeln!(
                        out,
                        "> [{omitted} more lines omitted ({}:{}-{}). Read the file directly for the rest; `expand_function_cfg` shows the branch structure.]",
                        s.file_path,
                        s.start_line + MAX_SOURCE_LINES as i64,
                        s.end_line
                    );
                }
            }
            None => {
                let _ = writeln!(
                    out,
                    "> SOURCE UNAVAILABLE: `{}` changed since the last sync. Call `sync_repository_skeleton` and request the batch again.",
                    s.file_path
                );
            }
        }

        if s.mapping_status == "STALE"
            && let Some(prev) = semantics.get(&s.symbol_id)
        {
            let _ = writeln!(out, "\n**Previous model (code has since changed — re-verify):** {}", prev.intent_summary);
        }

        render_callees(&mut out, s, edges_by_from.get(s.symbol_id.as_str()), by_id, &semantics, &contracts);
        out.push('\n');
    }
    Ok(out)
}

fn render_callees(
    out: &mut String,
    caller: &SymbolInfo,
    edges: Option<&Vec<&StoredEdge>>,
    by_id: &HashMap<&str, &SymbolInfo>,
    semantics: &HashMap<String, Semantics>,
    contracts: &HashMap<String, Contract>,
) {
    let Some(edges) = edges else {
        let _ = writeln!(out, "\n**Callees:** none resolved.");
        return;
    };
    let mut calls: Vec<&&StoredEdge> =
        edges.iter().filter(|e| e.kind == "CALLS" && e.confidence != "LLM_PRUNED").collect();
    calls.sort_by(|a, b| a.to.cmp(&b.to));
    let types: Vec<&&StoredEdge> = edges.iter().filter(|e| e.kind.ends_with("_TYPE")).collect();

    let _ = writeln!(out, "\n**Callees ({}):**", calls.len());
    for e in calls {
        let Some(callee) = by_id.get(e.to.as_str()) else { continue };
        let lines = e.lines.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
        let _ = write!(out, "- `{}` [{}] lines {}", e.to, e.confidence, lines);
        if e.confidence == "STATIC_HEURISTIC" {
            let _ = write!(out, " — **UNVERIFIED: return LLM_VERIFIED or LLM_PRUNED in resolved_edges**");
        }
        if callee.scc_group_id >= 0
            && callee.scc_group_id == caller.scc_group_id
            && callee.symbol_id != caller.symbol_id
        {
            let _ = write!(out, " — same recursive group");
        }
        if !e.snippet.is_empty() {
            let _ = write!(out, "\n  - call: `{}`", e.snippet);
        }
        let _ = writeln!(out, "\n  - signature: `{}`", callee.signature);
        match semantics.get(&e.to) {
            Some(sem) => {
                let _ = writeln!(out, "  - intent: {}", sem.intent_summary);
                if let Some(c) = contracts.get(&e.to) {
                    if !c.invariants_assumed.is_empty() {
                        let _ =
                            writeln!(out, "  - assumes (not checked by callee): {}", c.invariants_assumed.join("; "));
                    }
                    if !c.error_surface.is_empty() {
                        let _ = writeln!(out, "  - error surface: {}", c.error_surface.join("; "));
                    }
                }
            }
            None => {
                let _ = writeln!(out, "  - (callee not yet mapped; rely on its signature and source)");
            }
        }
    }
    if !types.is_empty() {
        let names: Vec<String> = types
            .iter()
            .filter_map(|e| by_id.get(e.to.as_str()).map(|t| format!("{} ({})", t.qualified_name, e.kind)))
            .collect();
        let _ = writeln!(out, "\n**Types referenced:** {}", names.join(", "));
    }
}

/// Keeps the first `max_lines` lines of `code`; returns the text and how many lines were cut.
/// Huge generated tables and parsers would otherwise swamp the agent's context.
pub(crate) fn clip_source(code: &str, max_lines: usize) -> (String, usize) {
    let total = code.lines().count();
    if total <= max_lines {
        return (code.to_string(), 0);
    }
    (code.lines().take(max_lines).collect::<Vec<_>>().join("\n"), total - max_lines)
}

/// Source text of `rel_path`, or `None` when it is unreadable or no longer matches the synced hash.
pub(crate) fn read_checked(repo_root: &Path, rel_path: &str, synced_hash: Option<&String>) -> Option<String> {
    let text = std::fs::read_to_string(repo_root.join(rel_path)).ok()?;
    match synced_hash {
        Some(h) if *h == blake3::hash(text.as_bytes()).to_hex().to_string() => Some(text),
        _ => None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::db::schema;
    use crate::ingestion::sync::sync_repository;

    pub(crate) fn repo(files: &[(&str, &str)]) -> (tempfile::TempDir, DbInstance) {
        let dir = tempfile::tempdir().unwrap();
        for (p, body) in files {
            let path = dir.path().join(p);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        let db = schema::open_in_memory().unwrap();
        sync_repository(&db, dir.path()).unwrap();
        (dir, db)
    }

    fn batch(db: &DbInstance, dir: &Path, args: BatchArgs) -> Batch {
        next_batch(db, dir, &args).unwrap()
    }

    fn mark_mapped(db: &DbInstance, name: &str) {
        db.run_script(
            &format!("?[symbol_id, mapping_status] := *symbol_node{{symbol_id, name: '{name}'}}, mapping_status = 'LIGHTWEIGHT_MAPPED' :update symbol_node {{symbol_id => mapping_status}}"),
            Default::default(),
            cozo::ScriptMutability::Mutable,
        )
        .map_err(crate::db::schema::cozo_err)
        .unwrap();
    }

    const CHAIN: &str = "fn leaf() {}\nfn mid() { leaf(); }\nfn top() { mid(); }\n";

    #[test]
    fn serves_leaves_first_and_unblocks_callers_as_callees_get_mapped() {
        let (dir, db) = repo(&[("src/lib.rs", CHAIN)]);
        let b = batch(&db, dir.path(), BatchArgs { batch_size: Some(5), ..Default::default() });
        assert_eq!(b.symbol_ids.len(), 1);
        assert!(b.symbol_ids[0].contains("::leaf#"));
        assert_eq!(b.remaining, 3);
        assert_eq!(b.blocked_groups, 2);
        assert!(b.markdown.contains("fn leaf() {}"));

        mark_mapped(&db, "leaf");
        let b = batch(&db, dir.path(), BatchArgs::default());
        assert!(b.symbol_ids[0].contains("::mid#"));
        assert!(b.markdown.contains("Callees (1)"));
    }

    #[test]
    fn mutually_recursive_symbols_are_returned_together() {
        let (dir, db) = repo(&[("src/lib.rs", "fn a() { b(); }\nfn b() { a(); }\nfn z() {}\n")]);
        // a/b share an SCC and level 0 with z; the pair sorts first and is never split, even
        // though batch_size is 1.
        let b = batch(&db, dir.path(), BatchArgs { batch_size: Some(1), ..Default::default() });
        assert_eq!(b.symbol_ids.len(), 2);
        assert!(b.markdown.contains("MUTUALLY RECURSIVE"));
        mark_mapped(&db, "a");
        mark_mapped(&db, "b");
        let b = batch(&db, dir.path(), BatchArgs { batch_size: Some(1), ..Default::default() });
        assert_eq!(b.symbol_ids.len(), 1);
        assert!(b.symbol_ids[0].contains("::z#"));
    }

    #[test]
    fn scope_and_cone_restrict_the_queue() {
        let (dir, db) =
            repo(&[("src/a/x.rs", "fn ax() { shared(); }\nfn shared() {}"), ("src/b/y.rs", "fn by() { shared(); }")]);
        let b = batch(&db, dir.path(), BatchArgs { scope_path_prefix: Some("src/b".into()), ..Default::default() });
        // `shared` is out of scope, so it no longer blocks `by`.
        assert_eq!(b.symbol_ids.len(), 1);
        assert!(b.symbol_ids[0].contains("::by#"));

        let ax = queries::all_symbols(&db).unwrap().into_iter().find(|s| s.name == "ax").unwrap();
        let b = batch(
            &db,
            dir.path(),
            BatchArgs { root_entrypoint_id: Some(ax.symbol_id), batch_size: Some(15), ..Default::default() },
        );
        assert_eq!(b.remaining, 2);
    }

    #[test]
    fn heuristic_edges_are_flagged_for_verification_and_risk_prioritised() {
        let src = "struct A; struct B;\nimpl A { fn run(&self) {} }\nimpl B { fn run(&self) {} }\n\
                   fn caller(x: A) { x.run(); }\nfn risky(p: *const u8) -> u8 { unsafe { *p } }\n";
        let (dir, db) = repo(&[("src/lib.rs", src)]);
        mark_mapped(&db, "run"); // marks both `run` methods
        let b = batch(
            &db,
            dir.path(),
            BatchArgs { prioritize_high_risk: Some(true), batch_size: Some(1), ..Default::default() },
        );
        assert!(b.symbol_ids[0].contains("::risky#"), "{:?}", b.symbol_ids);
        assert!(b.markdown.contains("STRICT CONTRACT REQUIRED"));

        mark_mapped(&db, "risky");
        let b = batch(&db, dir.path(), BatchArgs::default());
        assert!(b.symbol_ids[0].contains("::caller#"));
        assert!(b.markdown.contains("UNVERIFIED"));
    }

    #[test]
    fn test_code_is_excluded_unless_requested() {
        let (dir, db) = repo(&[
            ("src/lib.rs", "fn real() {}\n#[cfg(test)]\nmod tests {\n    fn helper() {}\n}\n"),
            ("tests/api.rs", "fn integration() {}"),
            ("web/user.spec.ts", "function spec() {}"),
        ]);
        let b = batch(&db, dir.path(), BatchArgs { batch_size: Some(15), ..Default::default() });
        assert_eq!(b.symbol_ids.len(), 1, "{:?}", b.symbol_ids);
        assert!(b.symbol_ids[0].contains("::real#"));
        let b =
            batch(&db, dir.path(), BatchArgs { batch_size: Some(15), include_tests: Some(true), ..Default::default() });
        assert_eq!(b.symbol_ids.len(), 4);
    }

    #[test]
    fn stale_file_withholds_source() {
        let (dir, db) = repo(&[("src/lib.rs", "fn f() {}\n")]);
        std::fs::write(dir.path().join("src/lib.rs"), "// moved\nfn f() {}\n").unwrap();
        let b = batch(&db, dir.path(), BatchArgs::default());
        assert!(b.markdown.contains("SOURCE UNAVAILABLE"));
    }

    #[test]
    fn empty_queue_says_so_and_unknown_root_is_an_error() {
        let (dir, db) = repo(&[("src/lib.rs", "fn f() {}\n")]);
        mark_mapped(&db, "f");
        let b = batch(&db, dir.path(), BatchArgs::default());
        assert!(b.symbol_ids.is_empty() && b.markdown.contains("empty"));
        assert!(
            next_batch(&db, dir.path(), &BatchArgs { root_entrypoint_id: Some("nope".into()), ..Default::default() })
                .is_err()
        );
    }
}
