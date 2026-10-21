#!/usr/bin/env python3
"""Replays every example payload in references/payloads.md against a fresh copy of the notes-app
fixture, through scripts/atlas_call.py, and fails if the server rejects one. It also checks that
the batch markers and the validation-error shape the skill describes are still what the server
emits. Run it after changing the server or the skill:

    CODE_ATLAS_BIN=/path/to/code-atlas-mcp python3 evals/check_examples.py
"""
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
SKILL = os.path.dirname(HERE)
CALL = os.path.join(SKILL, "scripts", "atlas_call.py")


def call(repo, tool, args=None, expect_ok=True):
    env = dict(os.environ)
    env.setdefault("CODE_ATLAS_EMBEDDER", "hash")
    out = subprocess.run([sys.executable, "-I", CALL, repo, tool, json.dumps(args or {})],
                         capture_output=True, text=True, env=env, timeout=300)
    if expect_ok and out.returncode != 0:
        sys.exit(f"FAIL {tool}: exit {out.returncode}\n{out.stdout}{out.stderr}")
    return out.returncode, out.stdout


def main():
    with open(os.path.join(SKILL, "references", "payloads.md")) as f:
        blocks = [json.loads(b) for b in re.findall(r"```json\n(.*?)\n```", f.read(), re.S)]
    examples = [b for b in blocks if "models" in b or "module_path" in b or "flow_id" in b]
    if len(examples) < 5:
        sys.exit(f"FAIL: expected at least 5 example payloads in payloads.md, found {len(examples)}")

    tmp = tempfile.mkdtemp()
    repo = os.path.join(tmp, "notes-app")
    shutil.copytree(os.path.join(HERE, "testdata", "notes-app"), repo)
    try:
        _, sync = call(repo, "sync_repository_skeleton")
        assert json.loads(sync)["files_parsed"] == 4, sync
        _, batch = call(repo, "get_next_cartography_batch", {"batch_size": 15})
        assert "# CARTOGRAPHY BATCH" in batch, batch

        # the documented rejection shape
        code, bad = call(repo, "submit_symbol_models", {"models": [{
            "symbol_id": "app/server.py::::show_note#fn:9a98", "intent_summary": "x", "mechanism_prose": "y",
            "domain_tags": [], "resolved_edges": []}]}, expect_ok=False)
        err = json.loads(bad)
        assert code == 2 and err["error"] == "validation_failed", bad
        assert err["errors"][0]["field"] == "strict_contract", bad

        # models first (edges they prune must still exist), then rollups and flows
        order = sorted(examples, key=lambda b: 0 if "models" in b else 1)
        for ex in order:
            tool = ("submit_symbol_models" if "models" in ex
                    else "submit_module_rollup" if "module_path" in ex else "record_execution_flow")
            _, out = call(repo, tool, ex)
            report = json.loads(out)
            if tool == "submit_symbol_models":
                assert report["accepted"] == len(ex["models"]), out
                assert report["unresolved_heuristic_edges"] == [], out
            print(f"ok  {tool}: {ex.get('flow_id') or ex.get('module_path') or [m['symbol_id'] for m in ex['models']]}")

        _, atlas = call(repo, "get_macro_atlas")
        assert "flow::note_search" in atlas, atlas
        print("ok  all example payloads accepted")
    finally:
        shutil.rmtree(tmp)


if __name__ == "__main__":
    main()
