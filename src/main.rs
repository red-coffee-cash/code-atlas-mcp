mod coordinator;
mod db;
mod folding;
mod ingestion;
mod mcp;
mod state;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use rmcp::{ServiceExt, transport::stdio};

/// `code-atlas-mcp [--repo <path>]`; defaults to the current directory.
fn repo_root() -> Result<PathBuf> {
    let mut args = std::env::args().skip(1);
    let path = match args.next().as_deref() {
        Some("--repo") => PathBuf::from(args.next().context("--repo needs a path")?),
        Some(other) => anyhow::bail!("unknown argument `{other}`; usage: code-atlas-mcp [--repo <path>]"),
        None => std::env::current_dir()?,
    };
    path.canonicalize().with_context(|| format!("repo path {}", path.display()))
}

#[tokio::main]
async fn main() -> Result<()> {
    // stdout carries the MCP protocol, so logs go to stderr.
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let root = repo_root()?;
    let db = db::schema::open(&root)?;
    let embedder = coordinator::embeddings::from_env();
    tracing::info!("atlas for {} (embedder: {})", root.display(), embedder.name());

    let state = Arc::new(state::AppState::new(root, db, embedder));
    let service = mcp::server::AtlasServer::new(state).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
