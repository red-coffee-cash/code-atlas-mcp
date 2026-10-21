#!/usr/bin/env python3
"""Grades one eval run of the cartographer skill by querying the atlas it left behind.

    grade.py <run-dir> <eval-name>

<run-dir> holds notes-app/ (the mapped repository) and outputs/ (calls.jsonl from ATLAS_CALL_LOG,
report.md). Writes <run-dir>/grading.json with one {text, passed, evidence} per assertion of the
eval's eval_metadata.json, in order. Assertions about the final report need human or model
judgement; they are written with passed=null and the report excerpt as evidence.
Run only after the agent finished: the atlas database is single-process.
"""
import json
import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
CALL = os.path.join(os.path.dirname(HERE), "scripts", "atlas_call.py")

IDS = {
    "search_route": "app/server.py::::search#fn:f3bf",
    "show_note": "app/server.py::::show_note#fn:9a98",
    "search_notes": "app/notes.py::NoteService::search_notes#method:4e9f",
    "get_note": "app/notes.py::NoteService::get_note#method:4e9f",
    "repo_search": "app/db.py::NoteRepo::search#method:4e9f",
    "note_find": "app/db.py::NoteRepo::find#method:8490",
    "user_find": "app/db.py::UserRepo::find#method:8490",
    "query": "app/db.py::Database::query#method:4e9f",
    "execute": "app/db.py::Database::execute#method:4e9f",
    "current_user": "app/auth.py::::current_user#fn:bc21",
    "verify_token": "app/auth.py::::verify_token#fn:9a98",
    "parse_bearer": "app/auth.py::::parse_bearer#fn:9a98",
    "sign": "app/auth.py::::sign#fn:9a98",
}


SHOP = {
    "login": "shop/app.py::::login#fn:f3bf",
    "checkout_route": "shop/app.py::::checkout#fn:9a98",
    "payments_webhook": "shop/app.py::::payments_webhook#fn:f3bf",
    "admin_export": "shop/app.py::::admin_export#fn:f3bf",
    "check_password": "shop/auth.py::::check_password#fn:bc21",
    "safe_next": "shop/auth.py::::safe_next#fn:9a98",
    "escape_html": "shop/html.py::::escape_html#fn:9a98",
    "render_receipt": "shop/html.py::::render_receipt#fn:bc21",
    "apply_discount": "shop/orders.py::::apply_discount#fn:bc21",
    "service_checkout": "shop/orders.py::OrderService::checkout#method:e2ec",
    "verify_signature": "shop/payments/webhook.py::::verify_signature#fn:bc21",
    "handle_refund": "shop/payments/webhook.py::::handle_refund#fn:bc21",
    "handle_payment_succeeded": "shop/payments/webhook.py::::handle_payment_succeeded#fn:bc21",
    "handle_webhook": "shop/payments/webhook.py::::handle_webhook#fn:613a",
    "export_orders": "shop/reports.py::::export_orders#fn:bc21",
    "db_query": "shop/storage.py::Db::query#method:4e9f",
    "db_execute": "shop/storage.py::Db::execute#method:4e9f",
    "refund_save": "shop/storage.py::RefundRepo::save#method:8490",
}
IDS.update(SHOP)


class Run:
    def __init__(self, run_dir):
        repos = [d for d in sorted(os.listdir(run_dir)) if os.path.isdir(os.path.join(run_dir, d)) and d != "outputs"]
        self.repo = os.path.join(run_dir, repos[0])
        self.outputs = os.path.join(run_dir, "outputs")
        log = os.path.join(self.outputs, "calls.jsonl")
        self.calls = [json.loads(l) for l in open(log)] if os.path.exists(log) else []
        rep = os.path.join(self.outputs, "report.md")
        self.report = open(rep).read() if os.path.exists(rep) else ""
        self._zoom = {}

    def tool(self, name, args):
        env = dict(os.environ, CODE_ATLAS_EMBEDDER="hash")
        env.pop("ATLAS_CALL_LOG", None)
        out = subprocess.run([sys.executable, "-I", CALL, self.repo, name, json.dumps(args)],
                             capture_output=True, text=True, env=env, timeout=300)
        return out.stdout

    def zoom(self, key):
        if key not in self._zoom:
            self._zoom[key] = self.tool("inspect_symbol_zoom", {"symbol_ids": [IDS[key]], "include_raw_source": False,
                                                                "max_neighbors_per_direction": 50})
        return self._zoom[key]

    def contract(self, key):
        z = self.zoom(key)
        m = re.search(r"## \[DISTANCE 0.*?(?=\n## \[DISTANCE 1|\Z)", z, re.S)
        return m.group(0) if m else ""

    def callees(self, key):
        z = self.zoom(key)
        m = re.search(r"## \[DISTANCE 1: DOWNSTREAM CALLEES.*", z, re.S)
        return m.group(0) if m else ""

    def header(self, key):
        return "\n".join(self.zoom(key).splitlines()[:3])

    def submitted(self):
        return [m["symbol_id"] for c in self.calls if c["tool"] == "submit_symbol_models" and c["exit"] == 0
                for m in c["arguments"].get("models", [])]


def risk(header):
    m = re.search(r"Risk Score:\*\* `(\d+)/10`", header)
    return int(m.group(1)) if m else None


def coverage(atlas):
    nums = {k: int(v) for k, v in re.findall(r"\*\*(mapped|stale|unmapped|Unverified heuristic edges|module rollups|execution flows):\*\* (\d+)", atlas)}
    m = re.search(r"\*\*Callable symbols:\*\* (\d+)", atlas)
    nums["callable"] = int(m.group(1)) if m else -1
    return nums


def edge_line(callees, target):
    m = re.search(r"\d+\. \*\*`" + re.escape(IDS[target]) + r"`\*\*.*?(?=\n\d+\. \*\*`|\Z)", callees, re.S)
    return m.group(0) if m else None


def manual(text, run):
    return {"text": text, "passed": None, "evidence": "needs judgement; report: " + run.report[:1500]}


def check(text, passed, evidence):
    return {"text": text, "passed": bool(passed), "evidence": evidence[:1500]}


def grade_full_map(run, assertions):
    atlas = run.tool("get_macro_atlas", {})
    cov = coverage(atlas)
    out = []
    a = iter(assertions)
    out.append(check(next(a), cov.get("callable") == 20 and cov.get("mapped") == 20 and cov.get("stale") == 0
                     and cov.get("Unverified heuristic edges") == 0, json.dumps(cov)))
    h, e = run.header("repo_search"), edge_line(run.callees("repo_search"), "query") or ""
    out.append(check(next(a), "STRICT_MAPPED" in h and "Trust Boundary:** `TRUE`" in h and (risk(h) or 0) >= 8
                     and "Taint Propagated: TRUE" in e, h + "\n" + e))
    c = run.contract("get_note")
    out.append(check(next(a), re.search(r"owner|idor|ownership", c, re.I), c))
    g, cu = run.callees("get_note"), run.callees("current_user")
    out.append(check(next(a), edge_line(g, "user_find") is None and edge_line(cu, "note_find") is None
                     and edge_line(g, "note_find") and edge_line(cu, "user_find"),
                     f"get_note callees:\n{g}\ncurrent_user callees:\n{cu}"))
    q = run.callees("query")
    out.append(check(next(a), edge_line(q, "execute") is None, q or "no callees"))
    sv = run.contract("sign") + run.contract("verify_token")
    out.append(check(next(a), re.search(r"hard-?coded|committed|dev-secret|in (the )?source|literal", sv, re.I), sv))
    fl = re.search(r"## Execution flows.*?(?=\n## |\Z)", atlas, re.S)
    flows = fl.group(0) if fl else ""
    both = "flow::" in run.zoom("search_route") and "flow::" in run.zoom("show_note")
    out.append(check(next(a), cov.get("module rollups", 0) >= 1 and "`app`" in atlas and both,
                     f"rollups={cov.get('module rollups')} flows={cov.get('execution flows')}\n{flows}"))
    t = run.tool("trace_connective_paths", {"source_symbol_id": IDS["search_route"], "target_symbol_id": IDS["query"],
                                            "mode": "TAINT"})
    out.append(check(next(a), IDS["repo_search"] in t and "no path" not in t.lower(), t))
    out.append(manual(next(a), run))
    return out


def grade_auth_only(run, assertions):
    out = []
    a = iter(assertions)
    batches = [c for c in run.calls if c["tool"] == "get_next_cartography_batch"]
    scoped = all(str(c["arguments"].get("scope_path_prefix", "")).startswith("app/auth") for c in batches)
    out.append(check(next(a), batches and scoped, json.dumps([c["arguments"] for c in batches])))
    subs = run.submitted()
    outside = [s for s in subs if not s.startswith("app/auth.py")]
    out.append(check(next(a), subs and not outside, f"submitted={subs}"))
    c = run.contract("verify_token")
    out.append(check(next(a), re.search(r"constant[- ]time|compare_digest|timing|ValueError", c, re.I), c))
    out.append(manual(next(a), run))
    return out


def grade_refresh(run, assertions):
    out = []
    a = iter(assertions)
    tools = [c["tool"] for c in run.calls]
    first_sync = tools.index("sync_repository_skeleton") if "sync_repository_skeleton" in tools else 10 ** 6
    first_batch = tools.index("get_next_cartography_batch") if "get_next_cartography_batch" in tools else 10 ** 6
    out.append(check(next(a), first_sync < first_batch, f"call order: {tools}"))
    claims = re.compile(r"(?<!no )(?<!not )(?<!fixed )(is|makes this|this is|: a) (a )?SQL injection|interpolat", re.I)
    subs = run.submitted()
    h, c = run.header("repo_search"), run.contract("repo_search")
    out.append(check(next(a), IDS["repo_search"] in subs and "Fresh ✓" in h and not claims.search(c), h + "\n" + c))
    callers = {k: run.contract(k) for k in ("search_notes", "search_route")}
    ok = all(IDS[k] in subs and not claims.search(v) for k, v in callers.items())
    out.append(check(next(a), ok, "\n\n".join(f"{k} resubmitted={IDS[k] in subs}\n{v}" for k, v in callers.items())))
    flows = [c["arguments"] for c in run.calls if c["tool"] == "record_execution_flow" and c["exit"] == 0
             and "note_search" in str(c["arguments"].get("flow_id", ""))]
    out.append(check(next(a), flows and not claims.search(json.dumps(flows)), json.dumps(flows)))
    cov = coverage(run.tool("get_macro_atlas", {}))
    out.append(check(next(a), cov.get("stale") == 0, json.dumps(cov)))
    return out


ONLY_LT = re.compile(r"""only[^.]{0,40}(<|less-than)|(does not|doesn't|never|not)[^.]{0,40}(escape|handle|replace)[^.]{0,40}(>|&|quote|")|incomplete|partial""", re.I)
OPEN_REDIRECT = re.compile(r"//|protocol-relative|scheme-relative", re.I)
USER_ID_UNUSED = re.compile(r"user_id[^.]{0,80}(unused|never used|ignored|not used|never checked|not checked|discarded)|no ownership|(ownership|owner)[^.]{0,40}(not|never)|any (user's |other )?order|IDOR", re.I)
MISSING_HEADER = re.compile(r"(missing|absent|no|without|none)[^.]{0,40}(header|signature)[^.]{0,80}(accept|bypass|skip|pass|allow|true|unsigned)|unsigned[^.]{0,60}(accept|allow|pass|reach|through)|legacy", re.I)
OLD_REJECT = "unsigned or forged events are rejected"


def edges_to(run, key, *targets):
    return all(edge_line(run.callees(key), t) for t in targets)


def self_checked(run):
    tools = [c["tool"] for c in run.calls]
    subs = [i for i, t in enumerate(tools) if t == "submit_symbol_models"]
    return bool(subs) and any(t in ("query_audit_surface", "semantic_atlas_search") for t in tools[subs[-1] + 1:])


def flows_logged(run, needle=""):
    return [c["arguments"] for c in run.calls if c["tool"] == "record_execution_flow" and c["exit"] == 0
            and needle in str(c["arguments"].get("flow_id", ""))]


def trace(run, src, dst):
    t = run.tool("trace_connective_paths", {"source_symbol_id": IDS[src], "target_symbol_id": IDS[dst], "mode": "TAINT"})
    return ("found" in t and " 0 of" not in t and IDS[dst] in t), t


def grade_shop_full_map(run, assertions):
    a = iter(assertions)
    out = []
    cov = coverage(run.tool("get_macro_atlas", {}))
    out.append(check(next(a), cov.get("callable") == 31 and cov.get("mapped") == 31 and cov.get("stale") == 0
                     and cov.get("Unverified heuristic edges") == 0, json.dumps(cov)))
    q = run.callees("db_query")
    out.append(check(next(a), "_MAPPED" in run.header("db_query") and edge_line(q, "db_execute") is None,
                     run.header("db_query") + "\n" + q))
    out.append(check(next(a), edges_to(run, "handle_webhook", "handle_refund", "handle_payment_succeeded"), run.callees("handle_webhook")))
    ok, t = trace(run, "payments_webhook", "refund_save")
    out.append(check(next(a), ok, t))
    h, c = run.header("safe_next"), run.contract("safe_next")
    out.append(check(next(a), "STRICT_MAPPED" in h and "Trust Boundary:** `TRUE`" in h and OPEN_REDIRECT.search(c), h + "\n" + c))
    c = run.contract("escape_html")
    out.append(check(next(a), ONLY_LT.search(c) and "all five" not in c.lower(), c))
    c = run.contract("service_checkout")
    out.append(check(next(a), USER_ID_UNUSED.search(c), c))
    ok, t = trace(run, "checkout_route", "escape_html")
    out.append(check(next(a), ok, t))
    out.append(check(next(a), self_checked(run), str([c["tool"] for c in run.calls])))
    out.append(manual(next(a), run))
    return out


def grade_shop_continue(run, assertions):
    a = iter(assertions)
    out = []
    subs = run.submitted()
    c = run.contract("escape_html")
    out.append(check(next(a), IDS["escape_html"] in subs and ONLY_LT.search(c) and "all five" not in c.lower(), c))
    h, c = run.header("safe_next"), run.contract("safe_next")
    out.append(check(next(a), IDS["safe_next"] in subs and "STRICT_MAPPED" in h and "Trust Boundary:** `TRUE`" in h
                     and OPEN_REDIRECT.search(c), h + "\n" + c))
    c = run.contract("render_receipt")
    out.append(check(next(a), re.search(r"xss|incomplete|only[^.]{0,30}<|not[^.]{0,30}(fully|safely|properly) escaped|unescaped", c, re.I)
                     and not re.search(r"safely escaped|safe escaping", c, re.I), c))
    cov = coverage(run.tool("get_macro_atlas", {}))
    out.append(check(next(a), cov.get("mapped") == 31 and cov.get("Unverified heuristic edges") == 0, json.dumps(cov)))
    out.append(manual(next(a), run))
    return out


def grade_shop_regression(run, assertions):
    a = iter(assertions)
    out = []
    subs = run.submitted()
    h, c = run.header("verify_signature"), run.contract("verify_signature")
    out.append(check(next(a), IDS["verify_signature"] in subs and (risk(h) or 0) >= 7 and MISSING_HEADER.search(c), h + "\n" + c))
    c = run.contract("handle_webhook")
    out.append(check(next(a), IDS["handle_webhook"] in subs and OLD_REJECT not in c and MISSING_HEADER.search(c), c))
    out.append(check(next(a), IDS["payments_webhook"] in subs, f"submitted={subs}"))
    hs = {k: run.contract(k) for k in ("handle_refund", "handle_payment_succeeded")}
    out.append(check(next(a), any(IDS[k] in subs and re.search(r"unsigned|missing[^.]{0,20}(header|signature)|forg|bypass|legacy", v, re.I)
                                  for k, v in hs.items()), "\n\n".join(f"{k} resubmitted={IDS[k] in subs}\n{v}" for k, v in hs.items())))
    fl = flows_logged(run, "payment_webhook")
    out.append(check(next(a), fl and MISSING_HEADER.search(json.dumps(fl[-1])), json.dumps(fl)))
    cov = coverage(run.tool("get_macro_atlas", {}))
    synced = any(c["tool"] == "sync_repository_skeleton" for c in run.calls)
    out.append(check(next(a), synced and cov.get("stale") == 0, f"synced={synced} {json.dumps(cov)}"))
    out.append(manual(next(a), run))
    return out


def grade_shop_priority(run, assertions):
    a = iter(assertions)
    out = []
    hs = [run.header(k) for k in ("handle_refund", "handle_payment_succeeded")]
    out.append(check(next(a), all("STRICT_MAPPED" in h for h in hs) and edges_to(run, "handle_webhook", "handle_refund", "handle_payment_succeeded"),
                     "\n".join(hs) + "\n" + run.callees("handle_webhook")))
    banned = [IDS[k] for k in ("login", "checkout_route", "admin_export", "export_orders", "render_receipt", "safe_next",
                               "check_password", "apply_discount", "service_checkout")]
    subs = run.submitted()
    out.append(check(next(a), subs and not [s for s in subs if s in banned], f"submitted={subs}"))
    fl = [f for f in flows_logged(run) if IDS["payments_webhook"] in f.get("entrypoint_symbol_ids", [])
          and any(s.get("symbol_id") in (IDS["handle_refund"], IDS["handle_payment_succeeded"]) for s in f.get("steps", []))]
    out.append(check(next(a), fl, json.dumps(flows_logged(run))))
    out.append(manual(next(a), run))
    return out


def main():
    run_dir, name = sys.argv[1], sys.argv[2]
    meta = json.load(open(os.path.join(os.path.dirname(run_dir.rstrip("/")), "eval_metadata.json")))
    run = Run(run_dir)
    grader = {"full-map": grade_full_map, "auth-only-scope": grade_auth_only, "refresh-after-fix": grade_refresh,
              "shop-full-map": grade_shop_full_map, "shop-continue": grade_shop_continue,
              "shop-regression-refresh": grade_shop_regression, "shop-webhook-priority": grade_shop_priority}[name]
    expectations = grader(run, meta["assertions"])
    graded = [e for e in expectations if e["passed"] is not None]
    summary = {"passed": sum(e["passed"] for e in graded), "failed": sum(not e["passed"] for e in graded),
               "total": len(graded), "pass_rate": round(sum(e["passed"] for e in graded) / max(len(graded), 1), 3)}
    json.dump({"expectations": expectations, "summary": summary}, open(os.path.join(run_dir, "grading.json"), "w"), indent=1)
    print(name, os.path.basename(run_dir.rstrip("/")), summary)


if __name__ == "__main__":
    main()
