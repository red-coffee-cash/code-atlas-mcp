//! Shared server state (spec §2.2 `state.rs`).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use cozo::DbInstance;

use crate::coordinator::embeddings::Embedder;

pub struct AppState {
    pub repo_root: PathBuf,
    pub db: DbInstance,
    pub embedder: Arc<dyn Embedder>,
    /// Serialises sync and submit so a re-sync can't interleave with a Cartographer write.
    pub write_lock: Mutex<()>,
}

impl AppState {
    pub fn new(repo_root: PathBuf, db: DbInstance, embedder: Arc<dyn Embedder>) -> Self {
        Self { repo_root, db, embedder, write_lock: Mutex::new(()) }
    }
}
