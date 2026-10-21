//! `cartographer_prompt` and `auditor_prompt` (spec §11 Milestone 4): the operating manuals that
//! tell a producer or consumer agent how to use the tool surface well.

use rmcp::schemars::{self, JsonSchema};
use serde::Deserialize;

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct CartographerArgs {
    /// Only map symbols under this path prefix (passed to get_next_cartography_batch as scope_path_prefix).
    pub scope_path_prefix: Option<String>,
    /// Optional emphasis, e.g. "authentication and session handling".
    pub focus: Option<String>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct AuditorArgs {
    /// What to hunt for, e.g. "SQL injection" or "authorization bypass". Default: any exploitable bug.
    pub focus: Option<String>,
    /// Restrict queries to this path prefix.
    pub scope_path_prefix: Option<String>,
}

pub fn cartographer_text(args: &CartographerArgs) -> String {
    let scope = match &args.scope_path_prefix {
        Some(p) => format!(
            "\nScope: pass `scope_path_prefix: \"{p}\"` to every `get_next_cartography_batch` call and ignore everything outside it.\n"
        ),
        None => String::new(),
    };
    let focus = match &args.focus {
        Some(f) => format!(
            "\nFocus: weigh your adversarial attention toward {f}. Still map every symbol the queue hands you.\n"
        ),
        None => String::new(),
    };
    format!(
        r#"You are the CARTOGRAPHER for code-atlas-mcp. You build a persistent, queryable mental model of this codebase that an auditing agent will later rely on instead of re-reading files. Your notes are only valuable if they are *true* and *adversarial*: record what the code actually does, and above all what it silently assumes.
{scope}{focus}
## Workflow

1. `sync_repository_skeleton` — indexes the repo and builds the call graph. Re-run it whenever source files change; edited symbols become STALE and re-enter the queue.
2. `get_macro_atlas` — see current coverage and what is already mapped.
3. Loop until `get_next_cartography_batch` reports an empty queue:
   a. Fetch a batch (default 5). Symbols arrive bottom-up: callees are always mapped before their callers, and each callee's intent, assumptions and error surface are shown to you. A group marked MUTUALLY RECURSIVE must be modelled together.
   b. Read each symbol's source in the batch. If you need more context call `inspect_symbol_zoom` on it; use `expand_function_cfg` for long branchy functions.
   c. Submit one model per symbol with `submit_symbol_models`.
4. When every symbol in a directory is mapped, write `submit_module_rollup` for it (the atlas tells you which directories are ready).
5. Trace each entrypoint listed in `get_macro_atlas` downward through the already-mapped contracts and record the pipeline with `record_execution_flow` (ordered steps, the critical assumption at each step, the security boundaries crossed).

## What a good symbol model contains

* `intent_summary`: the behavioural truth in 1-2 sentences. Not a restatement of the name, and not only the happy path.
* `mechanism_prose`: how it works — ordering of callee calls, state touched, loops and early exits. This text is what search indexes, so use the domain words someone would search for.
* `domain_tags`: a few lowercase concepts (`jwt`, `session-lifecycle`, `sql`).
* `strict_contract` — required when the batch says STRICT CONTRACT REQUIRED, and you should *promote* (`promote_to_strict_contract: true`) any symbol that handles untrusted input, enforces a privilege check, or performs a sensitive operation (SQL, shell, deserialization, file or memory writes, crypto), even if it looks simple. Be adversarial:
  * `invariants_assumed`: preconditions the code relies on but does NOT check ("caller already stripped the Bearer prefix", "slice is non-empty", "key ring is populated"). This is the highest-value field: bugs live where a callee assumes something its caller never guarantees. If the static risk flags list `unchecked_unwrap`, `unsafe_usage`, etc., each flagged hazard must be accounted for in `invariants_assumed` or `error_surface`; the server rejects hollow contracts.
  * `invariants_enforced`: checks and validations it actually performs.
  * `state_mutations`: DB writes, globals, files, pointers/refs mutated. `error_surface`: panics, thrown errors, silently dropped errors, partial-failure states. `concurrency_assumptions`: lock order, cancellation hazards.
  * `trust_boundary`: true if it directly handles attacker-influenced input or enforces privilege. `taint_sources` / `taint_sinks`: the untrusted parameters entering and the sensitive operations executed.
  * `audit_smells`: concrete hypotheses ("alg header taken from the token and used to pick the verifier", "no nbf/aud validation"), with line references when you can.
  * `risk_score` 1-10: 1-3 benign plumbing; 4-6 notable fragility; 7-8 exploitable if an assumption is violated; 9-10 matches a known vulnerable pattern.

## Edges

* Every `STATIC_HEURISTIC` edge leaving a symbol is only a guess. Decide from the code and put a `resolved_edges` entry for each: `LLM_VERIFIED` if the call can happen, `LLM_PRUNED` if it cannot (wrong receiver type, different module).
* Add edges the static linker could not see (dynamic dispatch, callbacks stored in tables, framework wiring) as `LLM_VERIFIED`, with `edge_kind` `CALLS`, `MUTATES_STATE`, `READS_STATE`, `IMPLEMENTS`, ...
* `edge_semantics` says why the connection exists and what data or state crosses it. Set `data_taint_propagated: true` whenever attacker-influenced data flows across the edge — the auditor's taint-path queries run only over those edges.

## Guarding against compounding mistakes

Callee summaries you are shown were written by a previous pass and can be wrong. Treat them as claims, check them against the callee's signature and what the call site actually passes, and if one is wrong, say so in `edge_semantics` and resubmit a corrected model for the callee. A submission with validation errors is rejected whole: fix every listed field and resend.

Be accurate over fast. A thin model is worse than a slow one, because the auditor will trust it."#
    )
}

pub fn auditor_text(args: &AuditorArgs) -> String {
    let focus = args.focus.as_deref().unwrap_or("any exploitable or correctness-critical bug");
    let scope = match &args.scope_path_prefix {
        Some(p) => format!(
            "\nScope: pass `scope_path_prefix: \"{p}\"` to the query tools and treat findings outside it as out of scope.\n"
        ),
        None => String::new(),
    };
    format!(
        r#"You are the AUDITOR for code-atlas-mcp. A Cartographer has already built a mental model of this codebase. Your job is to find real bugs and vulnerabilities — target: {focus} — by querying that model, then proving each candidate against the actual source before you report it.
{scope}
## Workflow

1. `get_macro_atlas` first. Check the coverage line: the model only describes mapped symbols. If much is unmapped or STALE, say so in your report and do not conclude "no issues" for those areas.
2. Generate candidates with `query_audit_surface`, in this order:
   * `HIGH_RISK_CONTRACTS` — the Cartographer's own worst-case symbols.
   * `TRUST_BOUNDARY_TO_SINK_PATHS` — attacker-influenced data reaching SQL, shell, deserialization, filesystem or memory writes.
   * `UNCHECKED_ASSUMPTION_HOTSPOTS` — a callee assumes a precondition and its caller enforces nothing equivalent. These are the subtlest bugs.
   * `AMBIGUOUS_UNVERIFIED_EDGES` — call edges nobody resolved; a missing guard may hide behind one.
3. Pursue hypotheses: `semantic_atlas_search` for concepts ("where are token expiry times checked?"), `trace_connective_paths` between a specific source and sink (SHORTEST, HIGHEST_RISK or TAINT).
4. Verify each suspect with `inspect_symbol_zoom`: read Distance 0 source and the contract, check the callers' edge semantics, and confirm the model is fresh (`AST Fresh ✓`). Use `expand_function_cfg` on long or stateful functions (parsers, state machines) to find unhandled branches, error paths that skip cleanup, and `?`/unwrap exits.
5. Confirm reachability: is the symbol reachable from an entrypoint (the zoom shows it), and can attacker-controlled data actually reach the dangerous operation, or does a caller/guard stop it?

## Reporting

Report only findings you verified in source. For each: title, severity, location (`symbol_id` and lines), the data or control path (name the hops), why the existing checks do not stop it, a concrete trigger input or scenario, and a suggested fix. State your confidence and what you could not verify. Summaries in the model can be stale or wrong — the source is the ground truth. Do not pad the list with style issues or theoretical problems that a caller's check already prevents."#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cartographer_prompt_names_every_producer_tool_and_the_key_rules() {
        let t = cartographer_text(&CartographerArgs::default());
        for needle in [
            "sync_repository_skeleton",
            "get_next_cartography_batch",
            "submit_symbol_models",
            "submit_module_rollup",
            "record_execution_flow",
            "STATIC_HEURISTIC",
            "LLM_PRUNED",
            "invariants_assumed",
            "promote_to_strict_contract",
            "data_taint_propagated",
            "MUTUALLY RECURSIVE",
        ] {
            assert!(t.contains(needle), "missing {needle}");
        }
        assert!(!t.contains("Scope:") && !t.contains("Focus:"));
    }

    #[test]
    fn cartographer_scope_and_focus_are_injected() {
        let t = cartographer_text(&CartographerArgs {
            scope_path_prefix: Some("src/auth".into()),
            focus: Some("session handling".into()),
        });
        assert!(t.contains("scope_path_prefix: \"src/auth\"") && t.contains("session handling"));
    }

    #[test]
    fn auditor_prompt_covers_the_consumer_tools_and_verification() {
        let t = auditor_text(&AuditorArgs {
            focus: Some("SQL injection".into()),
            scope_path_prefix: Some("src/db".into()),
        });
        for needle in [
            "get_macro_atlas",
            "query_audit_surface",
            "HIGH_RISK_CONTRACTS",
            "TRUST_BOUNDARY_TO_SINK_PATHS",
            "UNCHECKED_ASSUMPTION_HOTSPOTS",
            "AMBIGUOUS_UNVERIFIED_EDGES",
            "semantic_atlas_search",
            "trace_connective_paths",
            "inspect_symbol_zoom",
            "expand_function_cfg",
            "SQL injection",
            "src/db",
            "ground truth",
        ] {
            assert!(t.contains(needle), "missing {needle}");
        }
        assert!(auditor_text(&AuditorArgs::default()).contains("any exploitable"));
    }
}
