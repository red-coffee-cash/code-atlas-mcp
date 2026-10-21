# Worked example: one batch, start to finish

The repository is a small Flask notes service:
- `app/server.py`: routes
- `app/notes.py`: `NoteService`
- `app/auth.py`: HMAC session tokens
- `app/db.py`: a `Database` wrapper and two repositories

Everything below is real server output and payloads it accepted.

## Orientation

Reading `server.py` first gives the context the leaf functions lack:
- **Entrypoints**: two routes. `GET /notes/<int:note_id>` → `service.get_note(request.headers, note_id)`, and `GET /search` → `service.search_notes(request.headers, request.args.get("q", ""))`.
- **Untrusted inputs**: the Authorization header, the `q` query parameter, the note id in the path.
- **Wiring**: `NoteService(NoteRepo(db), UserRepo(db))`, so inside the service `self.notes` is always a `NoteRepo` and `self.users` a `UserRepo`. This settles guessed edges later.

The leaves (`parse_bearer`, `sign`, `Database.execute`, the constructors) came first. `Database.query` came in the second batch, and its accepted contract says:

> assumes (not checked by callee): `sql` contains no untrusted text; untrusted values travel in `params`

## The batch

Third batch, symbol 2 of 3, as the server rendered it:

````markdown
## 2. `app/db.py::NoteRepo::search#method:4e9f`
**Kind:** method · **Visibility:** pub · **Location:** app/db.py:25-28 · **Status:** UNMAPPED · **Complexity:** 1 · **Level:** 1
**Risk flags:** []
**Signature:** `def search(self, owner_id, term):`

```py
def search(self, owner_id, term):
        # term comes straight from the query string
        sql = f"SELECT id, body FROM notes WHERE owner_id = {owner_id} AND body LIKE '%{term}%'"
        return self.db.query(sql)
```

**Callees (1):**
- `app/db.py::Database::query#method:4e9f` [STATIC_EXACT] lines 28
  - call: `self.db.query(sql)`
  - signature: `def query(self, sql, params=()):`
  - intent: Runs a SELECT on the shared sqlite3 connection with bound parameters and returns all rows as tuples.
  - assumes (not checked by callee): `sql` contains no untrusted text; untrusted values travel in `params`
  - error surface: sqlite3 errors (including syntax errors from injected SQL) propagate
````

The symbol is not marked STRICT CONTRACT REQUIRED: complexity 1, and the Python frontend raised no risk flag. Static signals miss it; the reasoning below does not.

## Reasoning

1. **What does it really do?** It builds SQL by f-string interpolation of `owner_id` and `term`, then calls `query(sql)` with no params.
2. **Where do the inputs come from?** From orientation: `term` is the `q` query parameter, unmodified, via `search_notes`. So this is attacker-influenced input, a trust boundary.
3. **Compare with the callee's contract.** `Database.query` assumes `sql` holds no untrusted text. This call site breaks that assumption exactly. That pairing is what the auditor's `UNCHECKED_ASSUMPTION_HOTSPOTS` query finds, but only if both sides are written down.
4. **Promote.** It performs SQL with untrusted input, so `promote_to_strict_contract: true`, whatever the complexity.
5. **The edge.** It is `STATIC_EXACT` and correct. Verify it anyway, because attacker data crosses it: `data_taint_propagated: true` is what lets `TRUST_BOUNDARY_TO_SINK_PATHS` connect `/search` to the database.
6. **Risk.** It matches a known vulnerable pattern (string-built SQL from request input), so 9.

## The submission

```json
{"symbol_id": "app/db.py::NoteRepo::search#method:4e9f",
 "intent_summary": "Full-text-ish search of one owner's notes by substring, built by interpolating owner_id and the raw search term into the SQL string: a SQL injection.",
 "mechanism_prose": "Builds f\"SELECT id, body FROM notes WHERE owner_id = {owner_id} AND body LIKE '%{term}%'\" and runs it through Database.query without parameters. term is the `q` query-string value (search -> NoteService.search_notes), so a quote in it rewrites the WHERE clause (e.g. `' OR '1'='1` returns every user's notes; UNION SELECT reads other tables). LIKE wildcards in term are not escaped either.",
 "domain_tags": ["notes", "search", "sql", "sql-injection"],
 "promote_to_strict_contract": true,
 "strict_contract": {
   "invariants_assumed": ["term contains no quotes or SQL (it is raw user input)", "owner_id is an integer"],
   "invariants_enforced": [],
   "state_mutations": [],
   "error_surface": ["sqlite3.OperationalError on malformed injected SQL propagates as a 500"],
   "trust_boundary": true,
   "taint_sources": ["term (the `q` query parameter)"],
   "taint_sinks": ["SQL string executed by Database.query"],
   "audit_smells": ["L27: term interpolated into SQL; Database.query assumes untrusted values travel in params", "the scope restriction owner_id = ... can be bypassed via term"],
   "risk_score": 9},
 "resolved_edges": [
   {"to_symbol_id": "app/db.py::Database::query#method:4e9f", "edge_kind": "CALLS", "resolution": "LLM_VERIFIED",
    "edge_semantics": "Passes the interpolated SQL string, attacker-controlled through term, as `sql`; violates query's assumption that sql holds no untrusted text.", "data_taint_propagated": true}]}
```

### The weak version, and why it fails

```json
{"symbol_id": "app/db.py::NoteRepo::search#method:4e9f",
 "intent_summary": "Searches notes for a term.",
 "mechanism_prose": "Runs a query to find matching notes for the owner.",
 "domain_tags": ["search"],
 "resolved_edges": []}
```

The server accepts this: the symbol was not marked strict and every required field is present. It is still close to worthless, and actively harmful:
- No strict contract, so the symbol is invisible to `HIGH_RISK_CONTRACTS` and to the taint queries.
- No taint on the edge, so no path connects `/search` to SQL.
- "Runs a query" suggests nothing is wrong, and an auditor skimming the atlas moves on.

The validator only catches hollow contracts on symbols with static risk flags. Judgement does the rest.

## Resolving guessed edges in the same session

`current_user` (the next batch) shows two guessed callees for one call:

```text
- `app/db.py::NoteRepo::find#method:8490` [STATIC_HEURISTIC] lines 31 — UNVERIFIED
  - call: `users.find(user_id)`
- `app/db.py::UserRepo::find#method:8490` [STATIC_HEURISTIC] lines 31 — UNVERIFIED
  - call: `users.find(user_id)`
```

Both classes define `find`, and the parameter `users` has no type annotation. Orientation answered it: `NoteService` passes `self.users`, a `UserRepo`. Verify `UserRepo.find`, prune `NoteRepo.find` with the reason ("name match only: `users` is always a UserRepo"). The full payload is in `payloads.md`.

Two other edge situations came up in this run:
- **A wrong exact edge.** `Database.query` calls `self.conn.execute(...)`, and the static linker linked it to the sibling method `Database.execute` as `STATIC_EXACT`. It is sqlite3's `Connection.execute`. Pruned, with the reason. Exact means "the linker is confident", not "true".
- **Undecidable.** `NoteService.rename(headers, repo, record, name)` calls `repo.save(record)`, where `repo` is a parameter and nothing in the repo calls `rename`. Both `save` methods are possible, so both are verified, and the semantics say which one is likely ("the 'name' key matches UserRepo.save") and why the other cannot be ruled out.

## What the auditor sees afterwards

Once the queue was empty, `query_audit_surface` with `UNCHECKED_ASSUMPTION_HOTSPOTS` ranked this pair second:

```text
2. `app/notes.py::NoteService::search_notes#method:4e9f` → **`app/db.py::NoteRepo::search#method:4e9f`** — callee risk 9/10 · [Taint Propagated]
   * callee assumes: term contains no quotes or SQL (it is raw user input); owner_id is an integer
   * caller enforces: caller is authenticated
```

`TRUST_BOUNDARY_TO_SINK_PATHS` connected `search` → `search_notes` → `NoteRepo.search` → `Database.query` over taint-carrying edges. `semantic_atlas_search "where is the search term put into SQL?"` returned `NoteRepo.search` first.

All three results exist only because of the fields written above. The same holds for the other two bugs in this app:
- **IDOR**: `NoteRepo.find` assumes "caller checks owner_id", and `get_note` doesn't.
- **Hard-coded HMAC secret**: recorded in `sign`'s `audit_smells` and `invariants_assumed`.
