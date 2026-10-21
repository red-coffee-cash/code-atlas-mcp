#!/usr/bin/env python3
"""Builds the eval fixtures into <out-dir>: shop-app (fresh), shop-continue (leaves mapped by an
'earlier session', two models planted wrong) and shop-regression (webhook path mapped, then
verify_signature changed to accept a missing header without a re-sync). Needs CODE_ATLAS_BIN.

    build_states.py <out-dir>
"""
import json
import os
import shutil
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
CALL = os.path.join(os.path.dirname(HERE), "scripts", "atlas_call.py")
SRC = os.path.join(HERE, "testdata", "shop-app")
STATES = os.path.join(HERE, "states")


def call(repo, tool, payload=None):
    env = dict(os.environ, CODE_ATLAS_EMBEDDER="hash")
    env.pop("ATLAS_CALL_LOG", None)
    args = ["@" + os.path.join(STATES, payload)] if payload else []
    out = subprocess.run([sys.executable, "-I", CALL, repo, tool, *args], capture_output=True, text=True, env=env)
    if out.returncode != 0:
        sys.exit(f"{tool} {payload}: {out.stdout}{out.stderr}")


def main():
    out = sys.argv[1]
    os.makedirs(out, exist_ok=True)
    for name in ("shop-app", "shop-continue", "shop-regression"):
        shutil.rmtree(os.path.join(out, name), ignore_errors=True)
        shutil.copytree(SRC, os.path.join(out, name))
    cont = os.path.join(out, "shop-continue")
    call(cont, "sync_repository_skeleton")
    call(cont, "submit_symbol_models", "leaves.json")
    reg = os.path.join(out, "shop-regression")
    call(reg, "sync_repository_skeleton")
    for i in range(1, 6):
        call(reg, "submit_symbol_models", f"webhook{i}.json")
    call(reg, "record_execution_flow", "webhook_flow.json")
    path = os.path.join(reg, "shop", "payments", "webhook.py")
    text = open(path).read()
    old = "def verify_signature(body, header):\n    expected"
    assert old in text
    open(path, "w").write(text.replace(old, "def verify_signature(body, header):\n    if header is None:\n"
                                            "        # the legacy provider does not sign its webhooks yet\n"
                                            "        return True\n    expected"))
    print(json.dumps({n: os.path.join(out, n) for n in ("shop-app", "shop-continue", "shop-regression")}))


if __name__ == "__main__":
    main()
