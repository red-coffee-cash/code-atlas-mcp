# Writing AUDIT.md and scoring with CVSS 3.1

## CVSS 3.1 base vectors

Score from the **attacker's real position** (what step 4 established) and the **end-to-end impact** of the whole chain, not the local operation. Always finish with `python3 scripts/cvss.py CVSS:3.1/<vector>` and copy its score and rating; never estimate.

The eight base metrics:

| Metric | Values | Choose by |
|---|---|---|
| `AV` Attack Vector | N network, A adjacent, L local, P physical | How the attacker reaches it. A public HTTP endpoint is `N`; a CLI or local file is `L`. |
| `AC` Attack Complexity | L low, H high | `H` only if success needs conditions outside the attacker's control (a race, a specific config). Ordinary crafting is `L`. |
| `PR` Privileges Required | N none, L low, H high | What the attacker must already have. Anonymous is `N`; any logged-in user is `L`; admin-only is `H`. |
| `UI` User Interaction | N none, R required | `R` if a victim must act (click a link). A reflected open redirect or stored payload a victim views is `R`. |
| `S` Scope | U unchanged, C changed | `C` when the impact crosses into another component or trust domain — e.g. SSRF reaching internal services, or stored XSS running in other users' sessions. |
| `C`/`I`/`A` | H / L / N | Confidentiality, Integrity, Availability of what the attack affects. |

Worked examples (all verified against `scripts/cvss.py`):
- **Unauthenticated SQL injection reading any row**: `AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H` → 9.8 Critical.
- **IDOR: any logged-in user reads another user's record**: `AV:N/AC:L/PR:L/UI:N/S:U/C:H/I:N/A:N` → 6.5 Medium.
- **Command execution reachable only by an admin**: `AV:N/AC:L/PR:H/UI:N/S:U/C:H/I:H/A:H` → 7.2 High (raise to think about `S:C` if it escapes the app's own context).
- **Open redirect after login**: `AV:N/AC:L/PR:N/UI:R/S:C/C:L/I:L/A:N` → 6.1 Medium.
- **Unauthenticated endpoint that forges a state change (e.g. webhook accepted without a valid signature)**: `AV:N/AC:L/PR:N/UI:N/S:U/C:N/I:H/A:N` → 7.5 High.

If a guard bypass gates the bug, the gate usually sets `PR`/`AC`: a bypass that removes an admin check drops `PR` from `H` toward `N`.

### Correctness defects

A bug no attacker can trigger has no honest CVSS. Mark it **Defect (CVSS n/a)** and state the impact plainly ("every checkout raises `TypeError` before charging; no order can be paid"). If an attacker *can* trigger the fault, it is a finding with a vector — usually `A` (a crash they can force) or `I` (state they can corrupt).

## AUDIT.md template

Append a new dated section if the file exists; never overwrite earlier audits. Order findings by score, defects after findings.

```markdown
# Security & correctness audit — <repo or scope>
_<YYYY-MM-DD> · code-atlas-auditor · atlas embedder: <name>_

## Scope and method
<What was audited, and how: synced, mapped gaps, atlas queries, source review, PoCs. One short paragraph.>

## Coverage
- Mapped in scope: <n>/<n> callable symbols (<0 stale>).
- Mapped this run to close gaps: <list, or "none">.
- Not covered by the atlas, reviewed from source: <failed/unsupported files, or "none">.
- Not reviewed: <anything out of scope, so the reader knows the edges>.

## Summary
| # | Finding | Severity (CVSS) | Confidence | Location |
|---|---------|-----------------|------------|----------|
| 1 | <title> | 9.8 Critical | Confirmed | shop/reports.py:10 |
| 2 | <title> | 6.5 Medium | Inferred | shop/orders.py:12 |

## Findings

### 1. <title> — <score> <rating> (`CVSS:3.1/...`) — Confirmed | Inferred
- **Location:** `<symbol_id>`, `<file:line>`
- **Reachable from:** `<entrypoint>` (<who can reach it>)
- **Path:** <entrypoint → … → sink, naming the hops>
- **Trigger / PoC:** <the exact input; for Confirmed, the command run and the output observed, in a code block>
- **Why existing checks don't stop it:** <the guard that is missing or bypassable, and how>
- **Impact:** <what an attacker gains end to end>
- **Fix:** <the smallest change that closes it>

## Defects (not attacker-triggerable)
### D1. <title> — Defect (CVSS n/a)
- **Location / Impact / Fix** as above, impact in plain words.

## Rejected candidates
Things that looked suspicious and are safe, so they are not re-chased next time.
- `<symbol>` — <why it is safe: guarded by X / not reachable / value is constant>.

## Atlas notes
Where verification showed the model wrong or stale, for the next cartography pass.
- `<symbol>` — <contract claims X; the code does Y>.
```

Rules:
- Every finding names a real path from an entrypoint and the specific guard that fails. "Could be dangerous" is not a finding.
- `Confirmed` only with a PoC that ran; everything else is `Inferred`.
- Keep fixes concrete and minimal — the change that closes the hole, not a redesign.
- Don't inflate severity. A reader who finds one padded Critical discounts the rest.
