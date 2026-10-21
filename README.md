# code-atlas-mcp

An MCP server that lets an LLM build, persist and query a layered structural and semantic model of a
codebase, for bug hunting, security auditing and architecture questions. Design: see
[`code-atlas-mcp-spec.md`](code-atlas-mcp-spec.md).

* **Phase 0, skeleton (automatic):** tree-sitter parsing of Rust, Python, TypeScript/JavaScript, Go and Ruby
  extracts symbols, signatures, complexity, risk flags and a static call graph in seconds.
* **Phase 1, Cartographer (an agent):** works through a bottom-up queue, writes intent, mechanism and
  adversarial *assumed invariants* per symbol, resolves ambiguous edges, then records module rollups
  and end-to-end execution flows.
* **Phase 2, Auditor (an agent):** queries the persisted model with structured audit queries, taint
  paths, hybrid semantic search and progressive zoom.

State lives in `<repo>/.code_atlas/atlas.cozo` (embedded CozoDB on RocksDB). Add `.code_atlas/` to
your `.gitignore`.

## Build and run

Needs Rust (edition 2024) and `clang`/`libclang` (RocksDB build).

```sh
cargo build --release
./target/release/code-atlas-mcp --repo /path/to/repo   # speaks MCP over stdio
```

Register it with an MCP client, for example Claude Code (`.mcp.json`):

```json
{
  "mcpServers": {
    "code-atlas": {
      "command": "/path/to/code-atlas-mcp/target/release/code-atlas-mcp",
      "args": ["--repo", "."]
    }
  }
}
```

### Embeddings

Symbol, module and flow summaries are embedded locally with `BAAI/bge-small-en-v1.5` (ONNX, CPU).
The model downloads on first use to `$CODE_ATLAS_MODEL_CACHE`, else `$XDG_CACHE_HOME/code-atlas-mcp/models`.
Without network access set `CODE_ATLAS_EMBEDDER=hash` for a deterministic bag-of-words fallback
(much weaker semantic search, everything else unchanged).

Logging goes to stderr; set `RUST_LOG=debug` for more.

## Tools (11)

Producer (Cartographer):

| Tool | Purpose |
| --- | --- |
| `sync_repository_skeleton` | Parse, hash, link, mark changed symbols `STALE`, rebuild the SCC/topological queue. Safe to re-run; LLM models of unchanged code are kept. |
| `get_next_cartography_batch` | Next symbols bottom-up with source and the already-written contracts of their callees. |
| `submit_symbol_models` | Atomically store semantics, strict contracts and edge verdicts (`LLM_VERIFIED` / `LLM_PRUNED`); rejects hollow contracts for risky code. |
| `submit_module_rollup` | Architectural summary of a directory or file. |
| `record_execution_flow` | Ordered top-down pipeline across symbols. |

Consumer (Auditor):

| Tool | Purpose |
| --- | --- |
| `get_macro_atlas` | Rollups, flows, entrypoints and mapping coverage. |
| `query_audit_surface` | `HIGH_RISK_CONTRACTS`, `TRUST_BOUNDARY_TO_SINK_PATHS`, `UNCHECKED_ASSUMPTION_HOTSPOTS`, `AMBIGUOUS_UNVERIFIED_EDGES`. |
| `trace_connective_paths` | Shortest, highest-risk or taint-only paths between symbols. |
| `semantic_atlas_search` | Hybrid vector + full-text search with rank fusion. |
| `inspect_symbol_zoom` | Distance 0/1/2+ view with source, contract, callers and callees (paginated for hubs). |
| `expand_function_cfg` | Sub-function control-flow graph (basic blocks, branches, loops, `?`/panic exits). |

Prompts: `cartographer_prompt` and `auditor_prompt` are operating manuals for the two agent roles.

Typical session: run the Cartographer prompt (`sync_repository_skeleton`, then loop batch/submit,
rollups, flows), then the Auditor prompt.

## Notes and deviations from the spec

* **Dependency versions** differ from the spec's guesses (`rmcp` 3.6, `fastembed` 7, `petgraph` 0.8,
  `tree-sitter` 0.27); see `Cargo.toml`.
* **Taint paths** (spec §9.1) run as bounded graph traversals in Rust, not as recursive Datalog:
  path-accumulating recursion cannot be capped inside Cozo and blows up on dense graphs. One shortest
  path per (trust boundary, sink) pair, with result and work caps and a `truncated` marker. Query B
  (§9.2) is Datalog, re-ranked in Rust.
* `symbol_node` has an extra `is_entrypoint` column; `atlas_meta` stores the schema version.
* `ast_blake3` hashes token *kind names*, not numeric kind ids, so hashes survive grammar upgrades.
* Entrypoints are detected from definitions (`main`, route/CLI decorators and attributes, FFI exports)
  and from handlers passed to route-registering calls.
* Not implemented: the optional LSP/SCIP enrichment pass (`enrich_edges_via_lsp`), C/C++/Java
  frontends, and `MUTATES_STATE`/`READS_STATE` inference (the Cartographer can add those edges).

### Go notes
* Package-level `func`, methods (scoped by receiver type), `type`, `const` and `var` are symbols;
  locals and closures are not. Visibility is `pub` iff the name is exported.
* Interfaces are structural, so no `IMPLEMENTS` edges are emitted for Go yet.
* `vendor/` directories are skipped for Go (and Ruby) files.
* Import paths are matched to repo packages by directory suffix (module path from `go.mod` is not read),
  so a same-named package from another module can produce a heuristic edge.
* Only the first name of `const A, B = 1, 2` becomes a symbol. `goto` and labels are treated as simple
  statements in CFGs.

### Ruby notes
* Classes and modules are both kind `class` (disambiguated by scope); `class Foo::Bar` is the symbol
  `Bar` under scope `Foo`. Constants at top level or in a class body are `const`; locals and `@ivars`
  are not symbols. `def self.x` and an instance `x` get distinct IDs through the line marker.
* Visibility follows bare `private`/`protected`/`public`, `private :a`, `private def x` and
  `private_class_method`; `protected` is reported as `private`.
* `class A < B` and `include`/`extend`/`prepend M` become `IMPLEMENTS` edges. `require`/`require_relative`/
  `load`/`autoload` with literal paths are imports.
* Receiver-less calls inside a class resolve to that class's methods first. Method names passed as
  symbols (`before_action :x`, `send(:x)`, `&:x`) are `SymbolRef` edges.
* Not modelled: paren-less bare self-calls (`helper` is indistinguishable from a local variable),
  `define_method`, `method_missing`, `Struct.new` classes, `Module#prepend` ordering, and block-style
  routes (`get '/x' do`, `task :x do`), whose bodies belong to the enclosing symbol. Expect many
  heuristic edges (duck typing).

## Development

```sh
cargo test                     # unit + integration tests (in-memory Cozo, offline embedder)
cargo test -- --ignored        # also exercises the real embedding model (needs network)
```

Each language lives in `src/ingestion/lang/<lang>.rs` plus `queries/<lang>/{symbols,calls,risks}.scm`;
adding one means implementing `Frontend` (and optionally `CfgLang` in `src/ingestion/cfg.rs`).
