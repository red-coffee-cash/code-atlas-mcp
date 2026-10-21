---
name: code-atlas-cartographer
description: Explore a codebase and record what you learn in the code-atlas MCP server (the Cartographer role). Sync the structural skeleton, orient top-down, map every symbol bottom-up with an adversarial contract, verify or prune the guessed call edges, then write module rollups and end-to-end execution flows that an auditing agent can query later. Use this whenever the user wants to build, populate, extend or refresh a code atlas; "map", "index", "learn", "document" or "explore" a repository so it can be audited or questioned later; run the cartographer; or bring the atlas up to date after code changed. Use it as soon as code-atlas tools such as sync_repository_skeleton, get_next_cartography_batch or submit_symbol_models are available and the task is to understand a codebase, even if the user never says "atlas".
---

# Code Atlas Cartographer

code-atlas-mcp keeps a persistent model of a codebase in an embedded database:
- **The skeleton** (automatic): symbols, a static call graph, risk flags and a bottom-up work queue.
- **Your notes** on top of it: what each function really does, what it silently assumes, which guessed edges are real, how modules fit together, and how requests flow from entrypoints to sinks.

An Auditor agent will later answer questions and hunt bugs **from your notes instead of rereading the code**. That changes what "good" means:

- A plausible but wrong note is worse than no note: the auditor trusts it and stops looking.
- The most valuable thing you record is what the code assumes but does not check (`invariants_assumed`). Bugs live where a callee assumes something its caller never guarantees.
- The deliverable is data in the MCP. Chat summaries are lost when the session ends; the atlas is not.

Read `references/payloads.md` before your first submission: it has the exact schemas, the validation rules the server enforces, and complete example payloads. `references/worked-example.md` walks through one batch end to end: what the server shows, what to look for, and the submission it should produce. Read it when unsure how deep or adversarial a model should be.

## 0. Preconditions

The tools come from the `code-atlas` MCP server. Depending on the client they may be prefixed, e.g. `mcp__code-atlas__sync_repository_skeleton`. If none of them is available:
- Do not imitate the workflow in chat.
- If the server binary is built but not registered, you can still do the job: `scripts/atlas_call.py <repo> <tool> '<json>'` calls one tool per invocation, and state persists between calls. See `references/payloads.md#setup`.
- Otherwise, tell the user the server needs building and registering. Point them to the `.mcp.json` snippet and the offline embedder switch in the same section, and stop.

Settle the scope from the request before you start:
- the whole repository;
- a path (`scope_path_prefix`);
- the call cone under one entrypoint (`root_entrypoint_id`);
- or a refresh after changes.

Note any focus the user names ("auth", "payment flows"). A large repository mapped in full can take many hundreds of batches. When the user hasn't said "everything", propose starting with the riskiest entrypoints (§7).

## 1. Sync the skeleton

Call `sync_repository_skeleton`. Read its report rather than skimming it:
- `files_failed` / `files_unsupported`: code the atlas cannot see (unsupported languages, parse failures). Tell the user; the auditor must not treat those areas as covered.
- `symbols_total`, `edges_heuristic`: the size of the job and how many call edges are guesses.
- `symbols_stale`: code that changed since it was last mapped; it re-enters the queue.

Re-run the sync whenever source files change, including edits you make yourself. Batches read the source live, and a changed file shows as "SOURCE UNAVAILABLE" until you sync.

## 2. Orient top-down before mapping

The queue is bottom-up: leaf functions first, so each caller sees its callees' contracts. The catch is that a leaf alone rarely shows whether its input is attacker-controlled. `parse_header(buf)` is harmless plumbing if `buf` comes from a config file, and a trust boundary if it comes off the network. A short top-down survey gives you that context.

1. `get_macro_atlas` (with your scope). It shows:
   - coverage: mapped / stale / unmapped, and unverified heuristic edges;
   - entrypoints;
   - any rollups and flows already recorded. If a previous session already did work, resume it rather than redo it.
2. Read the files that explain the system:
   - the README and the manifests (`Cargo.toml`, `package.json`, `go.mod`, `Gemfile`, ...);
   - `main` and router/route tables, CLI definitions, configuration loading;
   - the entrypoints the atlas lists.
3. Write yourself a short working note. Keep it in your context or a scratch file, not in the atlas:
   - **Purpose**: what the system does, in a sentence.
   - **Untrusted inputs**: where they enter (HTTP handlers, CLI args, uploaded or parsed files, queue consumers, environment, deserialization).
   - **Sensitive sinks and privilege checks**: SQL, shell, filesystem, crypto, authn/authz, money movement.
   - **Domain vocabulary**: the words a person would search for ("refund", "tenant", "session token").

Don't submit models during orientation. The queue order exists so every caller is written with its callees' contracts in hand.

## 3. Map the queue

Repeat until the batch comes back empty:

1. **Fetch**: `get_next_cartography_batch`.
   - `batch_size`: 5 by default; up to 15 for files of trivial accessors; 3 for dense, risky code.
   - Pass `scope_path_prefix` or `root_entrypoint_id` if you scoped the job.
   - `prioritize_high_risk: true` maps flagged and complex symbols first, which is useful when time is short.

   Each symbol arrives with its source, location, complexity, static risk flags, its callees' existing contracts, and its outgoing edges, each marked `STATIC_EXACT` or `STATIC_HEURISTIC`.
2. **Understand each symbol**:
   - Read the source. When it is clipped, read the rest of the file.
   - For a long, branchy function, `expand_function_cfg` lays out its branches, early returns and error exits.
   - `inspect_symbol_zoom` shows callers and the wider neighbourhood when you need to know how a symbol is used.
3. **Check the callee contracts you are shown** against the call site: argument order, units, nullability, who validates what. They were written by an earlier pass and can be wrong.
   - If one is wrong, say so in this symbol's `edge_semantics` and resubmit a corrected model for the callee. A mapped symbol can be resubmitted at any time.
   - Don't silently build on a wrong contract.
4. **Decide light or strict**. A `strict_contract` is required when the batch says **STRICT CONTRACT REQUIRED** (complexity ≥ 5, static risk flags, or an external entrypoint). Also set `promote_to_strict_contract: true` and write one whenever the symbol:
   - handles untrusted input;
   - enforces a privilege or authentication check;
   - performs a sensitive operation: SQL, shell, deserialization, file or memory writes, crypto, money or permission changes.

   Promote even when the code looks simple. The auditor's risk and taint queries only see strict contracts.
5. **Resolve edges**. Every `STATIC_HEURISTIC` edge leaving the symbol is a guess by name.
   - Give each one a verdict in `resolved_edges`: `LLM_VERIFIED` if this call can really reach that target, `LLM_PRUNED` if it cannot (different receiver type, unrelated class or module that merely shares the method name).
   - `STATIC_EXACT` only means the linker was confident. Check those edges too, and prune one that is wrong (`self.conn.execute(...)` linked to the class's own `execute` when it is the database driver's).
   - Prune only what cannot happen. When the target can't be decided (a caller-supplied object with no callers in the repo), verify every plausible target and say in `edge_semantics` which is likely and why.
   - Add the edges the static linker could not see as `LLM_VERIFIED`, with the right `edge_kind`: callbacks, dynamic dispatch through tables or dependency injection, framework wiring, state written or read (`MUTATES_STATE` / `READS_STATE`).
   - Set `data_taint_propagated: true` on any edge that carries attacker-influenced data. The auditor's taint paths walk only those edges.
6. **Submit the whole batch** in one `submit_symbol_models` call. A group marked **MUTUALLY RECURSIVE** must be in the same submission.
7. **Read the report**:
   - `unresolved_heuristic_edges` lists guessed edges you gave no verdict. Resubmit those symbols with verdicts.
   - On a validation error nothing was written. Fix every listed field and resend the whole payload.
   - Common causes: an id not copied exactly; a missing contract; a risk flag such as `unchecked_unwrap` not accounted for in `invariants_assumed` or `error_surface`; pruning an edge that doesn't exist.

When the batch is empty, its header says how many symbols are still queued and how many groups are blocked:
- **Nothing queued**: the scope is mapped.
- **Symbols queued but every group blocked**: each waits on an unmapped callee. Usually the files changed (sync and retry). Otherwise the callee is test code; map it with `include_tests: true`, or prune the edge if the call is not real.

Every 10 batches or so, give the user a one-line progress update from the batch header (symbols still queued). Mapping a real codebase is long; silence looks like a hang.

## 4. Writing each model

| Field | Write | Avoid |
|---|---|---|
| `intent_summary` | 1–2 sentences of behavioural truth, including the unhappy path: "Verifies an HS256 session JWT and returns its claims; returns `None` for expired or malformed tokens but accepts tokens without an `aud` claim." | Restating the name ("Validates the token"). |
| `mechanism_prose` | How it works: the order of callee calls, state touched, loops, early exits, defaults. Use the domain vocabulary; this text is what semantic search indexes. | Line-by-line narration; vague words like "handles", "processes". |
| `domain_tags` | 2–5 lowercase concepts: `jwt`, `session-lifecycle`, `sql`, `billing`. | File or function names. |
| `invariants_assumed` | Preconditions relied on but **not checked**: "`user_id` was already authorised for this tenant by the caller", "`items` is non-empty (indexes `[0]`)", "the lock on `cache` is held". | Things the code does check (those go in `invariants_enforced`). |
| `invariants_enforced` | Checks it actually performs, precisely: "rejects `amount <= 0`", "compares HMAC in constant time". | "Validates input". |
| `state_mutations` | DB rows, files, globals, caches, shared structures written. | — |
| `error_surface` | Panics, raised/returned errors, swallowed errors, partial-failure states ("debits before crediting; a crash in between loses money"). | — |
| `concurrency_assumptions` | Lock order, reentrancy, cancellation and retry hazards. | — |
| `trust_boundary` | `true` if it directly receives attacker-influenced input or enforces privilege. | — |
| `taint_sources` / `taint_sinks` | The untrusted parameters entering; the sensitive operations executed. | — |
| `audit_smells` | Concrete, checkable hypotheses with lines: "L42: `alg` read from the token header selects the verifier". | Generic advice ("should add more validation"). |
| `risk_score` | 1–3 plumbing; 4–6 fragile; 7–8 exploitable if an assumption is violated; 9–10 matches a known vulnerable pattern. | Inflating everything to look thorough. |

**The calibration test:** would an auditor reading only your model, and not the code, reach the same conclusions as one who read the code? If your model says "validates the session", the auditor will stop looking at session handling. Write only what you verified in the source, and put unverified suspicions in `audit_smells`, phrased as hypotheses.

## 5. Module rollups

When `get_macro_atlas` lists directories as **Ready for `submit_module_rollup`**, summarise each one. Go bottom-up: leaf directories first, then their parents, so a parent rollup can cite its children. Each rollup records:
- `architectural_role`: what the module is for in the system;
- `key_abstractions`: the central types and functions;
- `cross_module_contracts`: what other modules may rely on, and what this module expects of them;
- `known_architectural_risks`: risks that only show at this level, such as a cache shared across tenants or two places that must stay in sync.

Build the rollup from the symbol models: `inspect_symbol_zoom` on the key symbols and `semantic_atlas_search` scoped to the path. Re-reading every file is unnecessary.

## 6. Execution flows

Record the important end-to-end paths with `record_execution_flow`: the entrypoints from the atlas plus the ones your orientation found (login, checkout, upload, admin actions, job handlers).

- Trace down from the entrypoint with `inspect_symbol_zoom` (its Distance-1 callees) along the main path to the terminal sinks.
- List the steps in order. In `critical_assumptions_at_step`, carry over the `invariants_assumed` that matter at that step: a flow is where an assumption made in one function meets the guarantee another function does or does not provide.
- Name the trust boundaries crossed.
- Use a stable `flow_id` such as `flow::password_reset`. Re-recording the same id replaces the flow, which is how you update one.

## 7. Large repositories and refreshes

- **Too big to map in one go**: map the cones of the riskiest entrypoints first (`root_entrypoint_id`, `prioritize_high_risk: true`), record their flows, then widen. Partial coverage of what matters beats shallow coverage of everything. Tell the user what is and isn't covered.
- **Tests** are excluded from the queue and from coverage by default. Map them (`include_tests: true`) only when asked or when production code calls them.
- **Types, structs, traits**: `include_types: true` queues them. That is worthwhile when the domain lives in data types (state machines, protocol messages, permission enums).
- **Refresh after changes**: sync, then run the queue again. Stale symbols come back with their previous summary marked "re-verify"; check whether the change breaks it rather than copying it.
  - Only the edited symbols go stale. Their callers keep contracts that describe the old behaviour: after fixing an injection in `search`, its caller's model still says "SQL injection reachable by any user".
  - So whenever a re-mapped contract changes (an assumption added or removed, a bug fixed or introduced), run `inspect_symbol_zoom` on the symbol (it lists the callers and the flows the symbol takes part in) and resubmit any model, rollup or flow (`record_execution_flow` with the same `flow_id`) that cites the old behaviour.

## 8. Finish: verify, then report

Before you report done:
1. Run `get_macro_atlas` on your scope. Coverage should be complete, with 0 stale. Unverified heuristic edges should be 0; any left are listed by `query_audit_surface` with `query_mode: AMBIGUOUS_UNVERIFIED_EDGES`. Every ready directory should have a rollup, and the main entrypoints should have flows.
2. Spot-check retrieval: ask `semantic_atlas_search` two or three questions in the domain vocabulary ("where are refunds authorised?"). If the right symbols don't come back, their `mechanism_prose` lacks the words. Improve and resubmit them.
3. Run `query_audit_surface` with `HIGH_RISK_CONTRACTS` and check that the list matches your own sense of where the danger is. A risky symbol that is missing usually means a missing promotion to a strict contract.

Then tell the user, briefly:
- coverage (mapped / total in scope) and anything left out, and why (unsupported languages, failed files, out of scope);
- the highest-risk contracts, one line each;
- the flows recorded.

Present `audit_smells` as hypotheses for the auditor, not as confirmed bugs. Confirming them is the Auditor's job (the server's `auditor_prompt` describes it).
