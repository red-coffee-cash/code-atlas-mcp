//! MCP surface (spec §7): thin async shims over the synchronous tool logic. Blocking Cozo and
//! ONNX work runs on the blocking pool so the stdio loop stays responsive.

use std::sync::Arc;

use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ErrorData, PromptMessage, Role};
use rmcp::{ServerHandler, prompt, prompt_handler, prompt_router, tool, tool_handler, tool_router};

use rmcp::schemars::{self, JsonSchema};
use serde::Deserialize;

use super::architecture::{self, FlowArgs, RollupArgs};
use super::atlas::{self, AtlasArgs};
use super::audit::{self, AuditArgs, TraceArgs};
use super::batch::{self, BatchArgs};
use super::prompts::{self, AuditorArgs, CartographerArgs};
use super::search::{self, SearchArgs};
use super::submit::{self, SubmitArgs, SubmitError};
use crate::folding::pagination::Cursor;
use crate::folding::renderer::{self, ZoomOptions};
use crate::ingestion::cfg;
use crate::ingestion::sync::sync_repository;
use crate::state::AppState;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InspectArgs {
    /// Up to 5 exact symbol_ids.
    pub symbol_ids: Vec<String>,
    /// Include the symbol's source code (default true).
    pub include_raw_source: Option<bool>,
    /// Neighbours shown per direction (default 12, max 50).
    pub max_neighbors_per_direction: Option<usize>,
    /// Cursor from a previous result's pagination notice (single symbol only).
    pub neighbor_cursor: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CfgArgs {
    /// Exact symbol_id of a function or method.
    pub symbol_id: String,
}

#[derive(Clone)]
pub struct AtlasServer {
    state: Arc<AppState>,
    #[allow(dead_code)] // read by the generated ServerHandler
    tool_router: ToolRouter<Self>,
}

fn text(s: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(s)])
}

fn tool_error(s: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(s)])
}

fn json<T: serde::Serialize>(v: &T) -> CallToolResult {
    match serde_json::to_string_pretty(v) {
        Ok(s) => text(s),
        Err(e) => tool_error(format!("could not serialise result: {e}")),
    }
}

fn validation_failed(errors: &[crate::db::queries::ValidationError]) -> CallToolResult {
    tool_error(
        serde_json::to_string_pretty(&serde_json::json!({
            "error": "validation_failed",
            "note": "Nothing was written. Fix every listed field and resubmit the full payload.",
            "errors": errors,
        }))
        .unwrap_or_else(|_| "validation_failed".into()),
    )
}

impl AtlasServer {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state, tool_router: Self::tool_router() + Self::consumer_router() }
    }

    /// Runs `f` on the blocking pool. An `Err` becomes a tool-level error the agent can read;
    /// only a panicked task is reported as a protocol error.
    async fn blocking<F>(&self, f: F) -> Result<CallToolResult, ErrorData>
    where
        F: FnOnce(&AppState) -> anyhow::Result<CallToolResult> + Send + 'static,
    {
        let state = self.state.clone();
        match tokio::task::spawn_blocking(move || f(&state)).await {
            Ok(Ok(r)) => Ok(r),
            Ok(Err(e)) => Ok(tool_error(format!("{e:#}"))),
            Err(e) => Err(ErrorData::internal_error(format!("tool task failed: {e}"), None)),
        }
    }
}

fn lock(state: &AppState) -> std::sync::MutexGuard<'_, ()> {
    state.write_lock.lock().unwrap_or_else(|p| p.into_inner())
}

#[tool_router(router = tool_router)]
impl AtlasServer {
    #[tool(
        description = "Runs parallel tree-sitter ingestion over the repository, computes normalized BLAKE3 AST hashes, \
        rebuilds static and heuristic call edges, marks symbols whose code changed as STALE, and recomputes the \
        Tarjan SCC / topological queue. Mental models and LLM edge verdicts for unchanged code are preserved. \
        Call this first and again after the code changes."
    )]
    async fn sync_repository_skeleton(&self) -> Result<CallToolResult, ErrorData> {
        self.blocking(|s| {
            let _guard = lock(s);
            Ok(json(&sync_repository(&s.db, &s.repo_root)?))
        })
        .await
    }

    #[tool(
        description = "Fetches the next batch of unmapped or stale symbols in bottom-up topological order (or scoped to a \
        path/entrypoint), including full source, risk flags, and the already-mapped contracts of their downstream callees."
    )]
    async fn get_next_cartography_batch(
        &self,
        Parameters(args): Parameters<BatchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| Ok(text(batch::next_batch(&s.db, &s.repo_root, &args)?.markdown))).await
    }

    #[tool(
        description = "Persists the natural-language mental model, strict audit contract, and resolved edge relationships for \
        a batch of symbols in one atomic transaction, and generates local embeddings. Rejects the whole submission with a \
        structured error if a symbol that requires a strict contract lacks one."
    )]
    async fn submit_symbol_models(
        &self,
        Parameters(args): Parameters<SubmitArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            let _guard = lock(s);
            match submit::submit_symbol_models(&s.db, s.embedder.as_ref(), args) {
                Ok(report) => Ok(json(&report)),
                Err(SubmitError::Invalid(errors)) => Ok(validation_failed(&errors)),
                Err(SubmitError::Internal(e)) => Err(e),
            }
        })
        .await
    }

    #[tool(
        description = "Writes a bottom-up architectural summary for a directory or file once its constituent symbols are mapped."
    )]
    async fn submit_module_rollup(
        &self,
        Parameters(args): Parameters<RollupArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            let _guard = lock(s);
            Ok(match architecture::submit_module_rollup(&s.db, s.embedder.as_ref(), args)? {
                Ok(report) => json(&report),
                Err(errors) => validation_failed(&errors),
            })
        })
        .await
    }

    #[tool(description = "Registers a top-down execution flow pipeline and its ordered steps across the call graph. \
        Re-recording an existing flow_id replaces it.")]
    async fn record_execution_flow(&self, Parameters(args): Parameters<FlowArgs>) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            let _guard = lock(s);
            Ok(match architecture::record_execution_flow(&s.db, s.embedder.as_ref(), args)? {
                Ok(report) => json(&report),
                Err(errors) => validation_failed(&errors),
            })
        })
        .await
    }
}

#[tool_router(router = consumer_router)]
impl AtlasServer {
    #[tool(
        description = "Returns the big-picture view: module rollups, registered execution flows, entrypoints, and mapping \
        coverage statistics (mapped / stale / unmapped, unverified heuristic edges, high-risk contracts). Start here."
    )]
    async fn get_macro_atlas(&self, Parameters(args): Parameters<AtlasArgs>) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| Ok(text(atlas::macro_atlas(&s.db, &args)?))).await
    }

    #[tool(
        description = "PRIMARY NAVIGATION TOOL. Renders a context-folded mental model around up to 5 symbols: Distance 0 \
        (semantic contract, audit smells, full source), Distance 1 (callers and callees with edge semantics, taint, and callee \
        assumptions; high-fanout hubs are paginated), and Distance 2+ (module, execution flows, reachable entrypoints)."
    )]
    async fn inspect_symbol_zoom(
        &self,
        Parameters(args): Parameters<InspectArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            let cursor = args.neighbor_cursor.as_deref().map(Cursor::parse).transpose()?;
            let opts = ZoomOptions {
                include_raw_source: args.include_raw_source.unwrap_or(true),
                max_neighbors: args.max_neighbors_per_direction.unwrap_or(renderer::DEFAULT_NEIGHBORS),
                cursor,
            };
            Ok(text(renderer::render_zoom(&s.db, &s.repo_root, &args.symbol_ids, &opts)?))
        })
        .await
    }

    #[tool(
        description = "Rebuilds the sub-function Control Flow Graph of one complex function from current source: basic blocks \
        with line ranges, TRUE/FALSE branches, match arms, loop back-edges and exits, early returns, panics and `?` propagation, \
        plus mutated locals and risky/external calls per block."
    )]
    async fn expand_function_cfg(&self, Parameters(args): Parameters<CfgArgs>) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| Ok(text(cfg::expand_function_cfg(&s.db, &s.repo_root, &args.symbol_id)?))).await
    }

    #[tool(
        description = "Finds the shortest, highest-risk, or taint-only call paths between a source and target symbol (or from any \
        trust boundary to any sink when both are omitted) and renders the semantic chain hop by hop."
    )]
    async fn trace_connective_paths(
        &self,
        Parameters(args): Parameters<TraceArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| Ok(text(audit::trace_connective_paths(&s.db, &args)?))).await
    }

    #[tool(
        description = "Structured bug-hunting queries over the mental model: HIGH_RISK_CONTRACTS, TRUST_BOUNDARY_TO_SINK_PATHS \
        (taint reachability), UNCHECKED_ASSUMPTION_HOTSPOTS (callee preconditions vs caller guards), AMBIGUOUS_UNVERIFIED_EDGES."
    )]
    async fn query_audit_surface(&self, Parameters(args): Parameters<AuditArgs>) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| Ok(text(audit::query_audit_surface(&s.db, &args)?))).await
    }

    #[tool(
        description = "Hybrid vector + full-text search over mapped symbols, module rollups and execution flows for conceptual \
        queries such as \"where are JWT expiration timestamps validated?\"."
    )]
    async fn semantic_atlas_search(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| Ok(text(search::semantic_atlas_search(&s.db, s.embedder.as_ref(), &args)?))).await
    }
}

#[prompt_router]
impl AtlasServer {
    #[prompt(
        name = "cartographer_prompt",
        description = "Operating manual for the Producer agent: how to iterate get_next_cartography_batch, write adversarial \
            symbol models, verify or prune heuristic edges, and stitch execution flows."
    )]
    async fn cartographer_prompt(&self, Parameters(args): Parameters<CartographerArgs>) -> Vec<PromptMessage> {
        vec![PromptMessage::new_text(Role::User, prompts::cartographer_text(&args))]
    }

    #[prompt(
        name = "auditor_prompt",
        description = "Operating manual for the Consumer agent: how to hunt bugs with the audit-surface, taint-path, search, \
            zoom and CFG tools and verify findings against source."
    )]
    async fn auditor_prompt(&self, Parameters(args): Parameters<AuditorArgs>) -> Vec<PromptMessage> {
        vec![PromptMessage::new_text(Role::User, prompts::auditor_text(&args))]
    }
}

#[tool_handler(
    router = self.tool_router,
    name = "code-atlas-mcp",
    instructions = "Persistent structural and semantic mental model of a codebase. \
        Cartographer (build): sync_repository_skeleton, then loop get_next_cartography_batch / submit_symbol_models until the queue is empty, \
        then submit_module_rollup and record_execution_flow. \
        Auditor (query): get_macro_atlas, query_audit_surface, trace_connective_paths, semantic_atlas_search, then inspect_symbol_zoom \
        and expand_function_cfg on suspects."
)]
#[prompt_handler]
impl ServerHandler for AtlasServer {}
