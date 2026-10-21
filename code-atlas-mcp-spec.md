# Technical Specification: `code-atlas-mcp`

**Version:** 1.0.0-draft

**Target Stack:** Rust (`rmcp`, `tokio`, `tree-sitter`, `petgraph`, `cozo`, `fastembed`, `blake3`)

**Primary Objective:** Enable LLMs to construct, persist, and query a multi-tier structural and semantic mental model of a codebase, optimized for autonomous bug hunting, security auditing, and deep architectural reasoning.

---

## 1. Executive Summary & Core Architecture

### 1.1 Problem Statement

LLMs reason over linear token streams, whereas source code is a high-dimensional hypergraph of lexical scopes, cross-file call topologies, type contracts, and state mutations. Existing MCP tools force agents into two failure modes:

1. **Context Starvation (`grep` / file slicing):** The agent spends dozens of turns guessing paths and reading 600-line files to locate a 15-line function, losing track of cross-file invariants.
2. **Syntactic Overload (raw AST/LSP dumps):** Dumping raw compiler structures floods the context window with low-level syntax while hiding *semantic intent* and *implicit assumptions*.

### 1.2 The Two-Phase Producer/Consumer Paradigm

`code-atlas-mcp` decouples **building the mental model** from **auditing/querying the mental model** while allowing both workflows to coexist seamlessly:

1. **Phase 0 — Deterministic Skeleton Ingestion (Rust Engine):** Zero-config `tree-sitter` parsing extracts files, symbols, signatures, cyclomatic complexity, risk flags, and static/heuristic call edges in milliseconds.
2. **Phase 1 — Cartographer Pass (Producer Agent):** An LLM agent uses MCP orchestration tools to traverse the codebase—either via an **MCP-driven bottom-up topological queue** (leaf functions up to entrypoints) or **agent-driven free exploration**—writing structured natural-language mental models, verifying ambiguous static edges, and stitching together top-down `ExecutionFlow` pipelines.
3. **Phase 2 — Auditor Pass (Consumer Agent):** A bug-hunting or code-review agent queries the persisted CozoDB graph using **hybrid semantic search (HNSW vectors + FTS)**, **structured audit predicates**, **Datalog reachability queries**, and **progressive context-folding** (Distance 0/1/2+ zoom levels) plus **on-demand sub-function Control Flow Graph (CFG) expansion**.

---

## 2. System Topology & Rust Crate Stack

### 2.1 System Architecture Diagram

```
+-----------------------------------------------------------------------------+
|                           HOST LLM CLIENT / HARNESS                         |
|    [Cartographer Agent (Producer)]          [Auditor Agent (Consumer)]      |
+-------------------+-------------------------------------+-------------------+
                    | MCP Write & Queue Tools             | MCP Query & Zoom Tools
                    v                                     v
+-----------------------------------------------------------------------------+
|                       code-atlas-mcp (Rust / tokio / rmcp)                  |
|                                                                             |
|  +-------------------------------+     +---------------------------------+  |
|  |    Static Ingestion Engine    |     |    Cognitive Folding Engine     |  |
|  | - Multi-lang tree-sitter      |     | - Distance 0/1/2+ Auto-Folder   |  |
|  | - Normalized BLAKE3 Hasher    |     | - High-Fanout Hub Paginator     |  |
|  | - Complexity & Risk Heuristics|     | - On-Demand Sub-Fn CFG Builder  |  |
|  +---------------+---------------+     +----------------+----------------+  |
|                  |                                      ^                   |
|                  v                                      |                   |
|  +---------------+--------------------------------------+----------------+  |
|  |              In-Memory Graph Coordinator (petgraph + fastembed)       |  |
|  | - Tarjan SCC Cycle Condensation & Topological Batch Scheduler         |  |
|  | - Local CPU Embedding Pipeline (BAAI/bge-small-en-v1.5, 384-dim)      |  |
|  +---------------+--------------------------------------+----------------+  |
|                  |                                      ^                   |
|                  v                                      |                   |
|  +---------------+--------------------------------------+----------------+  |
|  |                Embedded CozoDB Engine (RocksDB Storage)               |  |
|  | - Tier 1: file_node, symbol_node                                      |  |
|  | - Tier 2: symbol_edge, symbol_semantics, strict_audit_contract        |  |
|  | - Tier 3: module_rollup, execution_flow, flow_step                    |  |
|  | - Indices: HNSW Cosine Vector Index + Simple FTS + Datalog Rules      |  |
|  +-----------------------------------------------------------------------+  |
+-----------------------------------------------------------------------------+

```

### 2.2 Workspace Layout & Crate Dependencies

```toml
[package]
name = "code-atlas-mcp"
version = "0.1.0"
edition = "2024"

[dependencies]
# MCP & Async Runtime
rmcp = { version = "0.1", features = ["server", "transport-io"] }
tokio = { version = "1.40", features = ["full"] }
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
schemars = "0.8"

# Static Analysis & Parsing
tree-sitter = "0.24"
streaming-iterator = "0.1"
tree-sitter-rust = "0.23"
tree-sitter-python = "0.23"
tree-sitter-typescript = "0.23"
tree-sitter-go = "0.23"
tree-sitter-c = "0.23"
tree-sitter-cpp = "0.23"
tree-sitter-java = "0.23"

# Graph Coordination & Storage
petgraph = "0.6"
cozo = { version = "0.7", features = ["storage-rocksdb", "graph-algo"] }

# Embeddings, Hashing & File Traversal
fastembed = "4.0"
blake3 = "1.5"
ignore = "0.4"          # Gitignore-aware parallel directory walker
anyhow = "1.0"
thiserror = "2.0"
tracing = "0.1"

```

```
code-atlas-mcp/
├── Cargo.toml
├── queries/                    # Embedded tree-sitter .scm query files per language
│   ├── rust/
│   │   ├── symbols.scm
│   │   ├── calls.scm
│   │   └── risks.scm
│   ├── python/ ...
│   └── typescript/ ...
└── src/
    ├── main.rs                 # stdio JSON-RPC entrypoint & server initialization
    ├── state.rs                # Shared AppState (Cozo DbInstance, Petgraph cache, FastEmbed)
    ├── db/
    │   ├── schema.rs           # CozoDB table, HNSW, and FTS creation scripts
    │   ├── queries.rs          # Parametrized Datalog query builders
    │   └── mutations.rs        # Atomic CozoDB transaction writers
    ├── ingestion/
    │   ├── walker.rs           # Parallel repo scanner using `ignore` crate
    │   ├── parser.rs           # Tree-sitter extraction & AST normalization
    │   ├── linker.rs           # Two-pass static & heuristic symbol linker
    │   └── cfg.rs              # On-demand sub-function Control Flow Graph generator
    ├── coordinator/
    │   ├── scc.rs              # Tarjan SCC condensation & bottom-up topological queue
    │   └── embeddings.rs       # Batched local CPU embedding worker
    ├── folding/
    │   ├── renderer.rs         # Distance 0/1/2+ progressive disclosure formatter
    │   └── pagination.rs       # Deterministic cursor encoding for high-fanout hubs
    └── mcp/
        ├── producer_tools.rs   # Cartographer tools (queue, write contracts, resolve edges)
        └── consumer_tools.rs   # Auditor tools (zoom, trace, predicate query, CFG zoom)

```

---

## 3. CozoDB Data Model & Datalog Schema

The persistent state lives in `<repo_root>/.code_atlas/atlas.cozo`.

### 3.1 Symbol ID Disambiguation Specification

To prevent collisions across overloaded methods, trait implementations, or nested scopes across languages, every `symbol_id` is constructed deterministically as:

```
{relative_file_path}::{scope_chain}::{symbol_name}#{kind}:{sig_hash_4byte}

```

* **Example (Rust trait impl):** `src/auth/jwt.rs::TokenVerifier_for_JwtService::verify#method:9f2a`
* **Example (Top-level Python function):** `app/billing/stripe.py::::charge_card#fn:3c10`
* `sig_hash_4byte` is the first 4 hex characters of the `blake3` hash of the normalized parameter types/arity, ensuring overloaded C++/Java/TypeScript signatures never collide.

### 3.2 Complete CozoDB Stored Relations Schema

```prolog
# =====================================================================
# TIER 1: CONCRETE SYNTAX & DETERMINISTIC SKELETON
# =====================================================================

:create file_node {
    file_path: String
    =>
    language: String,
    file_blake3: String,
    parent_module_path: String,
    line_count: Int,
    indexed_at_ms: Int
}

:create symbol_node {
    symbol_id: String
    =>
    qualified_name: String,
    name: String,
    kind: String,                   # "fn" | "method" | "struct" | "class" | "interface" | "enum" | "const" | "global"
    visibility: String,             # "pub" | "crate" | "private" | "export"
    file_path: String,
    start_byte: Int,
    end_byte: Int,
    start_line: Int,
    end_line: Int,
    signature: String,              # Declaration header without body
    doc_comment: String,            # Raw extracted docstring if present
    ast_blake3: String,             # BLAKE3 of whitespace/comment-stripped AST tokens
    cyclomatic_complexity: Int,
    risk_flags: [String],           # e.g., ["unsafe_usage", "raw_sql", "external_io", "unchecked_unwrap", "concurrency_primitive"]
    requires_strict_contract: Bool, # True if complexity >= 5 or len(risk_flags) > 0
    scc_group_id: Int,              # Tarjan Strongly Connected Component ID
    topological_level: Int,         # 0 = leaf node, N = max dependency depth
    mapping_status: String          # "UNMAPPED" | "LIGHTWEIGHT_MAPPED" | "STRICT_MAPPED" | "STALE"
}

# =====================================================================
# TIER 2: RELATIONAL HYPERGRAPH (STATIC + LLM-VERIFIED EDGES)
# =====================================================================

:create symbol_edge {
    from_symbol_id: String,
    to_symbol_id: String,
    edge_kind: String               # "CALLS" | "PASSES_TYPE" | "RETURNS_TYPE" | "MUTATES_STATE" | "READS_STATE" | "IMPLEMENTS"
    =>
    confidence: String,             # "STATIC_EXACT" | "STATIC_HEURISTIC" | "LSP_EXACT" | "LLM_VERIFIED" | "LLM_PRUNED"
    call_site_lines: [Int],
    call_expression_snippet: String,
    edge_semantics: String,         # Written by Cartographer: how/why from_symbol interacts with to_symbol
    data_taint_propagated: Bool     # True if untrusted/user-controlled data flows across this edge
}

# =====================================================================
# TIER 2.5: LAYERED NATURAL-LANGUAGE MENTAL MODEL (6C)
# =====================================================================

# Layer A: Required for every mapped symbol
:create symbol_semantics {
    symbol_id: String
    =>
    intent_summary: String,         # 1-2 sentence behavioral truth (not just happy-path restatement)
    mechanism_prose: String,        # Detailed explanation of internal logic & how callees are orchestrated
    domain_tags: [String],          # e.g., ["jwt", "crypto", "session-lifecycle"]
    annotated_ast_blake3: String,   # Matches symbol_node.ast_blake3 when fresh
    embedding: <F32; 384>           # Embedded from: signature + "\n" + intent_summary + "\n" + mechanism_prose
}

::hnsw create symbol_semantics:vec_idx {
    dim: 384,
    m: 16,
    ef_construction: 200,
    fields: [embedding],
    distance: Cosine
}

::fts create symbol_semantics:fts_idx {
    extractor: mechanism_prose,
    tokenizer: Simple
}

# Layer B: Strict Audit Contract (Auto-required for risky/complex nodes, or LLM-promoted)
:create strict_audit_contract {
    symbol_id: String
    =>
    invariants_assumed: [String],       # Implicit preconditions NOT checked inside this symbol
    invariants_enforced: [String],      # Explicit checks/validations performed inside this symbol
    state_mutations: [String],          # Side effects: DB writes, global state, filesystem, pointer/ref mutations
    error_surface: [String],            # Panics, exceptions, silent error drops, partial failure states
    concurrency_assumptions: [String],  # Lock ordering, thread-safety assumptions, async cancellation hazards
    trust_boundary: Bool,               # True if directly handling untrusted input or enforcing privilege checks
    taint_sources: [String],            # Untrusted parameters/inputs entering here
    taint_sinks: [String],              # Sensitive operations executed here (SQL, shell, deserialization, memory write)
    audit_smells: [String],             # Specific bug candidates or fragile assumptions spotted by Cartographer
    risk_score: Int                     # 1 to 10 severity/fragility score
}

# =====================================================================
# TIER 3: MACRO ARCHITECTURE — STRUCTURAL ROLLUPS & EXECUTION FLOWS (7C)
# =====================================================================

:create module_rollup {
    module_path: String
    =>
    parent_module_path: String,
    architectural_role: String,
    key_abstractions: [String],
    cross_module_contracts: [String],
    known_architectural_risks: [String],
    embedding: <F32; 384>
}

::hnsw create module_rollup:module_vec_idx {
    dim: 384,
    m: 16,
    ef_construction: 200,
    fields: [embedding],
    distance: Cosine
}

:create execution_flow {
    flow_id: String                     # e.g., "flow::checkout_payment_settlement"
    =>
    name: String,
    entrypoint_symbol_ids: [String],
    terminal_sink_symbol_ids: [String],
    narrative_summary: String,          # End-to-end data transformation & control narrative
    state_lifecycle_summary: String,    # How state mutates from start to finish
    security_boundaries_crossed: [String],
    embedding: <F32; 384>
}

::hnsw create execution_flow:flow_vec_idx {
    dim: 384,
    m: 16,
    ef_construction: 200,
    fields: [embedding],
    distance: Cosine
}

:create flow_step {
    flow_id: String,
    symbol_id: String
    =>
    step_order: Int,
    role_in_flow: String,
    critical_assumptions_at_step: String
}

```

---

## 4. Static Ingestion & Multi-Language Tree-sitter Engine

### 4.1 Unified Tree-sitter Extraction Pipeline

Rather than writing bespoke AST walkers per language, `code-atlas-mcp` uses declarative Tree-sitter S-expression queries (`.scm` files) compiled into the binary via `include_str!`. Each supported language defines three standard capture files:

1. **`symbols.scm`:** Captures `@symbol.def`, `@symbol.name`, `@symbol.params`, `@symbol.return`, `@symbol.body`, `@symbol.doc`, and `@symbol.visibility`.
2. **`calls.scm`:** Captures `@call.site`, `@call.receiver`, `@call.target_name`, `@type.reference`, and `@import.path`.
3. **`risks.scm`:** Captures language-specific hazard constructs:
* **Rust:** `(unsafe_block) @risk.unsafe`, `.unwrap()` / `.expect()` calls `@risk.unwrap`, `transmute` `@risk.memory`, `Command::new` `@risk.exec`.
* **Python:** `eval` / `exec` / `pickle.loads` / `subprocess` `@risk.exec`, bare `except:` `@risk.error_swallow`, raw string formatting inside `.execute(...)` `@risk.raw_sql`.
* **TypeScript/JS:** `dangerouslySetInnerHTML` / `innerHTML` `@risk.xss`, `eval` / `child_process` `@risk.exec`, `JSON.parse` without schema validation `@risk.deserialization`.
* **C/C++:** Raw pointer arithmetic, `memcpy`/`strcpy`/`sprintf` `@risk.memory`, manual `malloc`/`free` `@risk.alloc`.



### 4.2 AST Normalization & `blake3` Staleness Hashing

To prevent formatting changes or comment edits from invalidating the Cartographer's expensive LLM annotations (`8C`), `ast_blake3` is computed as follows:

1. Traverse the `tree-sitter::Node` subtree for `@symbol.def`.
2. Skip all nodes where `node.is_extra()` is true (comments and non-semantic whitespace).
3. Stream `(node.kind_id(), source_bytes[node.byte_range()])` for leaf tokens into `blake3::Hasher`.
4. Compare the resulting hash against `symbol_semantics.annotated_ast_blake3`. If they differ during re-sync, set `mapping_status = "STALE"`.

### 4.3 Cyclomatic Complexity & Strict Contract Auto-Promotion

During the Tree-sitter pass over `@symbol.body`, the parser counts branching AST node kinds (`if_expression`, `match_arm`, `for_statement`, `while_statement`, `catch_clause`, `binary_expression` with `&&` / `||` / `?`).

* $\text{Cyclomatic Complexity } M = 1 + \text{branch\_nodes}$
* A symbol automatically receives `requires_strict_contract = true` if any of the following hold:
1. $M \ge 5$
2. `risk_flags` is non-empty
3. The symbol is an external entrypoint (e.g., HTTP route handler, CLI command, public FFI export).



### 4.4 Two-Pass Static & Heuristic Edge Linker

Because `3A*` runs with zero build-environment configuration, cross-file symbol resolution operates in two passes:

1. **Pass 1 (Global Symbol Table Construction):** Collects all `symbol_node` definitions and file-level `@import.path` declarations into an in-memory symbol resolution index keyed by `(module_path, name)` and `name`.
2. **Pass 2 (Call & Type Resolution):**
* **Case 1 — Unambiguous Match (`confidence = "STATIC_EXACT"`):** The call site matches an explicit import in the same file, a local file symbol, or a globally unique symbol name across the repository.
* **Case 2 — Ambiguous Match (`confidence = "STATIC_HEURISTIC"`):** A method call like `service.validate(req)` matches $K$ candidate definitions (`1 < K <= 8`) across the codebase where the receiver type cannot be proven by Tree-sitter alone. The linker creates `STATIC_HEURISTIC` edges to those candidates (ranking candidates in imported modules first). If $K > 8$ (e.g., `.clone()` or `.to_string()`), heuristic edges are suppressed to prevent hub noise.
* **Pass 3 (Optional Second-Pass LSP Enrichment Hook):** If an LSP server or SCIP index is later invoked via `enrich_edges_via_lsp`, `STATIC_HEURISTIC` edges are deterministically upgraded to `LSP_EXACT` or deleted. Otherwise, the Cartographer LLM verifies or prunes them during mapping.



---

## 5. The Two-Pass Cartographer Orchestration Engine

To solve the tension between bottom-up structural accuracy (`5A`) and top-down behavioral execution flows (`5B`, `7C`), the MCP server orchestrates the Cartographer Agent using an in-memory `petgraph::Graph` coordinator.

### 5.1 Tarjan SCC Condensation & Bottom-Up Topological Queue

Real codebases contain recursive functions and mutually dependent cycles.

1. The server loads all `CALLS` edges (`confidence != "LLM_PRUNED"`) into a directed `petgraph::DiGraph`.
2. It runs `petgraph::algo::tarjan_scc(&graph)` to partition all symbols into **Strongly Connected Components (SCCs)**.
* Any single non-recursive function forms an SCC of size 1.
* Mutually recursive functions ($A \leftrightarrow B$) form an SCC super-node of size $\ge 2$.


3. The condensed DAG of SCCs is topologically sorted from **leaves (`topological_level = 0`, out-degree 0)** to **roots (`topological_level = max`, in-degree 0)**.
4. When the Cartographer Agent calls `get_next_cartography_batch`, the server pops the lowest-level `UNMAPPED` or `STALE` SCCs whose downstream callees are *already mapped*.
5. **Crucial Context Injection:** For every symbol in the batch, the MCP server returns:
* The symbol's full source code and risk flags.
* The *already-completed* `intent_summary`, `invariants_assumed`, and `error_surface` of every function it calls!
* Any `STATIC_HEURISTIC` edges leaving the symbol so the Cartographer can mark each candidate `LLM_VERIFIED` or `LLM_PRUNED`.



### 5.2 Top-Down Behavioral Flow Tracing

Once a subsystem's symbols are mapped (or during targeted `5B` exploration), the Cartographer queries entrypoints (nodes with `in_degree == 0` or route/CLI decorators) and traces execution paths downward using the already-written node contracts to author `execution_flow` and `flow_step` records without having to re-read every raw file from scratch.

---

## 6. On-Demand Sub-Function Zoom (Adaptive CFG Engine)

In accordance with `4C` (Adaptive Granularity), the global CozoDB database remains strictly at the **Symbol Level** so the graph stays compact and fast. However, when the Auditor Agent inspects a complex function (e.g., a 180-line parser or state machine) and needs sub-function statement-level clarity, it calls `expand_function_cfg(symbol_id)`.

### 6.1 Ephemeral Control Flow Graph (CFG) Extraction

On demand, the Rust server re-parses the target symbol's byte range with `tree-sitter` and constructs a sub-function **Control Flow & Def-Use Graph**:

* **Basic Blocks (`CFGBlock`):** Consecutive non-branching statements grouped by line range (`L42-L48`).
* **Control Edges (`CFGEdge`):**
* `TRUE_BRANCH(condition_expr)` / `FALSE_BRANCH(condition_expr)`
* `MATCH_ARM(pattern)`
* `LOOP_BACKEDGE` / `LOOP_EXIT`
* `EARLY_RETURN` / `PANIC_OR_THROW` / `UNHANDLED_ERROR_PROPAGATION (?)`


* **Intra-Function State Tracking:** Lists local variables mutated within each basic block and highlights blocks where external calls or `risk_flags` occur.

---

## 7. MCP Tool Surface & JSON Schemas

The MCP server exposes **11 tools** divided into **Producer Tools** (for the Cartographer Agent) and **Consumer Tools** (for the Auditor/Bug-Hunter Agent).

### 7.1 Producer Tools (Cartographer Workflow)

| Tool Name | Purpose |
| --- | --- |
| `sync_repository_skeleton` | Runs parallel `tree-sitter` ingestion, computes BLAKE3 AST hashes, builds heuristic edges, and updates Tarjan SCC topological queues. |
| `get_next_cartography_batch` | Returns the next $N$ leaf-to-root symbols (or a targeted module/subgraph) ready for LLM mapping, bundled with their callees' existing mental models. |
| `submit_symbol_models` | Writes `symbol_semantics`, optional `strict_audit_contract`, edge verifications/prunings (`LLM_VERIFIED` / `LLM_PRUNED`), and edge taint flags in one atomic transaction + generates local embeddings. |
| `submit_module_rollup` | Writes a bottom-up `module_rollup` architectural summary for a file or directory once its constituent symbols are mapped. |
| `record_execution_flow` | Registers a top-down `execution_flow` pipeline and its ordered `flow_step` links across the graph. |

#### Detailed Schema: `get_next_cartography_batch`

```json
{
  "name": "get_next_cartography_batch",
  "description": "Fetches the next batch of unmapped or stale symbols in bottom-up topological order (or scoped to a path/entrypoint), including the already-mapped contracts of their downstream callees.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "batch_size": { "type": "integer", "default": 5, "maximum": 15 },
      "scope_path_prefix": { "type": "string", "description": "Optional directory or file prefix to restrict mapping scope (Mode 5B)." },
      "root_entrypoint_id": { "type": "string", "description": "Optional symbol_id; restricts queue to the transitive call cone beneath this symbol." },
      "prioritize_high_risk": { "type": "boolean", "default": false, "description": "If true, prioritizes unmapped symbols with risk_flags or high complexity." }
    }
  }
}

```

#### Detailed Schema: `submit_symbol_models`

```json
{
  "name": "submit_symbol_models",
  "description": "Persists the natural-language mental model, strict audit contract, and resolved edge relationships for a batch of symbols.",
  "inputSchema": {
    "type": "object",
    "required": ["models"],
    "properties": {
      "models": {
        "type": "array",
        "items": {
          "type": "object",
          "required": ["symbol_id", "intent_summary", "mechanism_prose", "domain_tags", "resolved_edges"],
          "properties": {
            "symbol_id": { "type": "string" },
            "intent_summary": { "type": "string", "description": "1-2 sentence behavioral truth of what this code actually does." },
            "mechanism_prose": { "type": "string", "description": "Detailed explanation of internal mechanics, control flow, and how it interacts with neighbors." },
            "domain_tags": { "type": "array", "items": { "type": "string" } },
            "promote_to_strict_contract": { "type": "boolean", "default": false },
            "strict_contract": {
              "type": "object",
              "description": "Required if symbol has requires_strict_contract=true or promote_to_strict_contract=true.",
              "required": ["invariants_assumed", "invariants_enforced", "state_mutations", "error_surface", "trust_boundary", "audit_smells", "risk_score"],
              "properties": {
                "invariants_assumed": { "type": "array", "items": { "type": "string" }, "description": "Unchecked assumptions made about inputs, state, or caller behavior." },
                "invariants_enforced": { "type": "array", "items": { "type": "string" }, "description": "Explicit validations/guards enforced by this code." },
                "state_mutations": { "type": "array", "items": { "type": "string" } },
                "error_surface": { "type": "array", "items": { "type": "string" } },
                "concurrency_assumptions": { "type": "array", "items": { "type": "string" } },
                "trust_boundary": { "type": "boolean" },
                "taint_sources": { "type": "array", "items": { "type": "string" } },
                "taint_sinks": { "type": "array", "items": { "type": "string" } },
                "audit_smells": { "type": "array", "items": { "type": "string" }, "description": "Concrete bug hypotheses, edge-case blindspots, or invariant mismatches." },
                "risk_score": { "type": "integer", "minimum": 1, "maximum": 10 }
              }
            },
            "resolved_edges": {
              "type": "array",
              "items": {
                "type": "object",
                "required": ["to_symbol_id", "edge_kind", "resolution", "edge_semantics"],
                "properties": {
                  "to_symbol_id": { "type": "string" },
                  "edge_kind": { "type": "string", "enum": ["CALLS", "PASSES_TYPE", "RETURNS_TYPE", "MUTATES_STATE", "READS_STATE", "IMPLEMENTS"] },
                  "resolution": { "type": "string", "enum": ["LLM_VERIFIED", "LLM_PRUNED"] },
                  "edge_semantics": { "type": "string", "description": "Why this connection exists and what data/state crosses it." },
                  "data_taint_propagated": { "type": "boolean", "default": false }
                }
              }
            }
          }
        }
      }
    }
  }
}

```

---

### 7.2 Consumer Tools (Auditor & Exploration Workflow)

| Tool Name | Purpose |
| --- | --- |
| `get_macro_atlas` | Returns the Tier 3 big-picture view: top-level `module_rollup` summaries, registered `execution_flow` pipelines, entrypoints, and mapping coverage statistics. |
| `inspect_symbol_zoom` | **Primary Navigation Tool.** Returns the Distance 0/1/2+ progressive disclosure view around one or more target symbols, connecting local code to immediate callers/callees and macro flows. |
| `expand_function_cfg` | Generates the sub-function basic-block Control Flow Graph (CFG) and branch condition map for a single complex symbol (`4C`). |
| `trace_connective_paths` | Finds the shortest or highest-risk call/taint paths between `source_symbol_id` and `target_symbol_id` (or between any `trust_boundary` and any `state_mutation`), rendering the semantic chain. |
| `query_audit_surface` | Structured predicate & Datalog query tool for bug hunting—filters symbols by `risk_score`, `trust_boundary`, `risk_flags`, `audit_smells`, or **caller-callee invariant mismatches**. |
| `semantic_atlas_search` | Hybrid HNSW vector + Full-Text search across symbols, module rollups, and execution flows for conceptual queries (e.g., *"where are JWT expiration timestamps validated?"*). |

#### Detailed Schema: `inspect_symbol_zoom`

```json
{
  "name": "inspect_symbol_zoom",
  "description": "Renders a context-folded mental model around target symbol(s): Distance 0 (full source + strict contract), Distance 1 (folded callers/callees + edge semantics), and Distance 2+ (macro module & execution flow ties).",
  "inputSchema": {
    "type": "object",
    "required": ["symbol_ids"],
    "properties": {
      "symbol_ids": { "type": "array", "items": { "type": "string" }, "maxItems": 5 },
      "include_raw_source": { "type": "boolean", "default": true },
      "max_neighbors_per_direction": { "type": "integer", "default": 12, "maximum": 50 },
      "neighbor_cursor": { "type": "string", "description": "Pagination cursor for high-fanout hub symbols." }
    }
  }
}

```

#### Detailed Schema: `query_audit_surface`

```json
{
  "name": "query_audit_surface",
  "description": "Queries the mental model for high-risk bug surfaces, taint paths, unverified edges, or caller-to-callee precondition violations.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "query_mode": {
        "type": "string",
        "enum": [
          "HIGH_RISK_CONTRACTS",
          "TRUST_BOUNDARY_TO_SINK_PATHS",
          "UNCHECKED_ASSUMPTION_HOTSPOTS",
          "AMBIGUOUS_UNVERIFIED_EDGES"
        ]
      },
      "min_risk_score": { "type": "integer", "default": 6 },
      "required_risk_flags": { "type": "array", "items": { "type": "string" } },
      "max_hop_depth": { "type": "integer", "default": 4, "maximum": 8 },
      "scope_path_prefix": { "type": "string" },
      "limit": { "type": "integer", "default": 20 }
    }
  }
}

```

---

## 8. Context-Folding Protocol & Output Wire Format

To maximize LLM comprehension per token (`10C`), tool outputs are formatted in a dense, structured Markdown wire format rather than deeply nested JSON. Below is the exact specification for the output of `inspect_symbol_zoom`:

```markdown
# SYMBOL ZOOM: `src/auth/jwt.rs::JwtService::verify_token#method:9f2a`
**Status:** `STRICT_MAPPED` (AST Fresh ✓) | **Complexity:** `8` | **Risk Score:** `8/10`
**Risk Flags:** `["crypto_op", "unchecked_unwrap"]` | **Trust Boundary:** `TRUE`

## [TIER 3: THE LARGER PICTURE]
* **Module (`src/auth`):** Handles stateless session authentication and HMAC/RSA token verification for all HTTP middleware routes.
* **Participates in Execution Flows:**
  1. `flow::api_request_auth_guard` (Step 2 of 4) — *Validates Bearer header token and extracts claims before route dispatch.*
  2. `flow::websocket_upgrade_handshake` (Step 3 of 5) — *Authenticates ephemeral query-param ticket during WS upgrade.*

## [DISTANCE 0: SEMANTIC CONTRACT & AUDIT MODEL]
* **Intent:** Parses a raw compact JWS string, verifies its cryptographic signature against the active keyset, and returns deserialized `SessionClaims`.
* **Mechanism:** Splits the input token on `.` into 3 segments, decodes the base64 header to select a `kid` from `KeyRing`, verifies the signature, and deserializes the payload JSON.
* **Invariants Assumed (Preconditions NOT checked here):**
  * `!` Assumes caller has already stripped the `"Bearer "` prefix (will fail parsing if prefix remains).
  * `!` Assumes `KeyRing` in shared state is already populated and non-empty.
* **Invariants Enforced:**
  * Verifies HMAC-SHA256 signature equality in constant time.
  * Checks `claims.exp > current_unix_time`.
* **State Mutations:** None (pure read of `self.key_ring`).
* **Error Surface:**
  * **PANIC HAZARD (Line 84):** Calls `.unwrap()` on `SystemTime::now().duration_since(UNIX_EPOCH)`.
  * Returns `AuthError::InvalidSignature` or `AuthError::Expired`.
* **Audit Smells:**
  * `[HIGH]` **Algorithm Confusion / `alg: none` Header:** Header `alg` field is parsed from untrusted user JSON on line 67 and used to select the verifier branch without whitelisting allowed algorithms against the key type.
  * `[MED]` Does not validate `nbf` (Not Before) or `aud` (Audience) claims.

## [DISTANCE 0: SOURCE CODE (`src/auth/jwt.rs:58-96`)]
```rust
pub fn verify_token(&self, raw_token: &str) -> Result<SessionClaims, AuthError> {
    // ... exact source lines 58-96 ...
}

```

## [DISTANCE 1: UPSTREAM CALLERS (Who calls this? — Showing 2 of 2)]

1. **`src/http/middleware.rs::auth_middleware#fn:11c4`** `[LLM_VERIFIED]` (Lines 41)
* *Caller Intent:* Intercepts incoming Axum HTTP requests and attaches `SessionClaims` to request extensions.
* *Edge Semantics:* Extracts `Authorization` header, strips `"Bearer "` via `strip_prefix`, and passes slice to `verify_token`. **[Taint Propagated: TRUE]**


2. **`src/ws/handler.rs::ws_upgrade#fn:88e1`** `[LLM_VERIFIED]` (Lines 109)
* *Caller Intent:* Upgrades HTTP connection to WebSocket using `?ticket=` query parameter.
* *Edge Semantics:* Passes raw `params.ticket` directly into `verify_token` without pre-validation. **[Taint Propagated: TRUE]**



## [DISTANCE 1: DOWNSTREAM CALLEES (What does this call? — Showing 3 of 3)]

1. **`src/auth/keyring.rs::KeyRing::get_key#method:44a0`** `[STATIC_EXACT]` (Line 72)
* *Callee Intent:* Looks up a verification key by `kid` string in an in-memory `HashMap`.
* *Edge Semantics:* Passes untrusted `header.kid` from JWT header to fetch verification key.
* *Callee Assumptions:* Assumes `kid` is under 64 bytes (no allocation cap).


2. **`src/crypto/hmac.rs::verify_hmac_sha256#fn:09b2`** `[LLM_VERIFIED]` (Line 79)
* *Callee Intent:* Constant-time HMAC-SHA256 verification via `ring`.
* *Edge Semantics:* Passes signing input bytes and decoded signature bytes.



```

### 8.1 High-Fanout Hub Pagination Mechanics
If a symbol has more than `max_neighbors_per_direction` callers or callees (e.g., an error logging utility called in 140 places), the Folding Engine:
1. Sorts neighbors deterministically by `(data_taint_propagated DESC, neighbor_risk_score DESC, symbol_id ASC)` so the highest-risk connections always appear on Page 1.
2. Appends a compact cursor block:
   `[+128 additional callers omitted. Call inspect_symbol_zoom with neighbor_cursor="caller:offset:12" to view next page.]`

---

## 9. Reference CozoDB Datalog Queries for Bug Hunting

These parametrized Datalog scripts live in `src/db/queries.rs` and power `query_audit_surface` and `trace_connective_paths`.

### 9.1 Query A: Trust Boundary to Taint Sink Reachability
Finds all multi-hop execution paths (up to `$max_hops`) where untrusted input enters a `trust_boundary` node and flows along `data_taint_propagated == true` edges into a node with non-empty `taint_sinks` or `state_mutations`.

```prolog
taint_path[start_id, curr_id, hops, path_nodes] :=
    *strict_audit_contract{symbol_id: start_id, trust_boundary: true},
    *symbol_edge{
        from_symbol_id: start_id,
        to_symbol_id: curr_id,
        confidence,
        data_taint_propagated: true
    },
    confidence != "LLM_PRUNED",
    hops = 1,
    path_nodes = [start_id, curr_id]

taint_path[start_id, next_id, next_hops, next_path] :=
    taint_path[start_id, curr_id, hops, path_nodes],
    hops < $max_hops,
    *symbol_edge{
        from_symbol_id: curr_id,
        to_symbol_id: next_id,
        confidence,
        data_taint_propagated: true
    },
    confidence != "LLM_PRUNED",
    not is_in(next_id, path_nodes),
    next_hops = hops + 1,
    next_path = append(path_nodes, next_id)

?[start_id, sink_id, hops, path_nodes, sink_mutations, sink_smells] :=
    taint_path[start_id, sink_id, hops, path_nodes],
    *strict_audit_contract{
        symbol_id: sink_id,
        state_mutations: sink_mutations,
        taint_sinks: sinks,
        audit_smells: sink_smells
    },
    (length(sinks) > 0 || length(sink_mutations) > 0)

:order hops, start_id
:limit $limit

```

### 9.2 Query B: Unchecked Precondition Hotspots (Caller/Callee Boundary Audit)

A classic source of subtle bugs is when Callee $B$ assumes a precondition (`invariants_assumed` is non-empty, e.g., *"Assumes slice is non-empty and sorted"*), and Caller $A$ calls $B$ across a taint-propagating edge without enforcing invariants or while exhibiting audit smells.

```prolog
?[caller_id, callee_id, edge_semantics, callee_assumptions, caller_enforced, callee_risk] :=
    *symbol_edge{
        from_symbol_id: caller_id,
        to_symbol_id: callee_id,
        confidence,
        edge_semantics
    },
    confidence != "LLM_PRUNED",
    *strict_audit_contract{
        symbol_id: callee_id,
        invariants_assumed: callee_assumptions,
        risk_score: callee_risk
    },
    length(callee_assumptions) > 0,
    callee_risk >= $min_risk_score,
    *strict_audit_contract{
        symbol_id: caller_id,
        invariants_enforced: caller_enforced
    }

:order -callee_risk
:limit $limit

```

---

## 10. Architectural Critique, Failure Modes & Hardening Guardrails

To ensure this design survives contact with messy, real-world production repositories, four failure modes must be explicitly guarded against in the implementation:

### 10.1 Cascading Hallucination in Bottom-Up Rollups

* **The Vulnerability:** In a bottom-up topological pass (`5A`), if the Cartographer Agent misinterprets a leaf function $F_0$, and $F_1$ relies only on $F_0$'s summary, the error compounds up the call graph.
* **The Engineering Guardrail:**
1. When `get_next_cartography_batch` presents a caller $F_1$, it includes both the callee $F_0$'s natural-language contract **and** $F_0$'s exact signature + return type + `invariants_assumed`.
2. The server validates every `submit_symbol_models` payload: if a symbol has `requires_strict_contract == true` and the agent omits `strict_contract` or leaves `invariants_assumed` and `error_surface` both empty despite the presence of static `risk_flags`, the MCP tool rejects the submission with a structured validation error prompting the agent to account for the detected `risk_flags`.



### 10.2 Dynamic Dispatch, Closures & Callback Inversion

* **The Vulnerability:** Static Tree-sitter (`3A*`) sees `router.route("/login", post(login_handler))` or `vec.iter().map(|x| transform(x))`, where control flow is passed as a higher-order function reference rather than a direct call expression.
* **The Engineering Guardrail:** The `calls.scm` queries for all supported languages must capture **Identifier References Passed as Arguments** (`@call.callback_ref`) alongside direct call expressions (`@call.site`). Whenever an identifier matching a known function name in scope is passed as an argument to another function, the linker emits a `CALLS` edge (`confidence = "STATIC_EXACT"` or `"STATIC_HEURISTIC"`) so route handlers, middleware, and async callbacks are never orphaned as disconnected subgraphs.

### 10.3 Macro & Code-Generation Blindness (Rust/C/C++)

* **The Vulnerability:** In Rust (`tokio::select!`, `sqlx::query!`, `derive` macros) or C preprocessor macros, crucial calls and control branches exist inside macro token streams (`token_tree`) that standard Tree-sitter grammar rules treat as opaque tokens.
* **The Engineering Guardrail:** The Rust and C/C++ parser module (`src/ingestion/parser.rs`) includes a secondary token-stream scanner over `macro_invocation` nodes: any identifier inside a macro's `token_tree` that matches a known symbol in the file or imports is linked via a `STATIC_HEURISTIC` edge, and macro invocations are automatically added to `risk_flags` (`"macro_invocation"`) if they wrap external I/O or unsafe blocks.

### 10.4 CozoDB Write Contention vs. FastEmbed Latency

* **The Vulnerability:** Computing local CPU embeddings for 10 symbols takes ~40–80ms. Holding a CozoDB write transaction open while computing ONNX embeddings blocks concurrent read queries.
* **The Engineering Guardrail:** In `submit_symbol_models`, the server always runs `fastembed` embedding generation **outside and prior to** opening the CozoDB write transaction, keeping database lock hold times under 2 milliseconds.

---

## 11. Step-by-Step Implementation Roadmap

### Milestone 1: Storage & Static Skeleton (`Days 1–3`)

1. Initialize the Cargo project and implement `src/db/schema.rs` to bootstrap `.code_atlas/atlas.cozo` with all 8 stored relations, HNSW indices, and FTS indices.
2. Implement `src/ingestion/walker.rs` and `src/ingestion/parser.rs` with embedded `.scm` queries for your primary target languages (starting with Rust, Python, and TypeScript, then adding Go/C/C++/Java).
3. Implement AST token normalization (`blake3`), cyclomatic complexity counting, and static `risk_flags` extraction.
4. Implement the Two-Pass Linker (`src/ingestion/linker.rs`) including callback-reference capture and heuristic candidate generation.

### Milestone 2: Graph Coordinator & Producer MCP Tools (`Days 4–5`)

1. Build `src/coordinator/scc.rs` using `petgraph::algo::tarjan_scc` to compute SCC super-nodes and assign `topological_level` values.
2. Integrate `fastembed` (`TextEmbedding::try_new` with `EmbeddingModel::BGESmallENV15`) in `src/coordinator/embeddings.rs`.
3. Expose Producer MCP tools (`sync_repository_skeleton`, `get_next_cartography_batch`, `submit_symbol_models`, `submit_module_rollup`, `record_execution_flow`) via `rmcp`.

### Milestone 3: Context-Folding, CFG Zoom & Consumer MCP Tools (`Days 6–7`)

1. Implement `src/folding/renderer.rs` to format Distance 0/1/2+ Markdown views and deterministic cursor pagination (`src/folding/pagination.rs`).
2. Implement `src/ingestion/cfg.rs` to lazily construct basic-block Control Flow Graphs for `expand_function_cfg`.
3. Implement Datalog query builders in `src/db/queries.rs` and expose Consumer MCP tools (`get_macro_atlas`, `inspect_symbol_zoom`, `expand_function_cfg`, `trace_connective_paths`, `query_audit_surface`, `semantic_atlas_search`).

### Milestone 4: Cartographer & Auditor System Prompts (`Day 8`)

1. Write a dedicated system prompt / MCP prompt template (`cartographer_prompt`) instructing the Producer Agent how to iterate over `get_next_cartography_batch`, verify/prune `STATIC_HEURISTIC` edges, extract adversarial `invariants_assumed`, and stitch top-down `execution_flow` slices.
2. Write the companion Auditor prompt template (`auditor_prompt`) instructing the Consumer Agent how to start with `get_macro_atlas` and `query_audit_surface`, trace taint paths, and zoom into suspect symbols using `inspect_symbol_zoom` and `expand_function_cfg`.
