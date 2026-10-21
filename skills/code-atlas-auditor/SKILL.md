---
name: code-atlas-auditor
description: Audit a codebase for security vulnerabilities and impactful correctness bugs by reading the source directly and using the code-atlas MCP server (the Auditor role) as a lens on how the code connects — its callers and the data they pass, its callees' assumptions, the flows and entrypoints that reach it — then write the findings to AUDIT.md with CVSS 3.1 scores. Re-syncs and maps any missing or stale part of the atlas before trusting it, follows connectivity from leads into the source, verifies every candidate against the code (and with an isolated proof of concept when possible), and rejects what a caller's guard already stops. Use this whenever the user asks to audit, pentest, security-review or bug-hunt a repository, or asks whether a feature, endpoint or change is safe, and code-atlas tools (get_macro_atlas, query_audit_surface, trace_connective_paths, inspect_symbol_zoom) are available, even if they never mention the atlas.
---

# Code Atlas Auditor

**You audit by reading the source. The atlas is your lens on how the code in front of you connects to the rest of the system.**

Read the actual functions. When you are looking at one, a function alone rarely tells you whether it is dangerous: `run_export(path)` is fine if `path` is a constant and a hole if it comes off a request. The atlas answers exactly the questions the source in front of you leaves open:
- **Who reaches this, and with what?** Its callers, the data each passes, and whether that data is attacker-controlled (taint).
- **What does it rely on?** Its callees' contracts — what they assume but do not check.
- **Where does it sit?** Which execution flows and entrypoints reach it, so you know who can trigger it and with what privilege.

So the loop is: read code → ask the atlas how it wires in → follow a real caller or callee to the next piece of source → repeat. A Cartographer built the model (intent, assumptions, taint-marked edges, flows); it is a map of the territory, not the territory. The source is ground truth.

Your job is **verified** findings: real vulnerabilities and correctness bugs with real impact, each traced through source you read, scored with CVSS 3.1, and written to `AUDIT.md`.

Three things decide whether an audit is worth reading:

- **Only verified findings.** The atlas holds hypotheses (`audit_smells`, risk scores, even edge directions) written by another agent, and they can be wrong or stale. Nothing is a finding until you have read the path in the source yourself. Prefer a proof of concept that ran over one you reasoned about.
- **No false alarms.** A suspicious sink that a caller's check already guards is not a finding. Reporting it costs the reader time and their trust in everything else on the list. Record it under "Rejected candidates" with the reason instead.
- **Freshness first.** The atlas can lag the code. When an atlas claim and the source disagree, the source wins — and the gap itself is a lead, because the most dangerous bug is often the recent change the model hasn't seen.

Read `references/hunting.md` before you hunt: the connectivity queries, a bug-class checklist and the proof-of-concept rules. Read `references/report.md` before you write: the `AUDIT.md` template and how to choose CVSS vectors. `scripts/cvss.py` computes scores; never compute them by hand.

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

Rollups' `known_architectural_risks` and flows' `critical_assumptions_at_step` are good places to start reading.

## 3. Pick where to start reading

You read the source; the atlas points you at the source worth reading first. Build a reading list of symbols and files from:

1. `query_audit_surface`, scoped as needed, in each mode (`references/hunting.md` says what each returns):
   - `HIGH_RISK_CONTRACTS` — the Cartographer's worst cases.
   - `TRUST_BOUNDARY_TO_SINK_PATHS` — request data reaching a sink.
   - `UNCHECKED_ASSUMPTION_HOTSPOTS` — a callee assumes what its caller may not enforce.
   - `AMBIGUOUS_UNVERIFIED_EDGES` — unresolved edges; blind spots in the taint paths, not findings.
2. `trace_connective_paths` from each entrypoint to each sink it might reach (`TAINT`, then `SHORTEST`; taint edges are sparse, so a missing taint path is not proof of safety).
3. `semantic_atlas_search` by bug-class vocabulary: "where are refunds authorised?", "where is the redirect target validated?", "where is user input put into a shell command?".
4. The stale or newly mapped symbols from step 1, the money/permission/data-deletion flows, and anything the user named.

These are leads — symbols to open, not findings. Keep the list; then read.

## 4. Read the source, using the atlas to connect it

This is the core loop. Take a lead, open the actual code, and use the atlas to see how it wires into the system — then follow a real edge to the next piece of source.

For each symbol you are reading:

1. **Read its source**, from the repo or from `inspect_symbol_zoom` (with source). For long or stateful functions, `expand_function_cfg` shows the branches, error exits and loops. What does this code *do* with its inputs, and what does it assume about them?
2. **Connect it upward — who reaches this, with what?** `inspect_symbol_zoom` lists the callers, each edge's semantics, and whether it carries taint. For a value that worries you, walk to each caller's source (every one, not just the first) and see where that value comes from. A sink is only a finding if some real caller feeds it attacker-controlled data past every guard.
3. **Connect it downward — what does it rely on?** The callees' contracts show what they assume but don't check. When this function fails to guarantee what a callee assumes, that mismatch is the bug; open the callee's source to confirm the assumption is real and unmet.
4. **Place it — who can trigger it?** The flows and reachable entrypoints the zoom lists tell you the attacker's position (anonymous, logged-in, admin, webhook) — which decides reachability and later the CVSS vector.
5. **Trust the source over the atlas.** If the code does not match the contract, believe the code, note the discrepancy for the report's "Atlas notes", and keep going.

Then **decide**:
- **A finding**: you traced a real path, in source, from an entrypoint to an impact, past every guard.
- **A rejected candidate**: a guard, type conversion or constant makes it safe. One line saying which.
- **Unverifiable**: say what you'd need.

A chain is one finding with its full path, scored end to end — e.g. a guard bypass in `require_admin` opening command execution in an admin-only function. Follow edges outward from each lead until the paths close; new leads you discover by reading (a caller that looks unguarded, a callee misused) go on the list.

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
