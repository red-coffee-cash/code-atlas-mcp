---
name: code-atlas-auditor
description: Audit a codebase for security vulnerabilities and impactful correctness bugs using the code-atlas MCP server (the Auditor role), then write the findings to AUDIT.md with CVSS 3.1 scores. Re-syncs and maps any missing or stale part of the atlas before trusting it, generates candidates from the atlas's audit queries, taint paths and semantic search, verifies each one against the source (and with an isolated proof of concept when possible), and rejects what a caller's guard already stops. Use this whenever the user asks to audit, pentest, security-review or bug-hunt a repository, or asks whether a feature, endpoint or change is safe, and code-atlas tools (get_macro_atlas, query_audit_surface, trace_connective_paths, inspect_symbol_zoom) are available, even if they never mention the atlas.
---

# Code Atlas Auditor

A Cartographer has built a model of this codebase in code-atlas. It records:
- what each function does and what it assumes;
- which calls carry attacker-controlled data;
- how requests flow from entrypoints to sinks.

Your job is to turn that model into **verified** findings: real vulnerabilities and correctness bugs with real impact, each proven against the source, scored with CVSS 3.1, and written to `AUDIT.md`.

Three things decide whether an audit is worth reading:

- **Only verified findings.** The atlas holds hypotheses (`audit_smells`, risk scores) written by another agent, and they can be wrong or stale. A finding is reported only after you have read the code path yourself. Prefer a proof of concept that ran over one you reasoned about.
- **No false alarms.** A suspicious sink that a caller's check already guards is not a finding. Reporting it costs the reader time and their trust in everything else on the list. Record it under "Rejected candidates" with the reason instead.
- **Freshness first.** Auditing a stale model audits code that no longer exists. The most dangerous bugs are often exactly the recent change the model hasn't seen.

Read `references/hunting.md` before you generate candidates: query recipes, a bug-class checklist and the proof-of-concept rules. Read `references/report.md` before you write: the `AUDIT.md` template and how to choose CVSS vectors. `scripts/cvss.py` computes scores; never compute them by hand.

## 0. Preconditions and scope

The tools come from the `code-atlas` MCP server, possibly with a prefix such as `mcp__code-atlas__get_macro_atlas`. If they aren't available but the server binary exists, `scripts/atlas_call.py <repo> <tool> '<json>'` calls one tool per invocation. Otherwise, tell the user the server must be registered (see the cartographer skill or the server README) and stop.

Settle what is being audited:
- the whole repository;
- a path (`scope_path_prefix` on every query);
- a feature or entrypoint ("is the admin export safe?");
- a change ("audit the new coupon code").

Note the user's focus, if any. The report goes to `AUDIT.md` in the repository root unless the user names another path. If the file exists, append a new dated section rather than overwriting earlier audits.

## 1. Make sure the model is fresh and covers the scope

1. **Sync** with `sync_repository_skeleton`, always, even if the user didn't mention changes. Read the report:
   - `symbols_stale` > 0 means code changed after it was modelled.
   - `files_failed` / `files_unsupported` are code the atlas cannot see. Audit those files by reading them directly, and say so in the report.
2. **Check coverage** with `get_macro_atlas` (scoped), which shows mapped, stale and unmapped counts.
3. **Map the gap before auditing it.** If anything in your scope is stale or unmapped, bring it up to date first:
   - Use the **code-atlas-cartographer** skill if it is installed, scoped with `scope_path_prefix` or `root_entrypoint_id` to the gap.
   - Otherwise:
     - Loop `get_next_cartography_batch` with that scope and `submit_symbol_models`.
     - Give a strict contract to anything touching untrusted input, privilege or a sensitive sink.
     - Give a verdict on every `STATIC_HEURISTIC` edge.
     - Mark taint on edges that carry attacker data.
   - When a stale symbol's behaviour changed, revisit its callers (`inspect_symbol_zoom`): their contracts may still describe the old behaviour. **A stale symbol is your first audit candidate**, because it is the code nobody has reviewed.

Keep the gap mapping to what the audit needs. You are not here to re-map the whole repository.

## 2. Orient

From `get_macro_atlas`, list the entrypoints, recorded flows and module rollups. Write yourself a short attack-surface note:
- each entrypoint;
- who can reach it (anonymous, logged in, admin, provider webhook, local CLI);
- what it accepts;
- the sinks its flow reaches.

Rollups' `known_architectural_risks` and flows' `critical_assumptions_at_step` are good seeds for candidates.

## 3. Generate candidates

Work through these, in roughly this order. `references/hunting.md` explains what each one returns.

1. `query_audit_surface` in each mode, scoped as needed:
   - `HIGH_RISK_CONTRACTS`
   - `TRUST_BOUNDARY_TO_SINK_PATHS`
   - `UNCHECKED_ASSUMPTION_HOTSPOTS`
   - `AMBIGUOUS_UNVERIFIED_EDGES`

   Unverified edges are blind spots in the taint paths, not findings.
2. `trace_connective_paths` from each entrypoint to each sensitive sink it might reach, in `TAINT` mode, then in `SHORTEST` mode. Taint edges are sparse, so a missing taint path is not proof of safety.
3. `semantic_atlas_search` for each bug class relevant to the system, using its vocabulary. For example: "where are refunds authorised?", "where is the redirect target validated?", "where is user input put into a shell command?".
4. The stale or newly mapped symbols from step 1, and anything the user specifically asked about.
5. Correctness: the flows that move money, change permissions or delete data. Check their `state_mutations` and `error_surface` for:
   - partial failure;
   - double application;
   - lost updates;
   - crashes on the main path.

Keep a candidate list: symbol, suspected issue, and why.

## 4. Verify every candidate against the source

For each candidate:

1. **Read the code.** `inspect_symbol_zoom` (with source) shows the contract, the callers and their edge semantics, the callees and their assumptions, and the reachable entrypoints. For long or stateful functions, `expand_function_cfg` shows the branches, error exits and loops. Read the file itself when you need more.
2. **Reachability.** Find a real path from an entrypoint. A dangerous function nothing calls is at most a latent issue.
3. **Attacker control.** Check whether the attacker controls the value that reaches the sink, or a guard, type conversion or constant in between makes it safe. Check every caller, not just one.
4. **Guards.** Find what the existing checks actually enforce, and what bypasses them: missing keys, `None`, alternative encodings, `//host`, case, type confusion.
5. **Decide.**
   - **A finding**: reachable, controllable, not guarded.
   - **A rejected candidate**: say why it is safe, in one line.
   - **Unverifiable**: say what you'd need.

A chain counts as one finding with its full path, scored by the end-to-end impact. One example: a guard bypass in `require_admin` that exposes command injection in an admin-only function.

## 5. Confirm with a proof of concept when you can

A proof of concept turns "inferred" into "confirmed". The rules, in brief (details in `references/hunting.md`):
- Work in a scratch directory outside the repository. Never modify the repository's files, never write into it (including `__pycache__`), and never commit.
- Exercise the code directly. Import or copy the function, call it with the crafted input, and use an in-memory or scratch database and stubbed external services.
- No network access to anything but localhost, no real credentials, and nothing destructive. For command injection, use a harmless payload such as `; touch <scratchdir>/pwned` or `; echo PWNED`.
- Time-box it. If a PoC would need the full app, real services or more than a few minutes, mark the finding **Inferred** and explain the reasoning instead.

Record the exact input and the observed output in the finding.

## 6. Score

For each finding, choose a CVSS 3.1 base vector from the attacker's real position and the end-to-end impact (`references/report.md` has the metric guide and worked examples). Then run `python3 scripts/cvss.py <vector>` and copy the score and rating exactly.

Correctness bugs that no attacker can trigger have no meaningful CVSS. Label them **Defect (CVSS n/a)** and describe their impact in plain words, for example "every checkout raises TypeError; no order can be paid". When an attacker *can* trigger the failure, it gets a vector (usually an availability or integrity impact).

## 7. Write AUDIT.md, then reply briefly

Write the report with the template in `references/report.md`:
- scope and method;
- coverage, including what the atlas did not cover and what you mapped;
- a summary table;
- the findings, highest score first, each with path, trigger or PoC, why the existing checks fail, impact and fix;
- defects;
- rejected candidates;
- atlas notes.

Don't record findings in the atlas itself. If verification showed an atlas contract to be wrong, list it under "Atlas notes" so the next cartography pass can correct it.

Your chat reply is a short pointer, three lines at most:
- where the report is;
- counts by severity, and how many findings were confirmed;
- the single most important finding.

The report is the deliverable.
