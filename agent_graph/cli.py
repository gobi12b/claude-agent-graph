"""Run workflows from the terminal: `agent-graph list`, `agent-graph run <workflow or file.yaml>`,
`agent-graph validate [workflows or files]`, `agent-graph runs`.

Runs use the same runner as the app and are saved in the same place, so they also show up on the app's
Runs page. A step that pauses for review asks you here (approve, request changes, or stop).
"""

import argparse
import json
import os
import signal
import sys
import time

from . import workflows
from .watcher import Graph

EXIT = {"succeeded": 0, "failed": 1, "stopped": 2}
NEEDS_REVIEW = 3
USE_COLOR = sys.stdout.isatty() and os.environ.get("NO_COLOR") is None


def _c(code, text):
    return f"\033[{code}m{text}\033[0m" if USE_COLOR else text


def _dur(sec):
    sec = int(max(0, sec))
    return f"{sec}s" if sec < 60 else f"{sec // 60}m {sec % 60}s" if sec < 3600 else f"{sec // 3600}h {sec % 3600 // 60}m"


def _is_file(arg):
    return arg.endswith((".yaml", ".yml")) or os.sep in arg or (os.altsep or os.sep) in arg or os.path.isfile(arg)


def _target(arg, folder=None):
    """A workflow file path, or the id/name of a workflow the app knows. Exits with a message if not found."""
    if _is_file(arg):
        try:
            return workflows.load_file(arg, folder)
        except ValueError as exc:
            sys.exit(f"{arg}: {exc}")
    wf = _find(arg)
    if folder:
        wf = workflows.validate({**wf, "cwd": os.path.abspath(os.path.expanduser(folder))})
    return wf


def _find(query):
    """A workflow by id, exact name, or the start of its name (case doesn't matter)."""
    errors = []
    wfs = workflows.list_workflows(errors)
    q = query.strip().lower()
    for test in (lambda w: w["id"] == query, lambda w: w["name"].lower() == q,
                 lambda w: w["name"].lower().startswith(q) or w["id"].startswith(q)):
        hits = [w for w in wfs if test(w)]
        if len(hits) == 1:
            return hits[0]
        if len(hits) > 1:
            names = "\n".join(f"  {w['id']:<28} {w['name']}" for w in hits)
            sys.exit(f"“{query}” matches several workflows; use the id:\n{names}")
    bad = next((e for e in errors if q in e["name"].lower()), None)
    if bad:
        sys.exit(f"{bad['name']} has a mistake, so it can't run: {bad['error']}")
    sys.exit(f"No workflow called “{query}”. See them with: agent-graph list")


def cmd_list(args):
    errors = []
    wfs = workflows.list_workflows(errors)
    if args.json:
        print(json.dumps([{k: w.get(k) for k in ("id", "name", "cwd", "file")} | {"steps": [s["name"] for s in w["steps"]]}
                          for w in wfs], indent=2))
        return 0
    if not wfs:
        print("No workflows yet. Create one in the app (agent-graph), on the Workflows page.")
    for w in wfs:
        print(f"{_c('1', w['name'])}  {_c('2', '(' + w['id'] + ')')}")
        print(f"  {' → '.join(s['name'] for s in w['steps'])}")
        print(f"  {_c('2', '📁 ' + workflows._tilde(w.get('cwd') or ''))}")
    for e in errors:
        print(_c("31", f"⚠ {e['name']} isn't loaded: {e['error']}"), file=sys.stderr)
    return 0


def cmd_runs(args):
    runner = workflows.Runner(Graph(), lambda: None)
    runs = runner.summary()[: args.limit]
    if args.json:
        print(json.dumps([{k: r.get(k) for k in ("id", "name", "status", "started", "ended", "cost", "error")} for r in runs], indent=2))
        return 0
    if not runs:
        print("No runs yet.")
    for r in runs:
        when = time.strftime("%b %d %H:%M", time.localtime(r["started"]))
        took = _dur((r.get("ended") or time.time()) - r["started"])
        print(f"{r['id']}  {when}  {r['status']:<9}  {took:>7}  ${r['cost']:.2f}  {r['name']}")
    return 0


def _ask_review(runner, rid, att):
    """The run paused for review: show the result and ask what to do."""
    print()
    print(_c("35;1", f"👤 “{att['name']}” is done and waiting for your review."))
    out = (att.get("output") or "").strip()
    if out:
        lines = out.splitlines()
        print(_c("2", "── its result " + ("(last 25 lines) " if len(lines) > 25 else "") + "──"))
        print("\n".join(lines[-25:]))
        print(_c("2", "──"))
    while True:
        choice = input("Approve and continue [a], request changes [c], or stop [s]? ").strip().lower()[:1]
        if choice in ("a", "s"):
            note = input("Notes for the next step (optional, Enter to skip): ").strip() if choice == "a" else ""
            return runner.review(rid, "approve" if choice == "a" else "stop", note)
        if choice == "c":
            fb = input("What should change? ").strip()
            if fb:
                return runner.review(rid, "revise", fb)
            print("Write what should change, or choose another option.")


def _stop_requested(*_):
    raise KeyboardInterrupt


def cmd_run(args):
    # Ctrl+C here, or Stop in the app (which sends the same signal), stops the run cleanly. Set it explicitly:
    # a process started in the background inherits "ignore Ctrl+C".
    signal.signal(signal.SIGINT, signal.default_int_handler)
    if hasattr(signal, "SIGTERM"):
        signal.signal(signal.SIGTERM, _stop_requested)
    try:
        sys.stdout.reconfigure(line_buffering=True)  # progress shows up live even when piped to a file
    except AttributeError:
        pass
    wf = _target(args.workflow, args.folder)
    runner = workflows.Runner(Graph(), lambda: None)
    interactive = sys.stdin.isatty() and not args.yes
    rid = runner.start(wf["id"], wf)
    run = runner.runs[rid]
    if not args.json:
        print(_c("1", f"▶ {wf['name']}") + _c("2", f"  ({len(wf['steps'])} steps · run {rid})"))
        print(_c("2", f"  in {workflows._tilde(wf['cwd'])}   ·   Ctrl+C stops it"))
    seen = {}
    try:
        while True:
            for att in run["attempts"]:
                key, st = att["session"], att["status"]
                if seen.get(key) == st:
                    continue
                seen[key] = st
                if args.json:
                    continue
                label = att["name"] + (f" (retry {att['attempt'] - 1})" if att["attempt"] > 1 else "") + (" (steered)" if att.get("steer") else "")
                if st == "running":
                    print(f"  {_c('33', '●')} {label}…")
                elif st == "ok":
                    took = _dur(att["ended"] - att["started"]) + " · $%.2f" % att["cost"]
                    j = att.get("judge") or {}
                    verdict = f"  {_c('36', '⚖ judge %d/100' % j['score'])}" if j.get("status") == "pass" else ""
                    print(f"  {_c('32', '✓')} {label}  {_c('2', took)}{verdict}")
                elif st == "failed":
                    print(f"  {_c('31', '✗')} {label}: {att.get('error', '')[:300]}")
                elif st == "stopped":
                    print(f"  {_c('2', '■')} {label} stopped")
            if run["status"] == "waiting":
                att = next((a for a in reversed(run["attempts"]) if (a.get("review") or {}).get("status") == "pending"), None)
                if att and args.yes:
                    runner.review(rid, "approve", "")
                elif att and interactive:
                    _ask_review(runner, rid, att)
                elif att:
                    runner.review(rid, "stop", "")
                    print(_c("35", f"👤 “{att['name']}” needs your review. Run it in a terminal you can type in, "
                                   "add --yes to approve reviews automatically, or use the app."), file=sys.stderr)
                    _wait(run)
                    return NEEDS_REVIEW
            if run["status"] not in ("running", "waiting"):
                break
            time.sleep(0.4)
    except KeyboardInterrupt:
        print(_c("31", "\n■ Stopping…"))
        try:
            runner.stop(rid)
        except ValueError:
            pass
        _wait(run)
    status = run["status"]
    if args.json:
        print(json.dumps({k: run.get(k) for k in ("id", "name", "status", "started", "ended", "cost", "error")} |
                         {"steps": [{k: a.get(k) for k in ("name", "status", "attempt", "cost", "error", "output", "judge", "usage")} for a in run["attempts"]]},
                         indent=2))
    else:
        mark = {"succeeded": _c("32;1", "✓ Finished"), "failed": _c("31;1", "✗ Failed"), "stopped": _c("2;1", "■ Stopped")}.get(status, status)
        took = _dur((run.get("ended") or time.time()) - run["started"]) + " · $%.2f" % run["cost"]
        print(f"{mark}  {_c('2', took)}" + ("\n  " + run["error"] if run.get("error") else ""))
        last = next((a for a in reversed(run["attempts"]) if a["status"] == "ok" and a.get("output")), None)
        if last and status == "succeeded" and not args.quiet:
            print(_c("2", f"── result of “{last['name']}” ──"))
            print(last["output"].strip()[-3000:])
        print(_c("2", "Details: open the app (agent-graph) → Runs."))
    return EXIT.get(status, 1)


def cmd_validate(args):
    """Check workflows without running them: errors stop a workflow from loading; warnings are likely mistakes."""
    items = []  # (label, raw, error)
    if not args.targets:  # everything the app knows, including files it couldn't load
        errors = []
        raw_all, load_errors = workflows.wfstore.load_all()
        for raw in raw_all:
            items.append((raw["file"], raw, None))
        for e in load_errors:
            items.append((e.get("file") or e["name"], None, e["error"]))
        if not items:
            print("No workflows found. Pass a file: agent-graph validate path/to/workflow.yaml")
            return 0
    for t in args.targets:
        if _is_file(t):
            try:
                raw, path = workflows.read_file(t, args.folder)
                items.append((path, raw, None))
            except ValueError as exc:
                items.append((os.path.abspath(t), None, str(exc)))
        else:
            wf = _find(t)
            raw, path = workflows.read_file(wf["file"], args.folder)
            items.append((path, raw, None))
    report, bad = [], 0
    for label, raw, err in items:
        entry = {"file": label, "ok": False, "errors": [], "warnings": []}
        if err is None:
            try:
                wf = workflows.validate(raw)
                entry.update(name=wf["name"], steps=len(wf["steps"]), cwd=wf["cwd"], warnings=workflows.lint(raw, wf), ok=True)
            except (ValueError, TypeError) as exc:
                err = str(exc)
        if err is not None:
            entry["errors"].append(err)
            entry["name"] = (raw or {}).get("name") if isinstance(raw, dict) else None
        if args.strict and entry["warnings"]:
            entry["ok"] = False
        bad += not entry["ok"]
        report.append(entry)
    if args.json:
        print(json.dumps(report, indent=2))
        return 1 if bad else 0
    for e in report:
        where = workflows._tilde(e["file"])
        if e["errors"]:
            print(f"{_c('31;1', '✗')} {where}")
            for m in e["errors"]:
                print(f"    {_c('31', 'error:')} {m}")
        else:
            mark = _c("33;1", "⚠") if e["warnings"] else _c("32;1", "✓")
            print(f"{mark} {e['name']}  {_c('2', where + ' · %d step%s · runs in %s' % (e['steps'], '' if e['steps'] == 1 else 's', workflows._tilde(e['cwd'])))}")
        for m in e["warnings"]:
            print(f"    {_c('33', 'warning:')} {m}")
    ok = sum(1 for e in report if e["ok"])
    summary = f"{ok} of {len(report)} valid" + (f" ({sum(len(e['warnings']) for e in report)} warning(s))" if any(e["warnings"] for e in report) else "")
    print(_c("1", summary) + (_c("2", "  · --strict treats warnings as errors") if not args.strict and any(e["warnings"] for e in report) else ""))
    return 1 if bad else 0


def _wait(run, timeout=60):
    end = time.time() + timeout
    while run["status"] in ("running", "waiting") and time.time() < end:
        time.sleep(0.3)


def main(argv):
    ap = argparse.ArgumentParser(prog="agent-graph", description="Run Claude Agent Graph workflows from the terminal.")
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("list", help="list your workflows")
    p.add_argument("--json", action="store_true", help="print JSON")
    p = sub.add_parser("run", help="run a workflow (by name, or a workflow .yaml file) and follow it here")
    p.add_argument("workflow", help="a workflow file (path/to/flow.yaml), or the id or name (or its start) of a workflow the app knows")
    p.add_argument("-C", "--folder", help="run in this project folder instead of the one in the workflow")
    p.add_argument("-y", "--yes", action="store_true", help="approve review points automatically")
    p.add_argument("-q", "--quiet", action="store_true", help="don't print the final result")
    p.add_argument("--json", action="store_true", help="print a JSON summary at the end instead of progress")
    p = sub.add_parser("validate", help="check workflows for mistakes without running them")
    p.add_argument("targets", nargs="*", help="workflow files or names (default: every workflow the app knows)")
    p.add_argument("-C", "--folder", help="check as if run in this folder (matters for a file without cwd)")
    p.add_argument("--strict", action="store_true", help="treat warnings as errors (exit code 1)")
    p.add_argument("--json", action="store_true", help="print JSON")
    p = sub.add_parser("runs", help="list recent runs")
    p.add_argument("-n", "--limit", type=int, default=15, help="how many (default 15)")
    p.add_argument("--json", action="store_true", help="print JSON")
    args = ap.parse_args(argv)
    return {"list": cmd_list, "run": cmd_run, "runs": cmd_runs, "validate": cmd_validate}[args.cmd](args)
