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
* Calls, type references and `IMPLEMENTS` edges link only to definitions in the caller's own
  language (JavaScript and TypeScript count as one); there is no cross-language linking.
* Not implemented: the optional LSP/SCIP enrichment pass (`enrich_edges_via_lsp`), C/C++/Java
  frontends, and `MUTATES_STATE`/`READS_STATE` inference (the Cartographer can add those edges).

### Go notes
* Package-level `func`, methods (scoped by receiver type), `type`, `const` and `var` are symbols;
  locals and closures are not. Visibility is `pub` iff the name is exported.
* Interfaces are structural, so no `IMPLEMENTS` edges are emitted for Go yet.
* `vendor/` directories are skipped for Go (and Ruby) files.
* Import paths resolve through the nearest `go.mod` to exact repo packages; stdlib and third-party
  imports link to nothing. Without a `go.mod`, paths fall back to directory-suffix matching.
  Unqualified calls resolve only within the caller's package (any file of its directory) or dot-imports.
* Method calls are resolved by type where the caller's code shows it: receivers, parameters, locals
  (`:=`, `var`, `range`, type switches, composite literals, `new`/`make`), struct fields (including
  embedded ones), function and method results, and package variables. A struct's method is an exact
  edge; an interface call is a heuristic edge to every repo type with the interface's full method
  set (class-hierarchy analysis, also for common stdlib interfaces such as `io.Writer` and `error`);
  a call on a stdlib/third-party concrete type, a field, or an anonymous struct links to nothing.
  Receivers of unknown type fall back to name matching.
* Builds are resolved as linux/amd64 with cgo: among platform variants of one name (`_windows.go`,
  `//go:build`), the default build's wins. Production code never links into `_test.go` files.
* Only the first name of `const A, B = 1, 2` becomes a symbol. `goto` and labels are treated as simple
  statements in CFGs.

### Ruby notes
* Classes and modules are both kind `class` (disambiguated by scope); `class Foo::Bar` is the symbol
  `Bar` under scope `Foo`. Constants at top level or in a class body are `const`; locals and `@ivars`
  are not symbols. `def self.x` and an instance `x` get distinct IDs (a `self.` marker in the hash).
* Visibility follows bare `private`/`protected`/`public`, `private :a`, `private def x` and
  `private_class_method`; `protected` is reported as `private`.
* `class A < B` and `include`/`extend`/`prepend M` become `IMPLEMENTS` edges. `require`/`require_relative`/
  `load`/`autoload` with literal paths are imports.
* Calls resolve through a class model (classes by qualified name, ancestors from `<`/`include`/
  `extend`/`prepend`, Ruby constant lookup). Self calls go to the class and its ancestors, then the
  classes that mix a module in, then top-level methods, never an unrelated class. `Foo.x` and
  `Foo.new` (-> `initialize`) resolve in `Foo` (class methods preferred), or to nothing when `Foo` is
  not a repo class. Receivers are typed from `Foo.new`, literals (core types), `self`/`self.class`,
  instance variables and collection elements assigned in the class, method results (last
  expression, `attr_reader`s), and positional parameters from the arguments of every call that may
  reach the method (closed-world; a method no call reaches has untyped parameters). A value typed as
  a module or base class resolves through the classes below it. `def self.x`, `def Const.x` and a
  `def` inside `class << self` are class methods.
* Structural typing for values of unknown type: the candidate classes must answer every method
  called on the same value (a local per binding, an `@ivar` class-wide, a method result, plus what
  the one exact callee calls on a parameter it is passed). `respond_to?` and `is_a?` checks switch
  this off. Only Object's methods are universal; a class below a built-in (`< Array`, `include
  Enumerable`, a reopened `class String`, `< Struct.new(..)`) answers the core API, and one below an
  external ancestor or with `method_missing` answers anything.
* Builds are resolved as CRuby 3.3 on x86_64-linux: a `def` in a branch that build never takes
  (`if RUBY_ENGINE == "jruby"`, `RUBY_VERSION < "2.7"`, `RUBY_PLATFORM =~ /mswin/`,
  `Gem.win_platform?`) is never a call target.
* Methods without a `def` (`attr_*`, `Struct.new` members, `alias`, `alias_method`, delegators) are
  part of the method table: they shadow ancestors and count as candidates. On a receiver of unknown
  type, a name that more than 8 classes respond to is too generic to link, and so is a core method
  name (`each`, `fetch`, `match`, ...) on a value nothing else shows to be a repo object; calls must
  also match the candidate's arity.
* Method names passed as symbols (`before_action :x`, `send(:x)`, `&:x`) are `SymbolRef` edges.
* Not modelled: paren-less bare self-calls as call sites (`helper` alone is indistinguishable from a
  local variable), `define_method` with dynamic names, `method_missing`, `Module#prepend` ordering, and
  block-style routes (`get '/x' do`, `task :x do`), whose bodies belong to the enclosing symbol.
  Block variables and parameters without typed callers stay untyped (heuristic edges).

## Development

```sh
cargo test                     # unit + integration tests (in-memory Cozo, offline embedder)
cargo test -- --ignored        # also exercises the real embedding model (needs network)
```

Each language lives in `src/ingestion/lang/<lang>.rs` plus `queries/<lang>/{symbols,calls,risks}.scm`;
adding one means implementing `Frontend` (and optionally `CfgLang` in `src/ingestion/cfg.rs`).
