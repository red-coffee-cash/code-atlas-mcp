# Hunting: queries, bug classes, proofs of concept

For authorised review of a codebase you or your user own or are engaged to assess. The aim is to find weaknesses so they can be fixed, and to prove them only as far as confirming they are real.

Contents:
- [What each query gives you](#queries)
- [Bug classes to check](#bug-classes)
- [Ruling candidates out](#ruling-out)
- [Proofs of concept](#poc)

## Queries

These point you at source worth reading and show how the code you are reading connects; they do not replace reading it. `inspect_symbol_zoom` is the bridge you will use most — given a symbol you have open, it shows its callers, callees, flows and reachability in one call. All take `scope_path_prefix`. Copy symbol ids exactly from tool output.

| Call | Returns | Use it for |
|---|---|---|
| `get_macro_atlas` | Coverage (mapped / stale / unmapped, unverified edges), rollups with known risks, flows, entrypoints with risk. | Freshness, attack surface, seeds. |
| `query_audit_surface {"query_mode":"HIGH_RISK_CONTRACTS","min_risk_score":6}` | Strict contracts at or above the score, with assumptions, sinks and smells. `required_risk_flags` filters by static flag. | The Cartographer's worst cases; verify each. |
| `query_audit_surface {"query_mode":"TRUST_BOUNDARY_TO_SINK_PATHS"}` | Paths from a trust boundary to a sink or state mutation over taint-carrying edges, with per-hop semantics. | Injection, unauthorised writes. |
| `query_audit_surface {"query_mode":"UNCHECKED_ASSUMPTION_HOTSPOTS"}` | Callee preconditions its callers do not enforce. | The subtlest bugs: a gap between what one function assumes and what another guarantees. |
| `query_audit_surface {"query_mode":"AMBIGUOUS_UNVERIFIED_EDGES"}` | Call edges nobody resolved. | Blind spots in the taint paths, not findings in themselves. |
| `trace_connective_paths {"source_symbol_id":...,"target_symbol_id":...,"mode":"TAINT"}` | The taint path between two symbols; `SHORTEST`/`HIGHEST_RISK` for any call path. | Confirming a route from an entrypoint to a sink. |
| `semantic_atlas_search {"query":"..."}` | Mapped symbols, rollups and flows ranked for a concept. | Reaching a bug class by its vocabulary. |
| `inspect_symbol_zoom` / `expand_function_cfg` | A symbol's source, contract, callers and callees / its branch structure. | Verification (step 4 of the skill). |

Taint edges are sparse and depend on the Cartographer marking them, so a missing taint path is a reason to look harder with `SHORTEST`, never a proof of safety.

### Static risk flags

`required_risk_flags` filters `HIGH_RISK_CONTRACTS` by the flags the frontends attach: `command_exec`, `external_io`, `raw_sql`, `unsafe_deserialization`, `xss_sink`, `concurrency_primitive`, `error_swallow`, `memory_unsafe`, `unsafe_usage`, `unchecked_unwrap`, `manual_alloc`. A flag marks a sink worth tracing back to its source; it is not itself a bug.

## Bug classes

For each entrypoint and flow in scope, ask these. Each "no" is a candidate for step 4.

**Input reaching a sink**
- Does request data reach a query, command, path, template, deserializer or redirect without being bound, escaped or validated for that sink?
- Is an escaping or validation helper actually complete for the context it is used in? (A helper can cover one context and miss another.)

**Authorization and authentication**
- Is every sensitive entrypoint behind a check that the acting principal may do this, to this object? An authentication check ("logged in") is not an authorization check ("may touch this record").
- Can a token, signature or session be forged, skipped or replayed — a hard-coded or weak secret, a non-constant-time compare, a check that passes when an input is missing, no expiry or revocation?
- Does the flag an authorization check reads come from somewhere the attacker influences?

**Correctness with real impact** (money, permissions, data loss, main-path crashes)
- Partial failure: a flow that writes in several steps with no rollback, so a mid-way failure leaves inconsistent state.
- Double application: an operation that can run twice (retry, replay, repeat submit) with no idempotency or state guard.
- Lost update: read-modify-write with no lock or version on shared state.
- A type or shape mismatch between what a function returns and how its callers use it, crashing the main path.

**Secrets and config**
- Credentials, keys or signing secrets in source.
- Error paths that return the wrong status (a 500 where a 401/403 belongs) and so hide authz failures or feed retry storms.

A found weakness in a guard plus a sink it was protecting is one finding, scored by the end-to-end impact.

## Ruling out

A candidate is **not** a finding when any link in its chain is broken. Check before reporting:
- **Not reachable.** No path from an entrypoint, or the only callers are tests or dead code. (A latent bug behind a currently-closed door is at most a low-severity note.)
- **Not controllable.** A constant, a type conversion (`<int:id>`, `int(x)`), or a prior validation fixes the value before the sink.
- **Already guarded.** A caller enforces what the callee assumes. Read *every* caller; one safe caller does not clear a sink another caller reaches unguarded.
- **Context-safe.** The incomplete-looking helper is used only where what it does is enough (e.g. element-text escaping used only in element text).

Record each ruled-out candidate in the report's "Rejected candidates" section with the one-line reason. This is as valuable as the findings: it shows the area was looked at, and it stops the next audit from re-chasing it.

## Proofs of concept

A proof of concept exists to answer one question: does the weakness actually trigger? Keep it the smallest thing that answers that, and no more.

Rules:
- **Outside the repo.** Work in a scratch directory. Never modify the repository's files, never write into its tree (including `__pycache__` — run Python with `-B` or set `PYTHONDONTWRITEBYTECODE=1`), never commit.
- **Exercise the unit, not the system.** Import or copy the one function, call it with the input that shows the issue, back it with an in-memory or scratch database and stubbed external services. You are confirming the code's behaviour, not operating a live system.
- **Harmless and local.** No network except localhost, no real credentials, nothing destructive. To show a command-execution weakness, use a payload whose only effect is a marker file or a printed string in your scratch directory — enough to demonstrate control, never a real command.
- **Confirm, don't weaponise.** One input that demonstrates the issue is the goal. Do not build a reusable exploit, chain, or anything aimed at a system you were not asked to assess.
- **Time-box.** If a proof would need the whole app running, real services, or more than a few minutes, stop and mark the finding **Inferred**, with the source reasoning that supports it.

Record the exact input and the observed result in the finding, and label it **Confirmed**. Everything you did not run is **Inferred** — honest labelling is what makes the confirmed ones worth trusting.
