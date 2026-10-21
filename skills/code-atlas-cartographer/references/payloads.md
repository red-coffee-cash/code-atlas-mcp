# code-atlas payload reference

Exact argument shapes for the producer tools, the rules the server validates, and complete payloads that the server accepted. They come from mapping a small Flask notes app; `worked-example.md` walks through the same run.

Contents:
- [Setup](#setup)
- [Symbol ids](#symbol-ids)
- [`get_next_cartography_batch`](#get_next_cartography_batch)
- [`submit_symbol_models`](#submit_symbol_models): schema, validation, the report, examples
- [`submit_module_rollup`](#submit_module_rollup)
- [`record_execution_flow`](#record_execution_flow)
- [Reading tools you will use while mapping](#reading-tools)

## Setup

The server is a single binary that speaks MCP over stdio, run against one repository:

```sh
cargo build --release            # in the code-atlas-mcp checkout; needs clang/libclang
./target/release/code-atlas-mcp --repo /path/to/repo
```

Register it with the client. For Claude Code, put this in `.mcp.json` at the repository root:

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

- **State**: lives in `<repo>/.code_atlas/`. Add that directory to `.gitignore`.
- **Embeddings**: computed locally with an ONNX model that downloads on first use. Without network access, set `"env": {"CODE_ATLAS_EMBEDDER": "hash"}` for a bag-of-words fallback. Semantic search gets weaker; everything else is unchanged.
- **Built but not registered**: `scripts/atlas_call.py` in this skill calls one tool per invocation over stdio. State persists in `.code_atlas/` between calls, so you can work through the whole workflow with it:

  ```sh
  python3 scripts/atlas_call.py <repo> sync_repository_skeleton
  python3 scripts/atlas_call.py <repo> get_next_cartography_batch '{"batch_size": 5}'
  python3 scripts/atlas_call.py <repo> submit_symbol_models @models.json
  ```

  Set `CODE_ATLAS_BIN` if the binary is not on `PATH` as `code-atlas-mcp`.

## Symbol ids

Ids look like `app/db.py::NoteRepo::search#method:4e9f`: the file path, the qualified name, the kind, and a short hash of the parameter shape.
- Top-level functions have an empty scope: `app/auth.py::::verify_token#fn:9a98`.
- Copy ids exactly from batches, zooms or search results. Never build them by hand: the hash part is not derivable.

## `get_next_cartography_batch`

| Argument | Meaning |
|---|---|
| `batch_size` | Symbols to return, default 5, max 15. A mutually recursive group is never split, so you can get more. |
| `scope_path_prefix` | Only symbols whose file path starts with this (`src/auth`, `app/db.py`). |
| `root_entrypoint_id` | Only the transitive call cone under this symbol id. |
| `prioritize_high_risk` | Ready symbols with risk flags or high complexity first. |
| `include_types` | Also queue structs, enums, traits and consts (default: functions and methods only). |
| `include_tests` | Also queue test code (default: excluded). |

What the batch markdown shows:
- **Header**: symbols still queued and groups blocked on unmapped callees.
- **Per symbol**: kind, visibility, location, status (`UNMAPPED`/`STALE`), complexity, topological level, static `Risk flags`, the doc comment, the signature and the source (clipped after a few hundred lines).
- **Callees**: each with its edge confidence (`STATIC_EXACT`/`STATIC_HEURISTIC`, or an earlier `LLM_*` verdict), the call line, and the callee's existing intent, assumptions and error surface.
- **Markers**:
  - `STRICT CONTRACT REQUIRED`: the submission fails without a `strict_contract`.
  - `MUTUALLY RECURSIVE`: submit the group together.
  - `**UNVERIFIED: return LLM_VERIFIED or LLM_PRUNED**`: on a heuristic edge that needs a verdict.
  - `Previous model (code has since changed — re-verify)`: on stale symbols.
  - `SOURCE UNAVAILABLE`: the file changed since the last sync. Sync again.

## `submit_symbol_models`

```text
{ "models": [ SymbolModel, ... ] }        // one entry per symbol, each id at most once

SymbolModel {
  symbol_id:                  string      // exact id from the batch
  intent_summary:             string      // non-empty, 1–2 sentences
  mechanism_prose:            string      // non-empty; embedded and full-text indexed
  domain_tags:                [string]    // required, may be []
  promote_to_strict_contract: bool        // optional, default false
  strict_contract:            StrictContract | omitted
  resolved_edges:             [ResolvedEdge]   // required, may be []
}

StrictContract {
  invariants_assumed:      [string]   // required
  invariants_enforced:     [string]   // required
  state_mutations:         [string]   // required
  error_surface:           [string]   // required
  concurrency_assumptions: [string]   // optional, default []
  trust_boundary:          bool       // required
  taint_sources:           [string]   // optional, default []
  taint_sinks:             [string]   // optional, default []
  audit_smells:            [string]   // required
  risk_score:              integer    // required, 1..=10
}

ResolvedEdge {
  to_symbol_id:          string   // exact id of the target
  edge_kind:             "CALLS" | "PASSES_TYPE" | "RETURNS_TYPE" | "MUTATES_STATE" | "READS_STATE" | "IMPLEMENTS"
  resolution:            "LLM_VERIFIED" | "LLM_PRUNED"
  edge_semantics:        string   // required, also when pruning: say why
  data_taint_propagated: bool     // optional, default false
}
```

### What the server rejects

If any model fails, nothing in the submission is written. Fix every listed error and resend the whole payload. The rules:
- `models` must not be empty, and each `symbol_id` may appear only once and must exist.
- `intent_summary` and `mechanism_prose` must not be blank.
- `strict_contract` is required when the symbol is marked `STRICT CONTRACT REQUIRED` (complexity ≥ 5, static risk flags, or an external entrypoint) or when you set `promote_to_strict_contract: true`.
- `risk_score` must be in 1..=10.
- If the symbol has static risk flags (`unchecked_unwrap`, `unsafe_usage`, `raw_sql`, ...), `invariants_assumed` and `error_surface` may not both be empty. Account for each flagged hazard: the panic path of an unwrap, what an unsafe block relies on.
- Every `to_symbol_id` must exist.
- `LLM_PRUNED` needs an existing edge of that `edge_kind` from this symbol to that target. Any existing edge can be pruned, `STATIC_EXACT` included. `LLM_VERIFIED` on a missing edge creates it.

A rejection looks like this:

```json
{
  "error": "validation_failed",
  "errors": [
    {"field": "strict_contract", "message": "required for this symbol (external entrypoint)",
     "symbol_id": "app/server.py::::show_note#fn:9a98"},
    {"field": "resolved_edges[0]",
     "message": "cannot prune app/server.py::::show_note#fn:9a98 -> app/db.py::UserRepo::find#method:8490: no such CALLS edge exists",
     "symbol_id": "app/server.py::::show_note#fn:9a98"}
  ],
  "note": "Nothing was written. Fix every listed field and resubmit the full payload."
}
```

### The report

```json
{"accepted": 4, "strict_contracts_written": 4, "edges_verified": 3, "edges_pruned": 1, "edges_created": 0,
 "unresolved_heuristic_edges": [], "topology_recomputed": true, "embedder": "hash-fallback"}
```

- `unresolved_heuristic_edges`: `{from, to}` pairs for guessed edges you gave no verdict. Keep it empty; resubmit the `from` symbols with verdicts.
- `topology_recomputed`: a prune or a new edge changed the call graph. Expect the queue order and the blocked counts to shift.

### Example: lightweight model

For plumbing with no input handling, no privilege and no sensitive operation, a lightweight model is enough:

```json
{"models": [
  {"symbol_id": "app/notes.py::NoteService::__init__#method:4e9f",
   "intent_summary": "Wires the note service to its note and user repositories.",
   "mechanism_prose": "Stores a NoteRepo as self.notes and a UserRepo as self.users (see app/server.py); get_note, search_notes and rename call through them.",
   "domain_tags": ["notes", "service-wiring"],
   "resolved_edges": []}
]}
```

### Example: strict model that prunes a wrong exact edge

`Database.query` calls `self.conn.execute(...)`, which is sqlite3's `Connection.execute`. The static linker had linked it to the sibling method `Database.execute` as `STATIC_EXACT`. Prune it and say why.

```json
{"models": [
  {"symbol_id": "app/db.py::Database::query#method:4e9f",
   "intent_summary": "Runs a SELECT on the shared sqlite3 connection with bound parameters and returns all rows as tuples.",
   "mechanism_prose": "Calls sqlite3's Connection.execute(sql, params) directly (not Database.execute, so no commit) and fetchall(). Parameter values are bound, but `sql` runs verbatim: NoteRepo.search passes an f-string with user input here, which is a SQL injection.",
   "domain_tags": ["sql", "sqlite", "read", "query"],
   "promote_to_strict_contract": true,
   "strict_contract": {
     "invariants_assumed": ["`sql` contains no untrusted text; untrusted values travel in `params`"],
     "invariants_enforced": ["values in params are bound"],
     "state_mutations": [],
     "error_surface": ["sqlite3 errors (including syntax errors from injected SQL) propagate"],
     "trust_boundary": false,
     "taint_sources": [],
     "taint_sinks": ["sqlite3 Connection.execute of `sql`"],
     "audit_smells": ["sink for NoteRepo.search's interpolated SQL"],
     "risk_score": 6},
   "resolved_edges": [
     {"to_symbol_id": "app/db.py::Database::execute#method:4e9f", "edge_kind": "CALLS", "resolution": "LLM_PRUNED",
      "edge_semantics": "Wrong target: `self.conn.execute` is sqlite3.Connection.execute, not Database.execute (which would also commit)."}]}
]}
```

### Example: verdicts on guessed edges, with taint

`users.find(user_id)` was matched by name to both `NoteRepo.find` and `UserRepo.find`. The wiring in `app/server.py` shows `users` is always a `UserRepo`.

```json
{"models": [
  {"symbol_id": "app/auth.py::::current_user#fn:bc21",
   "intent_summary": "Authenticates a request from its Authorization header and returns the user row (id, name, is_admin), or None for a bad signature or expired token. Missing or malformed headers and tokens raise (IndexError/ValueError) instead of returning None, and so does an unknown user id.",
   "mechanism_prose": "parse_bearer(headers.get(\"Authorization\", \"\")) then verify_token(token); a None user id returns None; otherwise users.find(user_id) on the UserRepo wired in app/server.py. The default \"\" for a missing header guarantees an IndexError in parse_bearer, so unauthenticated requests produce 500s, not 401s. The only authentication gate for NoteService.",
   "domain_tags": ["authentication", "session", "request-user", "authorization"],
   "promote_to_strict_contract": true,
   "strict_contract": {
     "invariants_assumed": ["headers is a mapping with .get", "users is a UserRepo (passed in by NoteService)", "Authorization header is present and well formed (else parse_bearer/verify_token raise)"],
     "invariants_enforced": ["token signature and expiry via verify_token"],
     "state_mutations": [],
     "error_surface": ["IndexError from parse_bearer on a missing/one-word header", "ValueError from verify_token on malformed tokens", "IndexError from UserRepo.find for unknown user ids", "returns None on bad signature or expiry"],
     "trust_boundary": true,
     "taint_sources": ["headers['Authorization'] (HTTP request)"],
     "taint_sinks": [],
     "audit_smells": ["L27: missing header defaults to \"\" which crashes parse_bearer; unauthenticated requests 500 instead of 401", "authentication relies on the hard-coded SECRET (forgeable tokens)"],
     "risk_score": 7},
   "resolved_edges": [
     {"to_symbol_id": "app/auth.py::::parse_bearer#fn:9a98", "edge_kind": "CALLS", "resolution": "LLM_VERIFIED",
      "edge_semantics": "Passes the raw Authorization header (\"\" when absent).", "data_taint_propagated": true},
     {"to_symbol_id": "app/auth.py::::verify_token#fn:9a98", "edge_kind": "CALLS", "resolution": "LLM_VERIFIED",
      "edge_semantics": "Passes the attacker-supplied token string; None means unauthenticated.", "data_taint_propagated": true},
     {"to_symbol_id": "app/db.py::UserRepo::find#method:8490", "edge_kind": "CALLS", "resolution": "LLM_VERIFIED",
      "edge_semantics": "`users` is the UserRepo NoteService was built with; looks up the authenticated user id from the verified token."},
     {"to_symbol_id": "app/db.py::NoteRepo::find#method:8490", "edge_kind": "CALLS", "resolution": "LLM_PRUNED",
      "edge_semantics": "Name match only: `users` is always a UserRepo, never a NoteRepo."}]}
]}
```

Exact edges you agree with (`parse_bearer`, `verify_token`) need no verdict. Verifying them anyway records why the connection exists and lets you mark taint, which the auditor's taint paths depend on.

When the target genuinely can't be decided, for example a caller-supplied `repo.save(record)` with no callers in the repo, verify every plausible target and say in `edge_semantics` which is likely and why. A prune claims the call can never happen.

## `submit_module_rollup`

```json
{"module_path": "app",
 "architectural_role": "The whole notes service: Flask routes (server.py) call NoteService (notes.py), which authenticates each request with HMAC session tokens (auth.py) and reads or writes sqlite through NoteRepo/UserRepo over one shared Database connection (db.py).",
 "key_abstractions": ["NoteService (per-request auth, then repository calls)", "current_user / verify_token (user_id.expires.hmac tokens)", "Database (shared sqlite3 connection; query vs execute+commit)", "NoteRepo / UserRepo (row tuples, no ownership checks)"],
 "cross_module_contracts": ["Repositories never check ownership or privilege; NoteService must (get_note does not)", "Database.query/execute bind params but run `sql` verbatim; callers must not interpolate input (NoteRepo.search does)", "current_user returns None only for bad signature or expiry; other auth failures raise"],
 "known_architectural_risks": ["Hard-coded HMAC SECRET in auth.py makes every session token forgeable", "No authorization layer: object access is decided per method and is missing on reads (IDOR)", "Uncaught exceptions turn authentication failures into 500s", "Single module-level sqlite connection shared across Flask threads"]}
```

- `module_path`: a directory or a file, relative to the repo root.
- `parent_module_path`: optional; defaults to the enclosing directory.
- `architectural_role` must not be empty, and the path must contain indexed files.
- The report gives `unmapped_symbols_in_module`. The rollup is saved even when that is not 0, but it then describes code nobody has read. Finish the mapping first.

## `record_execution_flow`

```json
{"flow_id": "flow::note_search",
 "name": "Search my notes (GET /search?q=)",
 "entrypoint_symbol_ids": ["app/server.py::::search#fn:f3bf"],
 "terminal_sink_symbol_ids": ["app/db.py::Database::query#method:4e9f"],
 "narrative_summary": "The `q` query parameter and the Authorization header enter search(); NoteService.search_notes authenticates via current_user and passes the user's id and the raw term to NoteRepo.search, which interpolates both into a SELECT string executed by Database.query.",
 "state_lifecycle_summary": "Read-only: no rows change. The only state consulted is the users row for the token's user id and the notes table.",
 "security_boundaries_crossed": ["HTTP request -> application (q, Authorization)", "application -> SQL engine (interpolated SQL string)"],
 "steps": [
   {"symbol_id": "app/server.py::::search#fn:f3bf", "role_in_flow": "Route handler; extracts q and headers", "critical_assumptions_at_step": "Assumes downstream code treats q safely."},
   {"symbol_id": "app/notes.py::NoteService::search_notes#method:4e9f", "role_in_flow": "Authenticates, scopes the search to the user", "critical_assumptions_at_step": "Scoping to user[0] is enforced only inside the SQL string, which q can rewrite."},
   {"symbol_id": "app/auth.py::::current_user#fn:bc21", "role_in_flow": "Token authentication", "critical_assumptions_at_step": "Authorization header present and well formed; SECRET not public (it is hard-coded)."},
   {"symbol_id": "app/db.py::NoteRepo::search#method:4e9f", "role_in_flow": "Builds the SQL with f-string interpolation", "critical_assumptions_at_step": "term contains no SQL: violated, this is the injection point."},
   {"symbol_id": "app/db.py::Database::query#method:4e9f", "role_in_flow": "Executes the SQL", "critical_assumptions_at_step": "sql contains no untrusted text: violated by NoteRepo.search."}]}
```

- Steps are in execution order; the first is step 1.
- A symbol may appear only once per flow. Describe a loop or a re-entry in the step's `role_in_flow` instead of repeating the step.
- Every id in `steps`, `entrypoint_symbol_ids` and `terminal_sink_symbol_ids` must exist, and `narrative_summary` must not be empty.
- The `flow::` prefix is added if you leave it out.
- Recording an existing `flow_id` again replaces that flow.

## Reading tools

These are the auditor's tools, and you will use them while mapping too:

| Tool | Arguments | Use while mapping |
|---|---|---|
| `get_macro_atlas` | `scope_path_prefix`, `max_entrypoints` | Coverage, entrypoints, directories ready for rollups, existing flows. |
| `inspect_symbol_zoom` | `symbol_ids` (≤ 5), `include_raw_source`, `max_neighbors_per_direction`, `neighbor_cursor` | Callers, callees and the module/flow context of a symbol; tracing flows. |
| `expand_function_cfg` | `symbol_id` | The branch structure of long functions: early returns, error exits, loops. |
| `semantic_atlas_search` | `query`, `scopes` (`SYMBOLS`/`MODULES`/`FLOWS`), `limit`, `scope_path_prefix` | Recalling mapped symbols for rollups; checking your prose is findable. |
| `query_audit_surface` | `query_mode` (`HIGH_RISK_CONTRACTS`, `TRUST_BOUNDARY_TO_SINK_PATHS`, `UNCHECKED_ASSUMPTION_HOTSPOTS`, `AMBIGUOUS_UNVERIFIED_EDGES`), `min_risk_score`, `scope_path_prefix`, `limit` | The final checks: leftover guessed edges, and whether the high-risk list matches reality. |
| `trace_connective_paths` | `source_symbol_id`, `target_symbol_id`, `mode` (`SHORTEST`/`HIGHEST_RISK`/`TAINT`), `max_hop_depth`, `limit` | Checking that a flow's taint edges really connect. |
