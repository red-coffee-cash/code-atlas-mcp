//! CozoDB schema bootstrap: stored relations, HNSW and FTS indices (spec §3.2).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use cozo::{DataValue, DbInstance, ScriptMutability};

/// Bumped whenever a relation or index definition changes.
pub const SCHEMA_VERSION: i64 = 1;

pub const EMBEDDING_DIM: usize = 384;

const DB_DIR: &str = ".code_atlas";
const DB_FILE: &str = "atlas.cozo";

struct Relation {
    name: &'static str,
    create: &'static str,
}

struct Index {
    relation: &'static str,
    name: &'static str,
    create: &'static str,
}

/// Cozo runs one statement per script, so every relation and index is its own entry.
const RELATIONS: &[Relation] = &[
    Relation { name: "atlas_meta", create: ":create atlas_meta { key: String => value: String }" },
    Relation {
        name: "file_node",
        create: r#"
            :create file_node {
                file_path: String
                =>
                language: String,
                file_blake3: String,
                parent_module_path: String,
                line_count: Int,
                indexed_at_ms: Int
            }"#,
    },
    Relation {
        name: "symbol_node",
        create: r#"
            :create symbol_node {
                symbol_id: String
                =>
                qualified_name: String,
                name: String,
                kind: String,
                visibility: String,
                file_path: String,
                start_byte: Int,
                end_byte: Int,
                start_line: Int,
                end_line: Int,
                signature: String,
                doc_comment: String,
                ast_blake3: String,
                cyclomatic_complexity: Int,
                risk_flags: [String],
                requires_strict_contract: Bool,
                scc_group_id: Int,
                topological_level: Int,
                mapping_status: String,
                is_entrypoint: Bool
            }"#,
    },
    Relation {
        name: "symbol_edge",
        create: r#"
            :create symbol_edge {
                from_symbol_id: String,
                to_symbol_id: String,
                edge_kind: String
                =>
                confidence: String,
                call_site_lines: [Int],
                call_expression_snippet: String,
                edge_semantics: String,
                data_taint_propagated: Bool
            }"#,
    },
    Relation {
        name: "symbol_semantics",
        create: r#"
            :create symbol_semantics {
                symbol_id: String
                =>
                intent_summary: String,
                mechanism_prose: String,
                domain_tags: [String],
                annotated_ast_blake3: String,
                embedding: <F32; 384>
            }"#,
    },
    Relation {
        name: "strict_audit_contract",
        create: r#"
            :create strict_audit_contract {
                symbol_id: String
                =>
                invariants_assumed: [String],
                invariants_enforced: [String],
                state_mutations: [String],
                error_surface: [String],
                concurrency_assumptions: [String],
                trust_boundary: Bool,
                taint_sources: [String],
                taint_sinks: [String],
                audit_smells: [String],
                risk_score: Int
            }"#,
    },
    Relation {
        name: "module_rollup",
        create: r#"
            :create module_rollup {
                module_path: String
                =>
                parent_module_path: String,
                architectural_role: String,
                key_abstractions: [String],
                cross_module_contracts: [String],
                known_architectural_risks: [String],
                embedding: <F32; 384>
            }"#,
    },
    Relation {
        name: "execution_flow",
        create: r#"
            :create execution_flow {
                flow_id: String
                =>
                name: String,
                entrypoint_symbol_ids: [String],
                terminal_sink_symbol_ids: [String],
                narrative_summary: String,
                state_lifecycle_summary: String,
                security_boundaries_crossed: [String],
                embedding: <F32; 384>
            }"#,
    },
    Relation {
        name: "flow_step",
        create: r#"
            :create flow_step {
                flow_id: String,
                symbol_id: String
                =>
                step_order: Int,
                role_in_flow: String,
                critical_assumptions_at_step: String
            }"#,
    },
];

const INDICES: &[Index] = &[
    Index {
        relation: "symbol_semantics",
        name: "vec_idx",
        create: r#"
            ::hnsw create symbol_semantics:vec_idx {
                dim: 384, m: 16, ef_construction: 200,
                fields: [embedding], distance: Cosine
            }"#,
    },
    Index {
        relation: "symbol_semantics",
        name: "fts_idx",
        create: r#"
            ::fts create symbol_semantics:fts_idx {
                extractor: mechanism_prose, tokenizer: Simple
            }"#,
    },
    Index {
        relation: "module_rollup",
        name: "module_vec_idx",
        create: r#"
            ::hnsw create module_rollup:module_vec_idx {
                dim: 384, m: 16, ef_construction: 200,
                fields: [embedding], distance: Cosine
            }"#,
    },
    Index {
        relation: "execution_flow",
        name: "flow_vec_idx",
        create: r#"
            ::hnsw create execution_flow:flow_vec_idx {
                dim: 384, m: 16, ef_construction: 200,
                fields: [embedding], distance: Cosine
            }"#,
    },
];

/// Cozo errors are `miette::Report`s, which don't implement `std::error::Error`.
pub(crate) fn cozo_err(e: impl std::fmt::Debug) -> anyhow::Error {
    anyhow!("{e:?}")
}

/// `<repo_root>/.code_atlas/atlas.cozo`
pub fn db_path(repo_root: &Path) -> PathBuf {
    repo_root.join(DB_DIR).join(DB_FILE)
}

/// Opens (creating if needed) the on-disk atlas for `repo_root` and bootstraps the schema.
pub fn open(repo_root: &Path) -> Result<DbInstance> {
    let path = db_path(repo_root);
    std::fs::create_dir_all(path.parent().expect("db path has a parent"))
        .with_context(|| format!("creating {}", path.display()))?;
    let db = DbInstance::new("rocksdb", &path, "").map_err(cozo_err)?;
    bootstrap(&db)?;
    Ok(db)
}

/// In-memory atlas with the full schema, for tests.
#[cfg(test)]
pub fn open_in_memory() -> Result<DbInstance> {
    let db = DbInstance::new("mem", "", "").map_err(cozo_err)?;
    bootstrap(&db)?;
    Ok(db)
}

/// Creates whatever relations and indices are missing, then checks the stored schema version.
/// Safe to call on every startup.
pub fn bootstrap(db: &DbInstance) -> Result<()> {
    let existing = column_strings(db, "::relations", 0)?;

    for rel in RELATIONS {
        if !existing.iter().any(|n| n == rel.name) {
            run(db, rel.create).with_context(|| format!("creating relation {}", rel.name))?;
        }
    }
    for idx in INDICES {
        let have = column_strings(db, &format!("::indices {}", idx.relation), 0)?;
        if !have.iter().any(|n| n == idx.name) {
            run(db, idx.create).with_context(|| format!("creating index {}:{}", idx.relation, idx.name))?;
        }
    }
    check_schema_version(db)
}

fn check_schema_version(db: &DbInstance) -> Result<()> {
    let rows = db
        .run_script(
            "?[value] := *atlas_meta{key: 'schema_version', value}",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )
        .map_err(cozo_err)?;

    match rows.rows.first().and_then(|r| r[0].get_str()) {
        None => {
            let mut params = BTreeMap::new();
            params.insert("v".to_string(), DataValue::from(SCHEMA_VERSION.to_string()));
            db.run_script(
                "?[key, value] <- [['schema_version', $v]] :put atlas_meta { key => value }",
                params,
                ScriptMutability::Mutable,
            )
            .map_err(cozo_err)?;
            Ok(())
        }
        Some(v) if v == SCHEMA_VERSION.to_string() => Ok(()),
        Some(v) => Err(anyhow!(
            "atlas schema version {v} on disk, this build expects {SCHEMA_VERSION}; re-sync into a fresh .code_atlas"
        )),
    }
}

fn run(db: &DbInstance, script: &str) -> Result<()> {
    db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable).map(|_| ()).map_err(cozo_err)
}

fn column_strings(db: &DbInstance, script: &str, col: usize) -> Result<Vec<String>> {
    let rows = db.run_script(script, BTreeMap::new(), ScriptMutability::Immutable).map_err(cozo_err)?;
    Ok(rows.rows.iter().filter_map(|r| r[col].get_str().map(str::to_owned)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_creates_all_relations_and_indices() {
        let db = open_in_memory().unwrap();

        let rels = column_strings(&db, "::relations", 0).unwrap();
        for rel in RELATIONS {
            assert!(rels.iter().any(|n| n == rel.name), "missing {}", rel.name);
        }
        for idx in INDICES {
            let have = column_strings(&db, &format!("::indices {}", idx.relation), 0).unwrap();
            assert!(have.iter().any(|n| n == idx.name), "missing {}", idx.name);
        }
    }

    #[test]
    fn bootstrap_is_idempotent() {
        let db = open_in_memory().unwrap();
        bootstrap(&db).unwrap();
        bootstrap(&db).unwrap();
    }

    #[test]
    fn schema_version_mismatch_is_rejected() {
        let db = open_in_memory().unwrap();
        run(&db, "?[key, value] <- [['schema_version', '0']] :put atlas_meta { key => value }").unwrap();
        let err = bootstrap(&db).unwrap_err().to_string();
        assert!(err.contains("schema version"), "{err}");
    }

    #[test]
    fn embedding_columns_accept_384_dim_vectors_and_search() {
        let db = open_in_memory().unwrap();
        let mut v = vec![0.0f32; EMBEDDING_DIM];
        v[0] = 1.0;
        let vec_lit = format!("{:?}", v.iter().map(|x| *x as f64).collect::<Vec<_>>());
        db.run_script(
            &format!(
                "?[symbol_id, intent_summary, mechanism_prose, domain_tags, annotated_ast_blake3, embedding] \
                 <- [['s1', 'i', 'validates jwt expiry', ['jwt'], 'h', vec({vec_lit})]] \
                 :put symbol_semantics {{ symbol_id => intent_summary, mechanism_prose, domain_tags, annotated_ast_blake3, embedding }}"
            ),
            BTreeMap::new(),
            ScriptMutability::Mutable,
        )
        .map_err(cozo_err)
        .unwrap();

        let hits = db
            .run_script(
                &format!(
                    "?[symbol_id] := ~symbol_semantics:vec_idx{{ symbol_id | query: vec({vec_lit}), k: 1, ef: 20 }}"
                ),
                BTreeMap::new(),
                ScriptMutability::Immutable,
            )
            .map_err(cozo_err)
            .unwrap();
        assert_eq!(hits.rows.len(), 1);

        let fts = db
            .run_script(
                "?[symbol_id] := ~symbol_semantics:fts_idx{ symbol_id | query: 'jwt', k: 5 }",
                BTreeMap::new(),
                ScriptMutability::Immutable,
            )
            .map_err(cozo_err)
            .unwrap();
        assert_eq!(fts.rows.len(), 1);
    }

    #[test]
    fn on_disk_db_persists_across_reopen() {
        let dir = std::env::temp_dir().join(format!("atlas-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let db = open(&dir).unwrap();
            run(&db, "?[key, value] <- [['k', 'v']] :put atlas_meta { key => value }").unwrap();
        }
        let db = open(&dir).unwrap();
        let rows = column_strings(&db, "?[value] := *atlas_meta{key: 'k', value}", 0).unwrap();
        assert_eq!(rows, vec!["v".to_string()]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
