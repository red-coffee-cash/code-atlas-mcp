#!/usr/bin/env python3
"""Call one code-atlas-mcp tool over stdio, for when the server is built but not registered as an MCP server.

    atlas_call.py <repo> <tool> [json-arguments | @file.json]
    atlas_call.py <repo> prompt:<name> [json-arguments]     # e.g. prompt:cartographer_prompt
    atlas_call.py <repo> tools                               # list the tools and their input schemas

Starts the server on <repo>, performs the MCP handshake, calls the tool, prints its text result
and exits: 0 on success, 2 when the tool reports an error (e.g. a validation failure), 1 on a
protocol error. State persists in <repo>/.code_atlas/ between calls; run one call at a time.

The binary is $CODE_ATLAS_BIN, else `code-atlas-mcp` on PATH. Set CODE_ATLAS_EMBEDDER=hash to
avoid downloading the embedding model. With ATLAS_CALL_LOG set, each call is appended to that file
as a JSON line ({"tool": ..., "arguments": ..., "exit": ...}).
"""
import json
import os
import shutil
import subprocess
import sys


def main():
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    repo, tool = sys.argv[1], sys.argv[2]
    raw = sys.argv[3] if len(sys.argv) > 3 else "{}"
    if raw.startswith("@"):
        with open(raw[1:]) as f:
            raw = f.read()
    args = json.loads(raw)

    binary = os.environ.get("CODE_ATLAS_BIN") or shutil.which("code-atlas-mcp")
    if not binary:
        sys.exit("code-atlas-mcp not found: set CODE_ATLAS_BIN to the built binary")
    proc = subprocess.Popen([binary, "--repo", repo], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.DEVNULL, text=True)

    def send(msg):
        proc.stdin.write(json.dumps(msg) + "\n")
        proc.stdin.flush()

    def recv(want_id):
        for line in proc.stdout:
            msg = json.loads(line)
            if msg.get("id") == want_id:
                return msg
        sys.exit("the server exited before answering (is another call holding the database?)")

    send({"jsonrpc": "2.0", "id": 1, "method": "initialize",
          "params": {"protocolVersion": "2025-03-26", "capabilities": {},
                     "clientInfo": {"name": "atlas_call", "version": "1"}}})
    recv(1)
    send({"jsonrpc": "2.0", "method": "notifications/initialized"})
    if tool == "tools":
        send({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
        res = recv(2)
        proc.stdin.close()
        proc.wait(timeout=120)
        for t in res["result"]["tools"]:
            print(f"## {t['name']}\n{t.get('description', '')}\n```json\n{json.dumps(t.get('inputSchema', {}), indent=1)}\n```\n")
        return
    if tool.startswith("prompt:"):
        send({"jsonrpc": "2.0", "id": 2, "method": "prompts/get",
              "params": {"name": tool[len("prompt:"):], "arguments": args}})
    else:
        send({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": tool, "arguments": args}})
    res = recv(2)
    proc.stdin.close()
    proc.wait(timeout=120)

    code = 1 if "error" in res else 2 if res["result"].get("isError") else 0
    if os.environ.get("ATLAS_CALL_LOG"):
        with open(os.environ["ATLAS_CALL_LOG"], "a") as log:
            log.write(json.dumps({"tool": tool, "arguments": args, "exit": code}) + "\n")
    if "error" in res:
        print(json.dumps(res["error"], indent=2))
        sys.exit(1)
    result = res["result"]
    parts = result.get("content") or [m["content"] for m in result.get("messages", [])]
    for part in parts:
        print(part.get("text", json.dumps(part)))
    if result.get("isError"):
        sys.exit(2)


if __name__ == "__main__":
    main()
