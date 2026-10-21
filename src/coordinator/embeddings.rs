//! Local CPU embeddings (spec §2.2). Always run *before* a Cozo write transaction is opened
//! (spec §10.4); callers hand the finished vectors to the writers.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};

use crate::db::schema::EMBEDDING_DIM;

pub trait Embedder: Send + Sync {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    fn name(&self) -> &'static str;
}

/// `BAAI/bge-small-en-v1.5` via ONNX. The model downloads on first use, so loading is lazy and
/// a failure only affects tools that need vectors.
pub struct FastEmbedder {
    model: Mutex<Option<TextEmbedding>>,
    cache_dir: PathBuf,
}

impl FastEmbedder {
    pub fn new(cache_dir: PathBuf) -> Self {
        Self { model: Mutex::new(None), cache_dir }
    }
}

impl Embedder for FastEmbedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut guard = self.model.lock().map_err(|_| anyhow!("embedder mutex poisoned"))?;
        if guard.is_none() {
            let opts = TextInitOptions::new(EmbeddingModel::BGESmallENV15)
                .with_cache_dir(self.cache_dir.clone())
                .with_show_download_progress(false);
            let model = TextEmbedding::try_new(opts).with_context(|| {
                format!(
                    "loading bge-small-en-v1.5 (cache {}); set CODE_ATLAS_EMBEDDER=hash to use the offline fallback",
                    self.cache_dir.display()
                )
            })?;
            *guard = Some(model);
        }
        let model = guard.as_mut().expect("just initialised");
        let vecs = model.embed(texts, Some(32)).context("embedding batch")?;
        if let Some(bad) = vecs.iter().find(|v| v.len() != EMBEDDING_DIM) {
            return Err(anyhow!("model produced {}-dim vectors, schema expects {EMBEDDING_DIM}", bad.len()));
        }
        Ok(vecs)
    }

    fn name(&self) -> &'static str {
        "fastembed:bge-small-en-v1.5"
    }
}

/// Deterministic bag-of-words hashing embedder: no model download, similar texts land close
/// together. Used by tests and as an explicit offline fallback (much weaker than a real model).
pub struct HashEmbedder;

impl HashEmbedder {
    fn tokens(text: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut prev_lower = false;
        for ch in text.chars() {
            if ch.is_alphanumeric() {
                // split camelCase boundaries
                if ch.is_uppercase() && prev_lower && !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                prev_lower = ch.is_lowercase();
                cur.extend(ch.to_lowercase());
            } else {
                prev_lower = false;
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
        out
    }
}

impl Embedder for HashEmbedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|t| {
                let mut v = vec![0f32; EMBEDDING_DIM];
                for tok in Self::tokens(t) {
                    let h = blake3::hash(tok.as_bytes());
                    let b = h.as_bytes();
                    let idx = u16::from_le_bytes([b[0], b[1]]) as usize % EMBEDDING_DIM;
                    v[idx] += if b[2] & 1 == 0 { 1.0 } else { -1.0 };
                }
                let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                if norm > 0.0 {
                    v.iter_mut().for_each(|x| *x /= norm);
                } else {
                    v[0] = 1.0; // keep cosine distance defined for empty text
                }
                v
            })
            .collect())
    }

    fn name(&self) -> &'static str {
        "hash-fallback"
    }
}

fn default_cache_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("CODE_ATLAS_MODEL_CACHE") {
        return dir.into();
    }
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("code-atlas-mcp").join("models")
}

/// `CODE_ATLAS_EMBEDDER=hash` selects the offline fallback; anything else uses fastembed.
pub fn from_env() -> Arc<dyn Embedder> {
    match std::env::var("CODE_ATLAS_EMBEDDER").as_deref() {
        Ok("hash") => Arc::new(HashEmbedder),
        _ => Arc::new(FastEmbedder::new(default_cache_dir())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cos(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    #[test]
    fn hash_embedder_is_normalised_deterministic_and_topical() {
        let e = HashEmbedder;
        let v = e
            .embed(&[
                "validates JWT expiration timestamps".into(),
                "validates jwt expiration timestamps".into(),
                "renders html templates for the billing page".into(),
                String::new(),
            ])
            .unwrap();
        assert!(v.iter().all(|x| x.len() == EMBEDDING_DIM));
        assert!((cos(&v[0], &v[0]) - 1.0).abs() < 1e-5);
        assert!(cos(&v[0], &v[1]) > 0.99);
        assert!(cos(&v[0], &v[2]) < 0.5);
        assert!(v[3].iter().all(|x| x.is_finite()));
    }

    #[test]
    fn camel_and_snake_case_split_into_words() {
        assert_eq!(HashEmbedder::tokens("verifyToken_jwtService"), ["verify", "token", "jwt", "service"]);
    }

    #[test]
    fn float_list_params_coerce_into_vector_columns() {
        use std::collections::BTreeMap;
        let db = crate::db::schema::open_in_memory().unwrap();
        let v = HashEmbedder.embed(&["hello world".into()]).unwrap().remove(0);
        let mut params = BTreeMap::new();
        params.insert("e".to_string(), crate::db::dv_floats(&v));
        db.run_script(
            "?[module_path, parent_module_path, architectural_role, key_abstractions, cross_module_contracts, \
              known_architectural_risks, embedding] <- [['m', '', 'r', [], [], [], $e]] \
             :put module_rollup { module_path => parent_module_path, architectural_role, key_abstractions, \
              cross_module_contracts, known_architectural_risks, embedding }",
            params,
            cozo::ScriptMutability::Mutable,
        )
        .map_err(crate::db::schema::cozo_err)
        .unwrap();
    }

    #[test]
    #[ignore = "downloads bge-small-en-v1.5 (~130 MB) on first run"]
    fn real_model_embeds_384_dims_and_ranks_by_meaning() {
        let e = FastEmbedder::new(default_cache_dir());
        let v = e
            .embed(&[
                "validates JWT expiration".to_string(),
                "checks whether an access token has timed out".to_string(),
                "renders an HTML invoice page".to_string(),
            ])
            .unwrap();
        assert!(v.iter().all(|x| x.len() == EMBEDDING_DIM));
        assert!(cos(&v[0], &v[1]) > cos(&v[0], &v[2]), "synonymous phrasing must be closer than an unrelated one");
    }
}
