"""Workflow storage + runner, and the permissions editor backend.

A workflow is a list of steps run as headless `claude -p` sessions. Each step
has a prompt, a retry count, an optional shell check, and directions for where
to go on success / failure. Runs are drawn into the live graph.
"""

import glob
import json
import os
import queue
import re
import shutil
import subprocess
import threading
import time
import uuid

from . import compat, providers, quality, wfstore
from .watcher import FILE_TOOLS, parse_ts

CONFIG_DIR = os.path.expanduser("~/.config/claude-agent-graph")
RUN_DIR = os.path.join(CONFIG_DIR, "runs")
SETTINGS = os.path.expanduser("~/.claude/settings.json")
PERMISSION_MODES = ["acceptEdits", "auto", "dontAsk", "plan", "manual", "bypassPermissions"]
AGENT_NAME = re.compile(r"^[a-z][a-z0-9-]{0,39}$")
MAX_HOPS = 50  # guards against workflows whose directions loop forever
CHECK_TIMEOUT = 600
BASH_TIMEOUT = 1800  # default limit for shell steps (seconds)


def _write_json(path, data):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    tmp = f"{path}.{uuid.uuid4().hex[:8]}.tmp"
    with open(tmp, "w") as f:
        json.dump(data, f, indent=2)
        f.write("\n")
    os.replace(tmp, path)


def _slug(name):
    return re.sub(r"[^a-z0-9]+", "-", name.lower()).strip("-")[:40] or "workflow"


# ---- permissions (~/.claude/settings.json) --------------------------------

def read_permissions():
    try:
        with open(SETTINGS) as f:
            settings = json.load(f)
    except FileNotFoundError:
        settings = {}
    perms = settings.get("permissions") or {}
    return {
        "path": SETTINGS,
        "modes": PERMISSION_MODES,
        "defaultMode": perms.get("defaultMode", ""),
        "allow": perms.get("allow", []),
        "ask": perms.get("ask", []),
        "deny": perms.get("deny", []),
        "additionalDirectories": perms.get("additionalDirectories", []),
    }


def write_permissions(p):
    with open(SETTINGS) as f:  # re-read so edits made elsewhere aren't lost
        settings = json.load(f)
    shutil.copy2(SETTINGS, SETTINGS + ".agent-graph.bak")
    perms = dict(settings.get("permissions") or {})
    for key in ("allow", "ask", "deny", "additionalDirectories"):
        rules = [str(r).strip() for r in p.get(key, []) if str(r).strip()]
        if rules:
            perms[key] = list(dict.fromkeys(rules))
        else:
            perms.pop(key, None)
    mode = p.get("defaultMode") or ""
    if mode:
        if mode not in PERMISSION_MODES:
            raise ValueError(f"unknown mode {mode}")
        perms["defaultMode"] = mode
    else:
        perms.pop("defaultMode", None)
    if perms:
        settings["permissions"] = perms
    else:
        settings.pop("permissions", None)
    _write_json(SETTINGS, settings)
    return read_permissions()


# ---- workflow storage -----------------------------------------------------

def list_workflows(errors=None):
    raw, errs = wfstore.load_all()
    out = []
    for wf in raw:
        try:
            out.append(validate(wf))
        except (ValueError, TypeError) as exc:
            errs.append({"file": wf["file"], "name": os.path.basename(wf["file"]), "error": str(exc)})
    if errors is not None:
        errors.extend(errs)
    return out


# ---- workflow files given on the command line ------------------------------------------------------------
WF_KEYS = {"id", "name", "cwd", "model", "permissionMode", "allowedTools", "disallowedTools", "passOutput",
           "maxBudgetUsd", "judgeModel", "agents", "steps", "updated", "file"}
STEP_KEYS = {"id", "name", "kind", "run", "timeout", "prompt", "retries", "check", "model", "dependsOn",
             "onSuccess", "onFailure", "loopBack", "agents", "review", "judge"}
AGENT_KEYS = {"name", "description", "prompt", "tools", "model"}


def read_file(path, folder=None):
    """A workflow from any YAML file, not only the ones the app knows. Returns (raw dict, path).
    cwd: `folder` if given; else the file's cwd, relative to the file; else its project (…/.claude/workflows/x.yaml)
    or the file's own folder."""
    path = os.path.abspath(os.path.expanduser(path))
    if not os.path.isfile(path):
        raise ValueError(f"No such file: {path}")
    stem = os.path.splitext(os.path.basename(path))[0]
    wid = stem if wfstore.ID_RE.match(stem) else wfstore._slug(stem)
    try:
        with open(path) as f:
            doc = wfstore._yaml().load(f)
    except wfstore.MarkedYAMLError as exc:
        mark = exc.problem_mark
        raise ValueError(f"line {mark.line + 1}, column {mark.column + 1}: {exc.problem}" if mark else str(exc))
    except UnicodeDecodeError:
        raise ValueError("The file isn't text (expected a YAML workflow).")
    wf = wfstore.from_doc(doc, wid)
    here = os.path.dirname(path)
    if folder:
        wf["cwd"] = os.path.abspath(os.path.expanduser(folder))
    elif wf.get("cwd"):
        cwd = os.path.expanduser(str(wf["cwd"]))
        wf["cwd"] = cwd if os.path.isabs(cwd) else os.path.normpath(os.path.join(here, cwd))
    else:
        parts = here.split(os.sep)
        wf["cwd"] = os.sep.join(parts[:-2]) if parts[-2:] == [".claude", "workflows"] else here
    wf["file"], wf["updated"] = path, os.path.getmtime(path)
    return wf, path


def load_file(path, folder=None):
    """A validated, ready-to-run workflow from a YAML file. Raises ValueError with a readable message."""
    wf, _ = read_file(path, folder)
    return validate(wf)


def lint(raw, wf):
    """Things that don't stop a workflow from loading but are probably mistakes. raw: as written; wf: validated."""
    import difflib
    warn = []

    def unknown(keys, allowed, where):
        for k in keys:
            if k not in allowed:
                guess = difflib.get_close_matches(k, allowed, n=1)
                warn.append(f"{where}: unknown setting “{k}”" + (f" (did you mean “{guess[0]}”?)" if guess else " (it's ignored)"))

    unknown(raw.keys(), WF_KEYS, "workflow")
    for a in raw.get("agents") or []:
        if isinstance(a, dict):
            unknown(a.keys(), AGENT_KEYS, f"helper “{a.get('name', '?')}”")
    for i, s in enumerate(raw.get("steps") or []):
        if isinstance(s, dict):
            unknown(s.keys(), STEP_KEYS, f"step {i + 1} (“{s.get('name', '?')}”)")
    models = {("workflow", wf["model"])} | {(f"step “{s['name']}”", s["model"]) for s in wf["steps"] if s["model"]}
    for where, model in sorted(models):
        prov, name = providers.split(model)
        if prov == "claude":
            if name and name not in providers.CLAUDE_MODELS and not name.startswith("claude-"):
                warn.append(f"{where}: “{name}” isn't a Claude model name I know (haiku, sonnet, opus, fable, or a full claude-… id)")
            continue
        try:
            providers.check_ready(prov, name)
        except ValueError as exc:
            warn.append(f"{where}: model “{model}” can't run on this computer yet: {exc}")
    for s in wf["steps"]:
        hole = re.search(r"<describe [^<>\n]*>|\{task\}", s["prompt"])
        if hole:
            warn.append(f"step “{s['name']}”: still contains “{hole.group(0)}”, a part meant to be filled in")
    used = {n for s in wf["steps"] for n in s["agents"]}
    for a in wf["agents"]:
        if a["name"] not in used:
            warn.append(f"helper “{a['name']}” is defined but no step uses it")
    return warn


# ---- IDEs ------------------------------------------------------------------

# (command, name, style): "vscode" takes `folder -g file`, the rest take `folder file` or just a path
IDES = [("code", "VS Code", "vscode"), ("antigravity-ide-snap.antigravity-ide", "Antigravity", "vscode"),
        ("antigravity", "Antigravity", "vscode"), ("cursor", "Cursor", "vscode"), ("windsurf", "Windsurf", "vscode"),
        ("codium", "VSCodium", "vscode"), ("rider", "Rider", "jetbrains"), ("pycharm", "PyCharm", "jetbrains"),
        ("idea", "IntelliJ IDEA", "jetbrains"), ("webstorm", "WebStorm", "jetbrains"),
        ("android-studio", "Android Studio", "jetbrains"), ("zed", "Zed", "plain"), ("subl", "Sublime Text", "plain")]
APP_SETTINGS = os.path.join(CONFIG_DIR, "settings.json")


def _app_settings():
    try:
        with open(APP_SETTINGS) as f:
            return json.load(f)
    except (OSError, ValueError):
        return {}


def list_ides():
    seen, out = set(), []
    for cmd, name, style in IDES:
        if name not in seen and shutil.which(cmd):
            seen.add(name)
            out.append({"id": cmd, "name": name})
    chosen = _app_settings().get("ide")
    default = chosen if any(i["id"] == chosen for i in out) else (out[0]["id"] if out else None)
    return {"ides": out, "default": default}


def set_ide(ide):
    if not any(i["id"] == ide for i in list_ides()["ides"]):
        raise ValueError("That IDE isn't installed.")
    s = _app_settings()
    s["ide"] = ide
    _write_json(APP_SETTINGS, s)
    return list_ides()


def open_in_ide(path=None, project=None):
    """Open a file (inside its project folder when given) or a folder in the chosen IDE."""
    info = list_ides()
    if not info["default"]:
        raise ValueError("No IDE found. Install VS Code, a JetBrains IDE, Zed or Sublime Text.")
    cmd = info["default"]
    style = next(st for c, _, st in IDES if c == cmd)
    project = project if project and os.path.isdir(project) else None
    if path and not os.path.exists(path):
        raise ValueError("File no longer exists.")
    if not path:
        args = [project or os.path.expanduser("~")]
    elif style == "vscode":
        args = ([project] if project else []) + ["-g", path]
    elif style == "jetbrains":
        args = [project, path] if project and path.startswith(project + os.sep) else [path]
    else:
        args = ([project] if project else []) + [path]
    subprocess.Popen([shutil.which(cmd)] + args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, **compat.detached())
    return next(i["name"] for i in info["ides"] if i["id"] == cmd)


def workflows_state():
    errors = []
    wfs = list_workflows(errors)
    return {"workflows": wfs, "errors": errors, "projects": [_tilde(p) for p in wfstore.project_dirs()]}


def _tilde(p):
    home = os.path.expanduser("~")
    return "~" + p[len(home):] if p.startswith(home) else p


def open_workflow_file(wid=None, what=None):
    """Open a workflow's YAML (or a defaults file) in the IDE, with its project folder as the workspace."""
    if what in ("agents", "steps", "userAgents", "userSteps"):
        target = wfstore.load_defaults()["files"][what]
        if not os.path.exists(target):  # first time: start the user's file from a commented template
            os.makedirs(os.path.dirname(target), exist_ok=True)
            with open(target, "w") as f:
                f.write("# Your additions to the app's defaults (same shape as defaults/%s.yaml in the app).\n"
                        "# Entries with the same name replace the defaults.\n%s:\n" %
                        (what[4:].lower(), "categories" if what == "userAgents" else "steps"))
        project = os.path.dirname(target)
    else:
        target = wfstore._index().get(str(wid))
        if not target or not os.path.exists(target):
            raise ValueError("That workflow file doesn't exist.")
        project = os.path.dirname(os.path.dirname(os.path.dirname(target)))
    if list_ides()["default"]:
        return open_in_ide(target, project)
    compat.open_path(target)
    return target


def validate(wf):
    name = str(wf.get("name", "")).strip()
    if not name:
        raise ValueError("Workflow needs a name.")
    cwd = os.path.expanduser(str(wf.get("cwd") or "~"))
    if not os.path.isdir(cwd):
        raise ValueError(f"Folder doesn't exist: {cwd}")
    mode = wf.get("permissionMode") or "acceptEdits"
    if mode not in PERMISSION_MODES:
        raise ValueError(f"Unknown permission mode: {mode}")
    agents = []
    for a in wf.get("agents") or []:
        aname = str(a.get("name", "")).strip().lower()
        if not AGENT_NAME.match(aname):
            raise ValueError(f"Subagent name “{aname}” must be lowercase letters, digits and dashes.")
        if any(x["name"] == aname for x in agents):
            raise ValueError(f"Two subagents are called “{aname}”.")
        if not str(a.get("description") or "").strip() or not str(a.get("prompt") or "").strip():
            raise ValueError(f"Subagent “{aname}” needs a role description and instructions.")
        tools = a.get("tools") or []
        if isinstance(tools, str):
            tools = tools.split(",")
        agents.append({"name": aname, "description": str(a["description"]).strip(), "prompt": str(a["prompt"]),
                       "tools": [t.strip() for t in tools if t.strip()], "model": str(a.get("model") or "").strip()})
    agent_names = {a["name"] for a in agents}
    steps = wf.get("steps") or []
    if not steps:
        raise ValueError("Add at least one step.")
    # steps added in the builder get a readable id from their name ("new-…" is a placeholder)
    renames, new_ids = {}, []
    taken = {str(s.get("id")) for s in steps if s.get("id") and not str(s["id"]).startswith("new-")}
    for i, s in enumerate(steps):
        old = str(s.get("id") or "")
        sid = old
        if not old or old.startswith("new-"):
            base = wfstore._slug(s.get("name") or f"step-{i + 1}")
            sid, n = base, 2
            while sid in taken:
                sid, n = f"{base}-{n}", n + 1
            taken.add(sid)
            if old:
                renames[old] = sid
        new_ids.append(sid)
    ids = set()
    clean = []
    for i, s in enumerate(steps):
        sid = new_ids[i]
        if not re.match(r"^[A-Za-z0-9_-]{1,60}$", sid):
            raise ValueError(f"Step id “{sid}” should be letters, digits and dashes.")
        if sid in ids:
            raise ValueError(f"Duplicate step id {sid}")
        ids.add(sid)
        kind = "bash" if s.get("kind") == "bash" or (s.get("run") and s.get("kind") != "claude") else "claude"
        if kind == "bash" and not str(s.get("run") or "").strip():
            raise ValueError(f"Step “{s.get('name') or i + 1}” is a shell step but has no command (run).")
        if kind == "claude" and not str(s.get("prompt") or "").strip():
            raise ValueError(f"Step {i + 1} has no prompt.")
        unknown = [n for n in (s.get("agents") or []) if n not in agent_names]
        if unknown:
            raise ValueError(f"Step “{s.get('name') or i + 1}” uses subagent “{unknown[0]}”, which isn't defined under agents.")
        clean.append({
            "id": sid,
            "name": str(s.get("name") or f"Step {i + 1}").strip(),
            "kind": kind,
            "run": str(s.get("run") or "") if kind == "bash" else "",
            "timeout": max(10, min(86400, int(s.get("timeout") or BASH_TIMEOUT))),
            "prompt": str(s.get("prompt") or "") if kind == "claude" else "",
            "retries": max(0, min(10, int(s.get("retries") or 0))),
            "check": str(s.get("check") or "").strip(),
            "judge": str(s.get("judge") or "").strip(),
            "model": str(s.get("model") or "").strip(),
            "dependsOn": ([renames.get(str(d), str(d)) for d in s["dependsOn"]] if isinstance(s.get("dependsOn"), list)
                          else [renames.get(str(s["dependsOn"]), str(s["dependsOn"]))] if s.get("dependsOn")
                          else [] if "dependsOn" in s or not clean else [clean[-1]["id"]]),
            "onSuccess": renames.get(str(s.get("onSuccess")), str(s.get("onSuccess") or "next")),
            "onFailure": renames.get(str(s.get("onFailure")), str(s.get("onFailure") or "stop")),
            "loopBack": _clean_loop(s.get("loopBack"), renames),
            "agents": list(s.get("agents") or []),
            "review": bool(s.get("review")),
        })
    by_id = {s["id"]: s for s in clean}
    for s in clean:
        for d in s["dependsOn"]:
            if d not in ids:
                raise ValueError(f"Step “{s['name']}” depends on a missing step ({d}).")
            if d == s["id"]:
                raise ValueError(f"Step “{s['name']}” can't depend on itself.")
        # older files: onFailure/onSuccess pointing at a step id were jumps; express them as loop-backs
        if s["onFailure"] == "next":
            s["onFailure"] = "continue"
        if s["onFailure"] not in ("stop", "continue"):
            if s["onFailure"] not in ids:
                raise ValueError(f"Step “{s['name']}” points to a missing step ({s['onFailure']}).")
            s["loopBack"] = s["loopBack"] or {"to": s["onFailure"], "when": "failure", "max": 3}
            s["onFailure"] = "stop"
        if s["onSuccess"] not in ("next", "end"):
            if s["onSuccess"] not in ids:
                raise ValueError(f"Step “{s['name']}” points to a missing step ({s['onSuccess']}).")
            s["loopBack"] = s["loopBack"] or {"to": s["onSuccess"], "when": "success", "max": 3}
            s["onSuccess"] = "next"
    order = _topo_order(clean)  # raises on cycles
    for s in clean:
        lb = s["loopBack"]
        if lb:
            if lb["to"] not in ids:
                raise ValueError(f"Step “{s['name']}” loops back to a missing step ({lb['to']}).")
            if lb["to"] != s["id"] and lb["to"] not in _ancestors(by_id, s["id"]):
                raise ValueError(f"Step “{s['name']}” can only loop back to itself or a step it depends on "
                                 f"(directly or indirectly), not “{by_id[lb['to']]['name']}”.")
    del order
    budget = wf.get("maxBudgetUsd")
    return {
        "id": wf.get("id") or wfstore.new_id(name),
        "name": name,
        "cwd": cwd,
        "model": str(wf.get("model") or "").strip(),
        "permissionMode": mode,
        "allowedTools": [t.strip() for t in wf.get("allowedTools", []) if t.strip()],
        "disallowedTools": [t.strip() for t in wf.get("disallowedTools", []) if t.strip()],
        "passOutput": bool(wf.get("passOutput", True)),
        "maxBudgetUsd": float(budget) if budget not in (None, "") else None,
        "judgeModel": str(wf.get("judgeModel") or "").strip(),
        "agents": agents,
        "steps": clean,
        "updated": wf.get("updated") or time.time(),
        "file": wf.get("file") or "",
    }


def _clean_loop(lb, renames):
    if not lb:
        return None
    if isinstance(lb, str):
        lb = {"to": lb}
    if not isinstance(lb, dict) or not lb.get("to"):
        raise ValueError("loopBack needs “to: <step id>”.")
    when = str(lb.get("when") or "failure")
    if when not in ("failure", "success"):
        raise ValueError("loopBack “when” must be failure or success.")
    return {"to": renames.get(str(lb["to"]), str(lb["to"])), "when": when,
            "max": max(1, min(20, int(lb.get("max") or 3)))}


def _copy(run):
    """A deep copy of a run; parallel step threads may be updating it, so retry if it changes mid-copy."""
    for _ in range(20):
        try:
            return json.loads(json.dumps(run))
        except RuntimeError:
            time.sleep(0.005)
    return json.loads(json.dumps(run))


def _descendants(steps, sid):
    """Every step that depends on sid, directly or indirectly."""
    out, changed = set(), True
    while changed:
        changed = False
        for s in steps:
            if s["id"] not in out and any(d == sid or d in out for d in s["dependsOn"]):
                out.add(s["id"])
                changed = True
    return out


def _ancestors(by_id, sid):
    seen, todo = set(), list(by_id[sid]["dependsOn"])
    while todo:
        d = todo.pop()
        if d not in seen:
            seen.add(d)
            todo += by_id[d]["dependsOn"]
    return seen


def _topo_order(steps):
    """Steps in an order where dependencies come first; raises if the dependencies form a cycle."""
    by_id = {s["id"]: s for s in steps}
    state, order = {}, []

    def visit(sid, path):
        if state.get(sid) == "done":
            return
        if state.get(sid) == "visiting":
            names = " → ".join(by_id[x]["name"] for x in path[path.index(sid):] + [sid])
            raise ValueError(f"The step dependencies form a cycle: {names}. Use loopBack to repeat steps.")
        state[sid] = "visiting"
        for d in by_id[sid]["dependsOn"]:
            visit(d, path + [sid])
        state[sid] = "done"
        order.append(sid)

    for s in steps:
        visit(s["id"], [])
    return order


def save_workflow(wf):
    base = wf.get("updated") if wf.get("id") else None
    wf = dict(wf, id=wf.get("id") or wfstore.new_id(str(wf.get("name") or "workflow")))
    wf = validate(wf)
    wfstore.write(wf, base)
    return next(w for w in list_workflows() if w["id"] == wf["id"])


def delete_workflow(wid):
    wfstore.remove(wid)


# ---- runner ---------------------------------------------------------------

EDITABLE = (".md", ".markdown", ".txt")

REWRITE_SYSTEM = (
    "You improve text used to instruct AI coding agents in a workflow. Rewrite the user's text so it is clear, "
    "specific and actionable: state the goal, the concrete deliverable (files, format), and constraints. Keep the "
    "author's intent, language, tone, and every path, command, name and number exactly. Do not invent requirements "
    "or facts. Keep it about as long as needed, no longer. NEVER ask questions or ask for details: always return a "
    "rewrite. Where something essential is missing, insert a short <placeholder in angle brackets> for the author to "
    "fill in. Output ONLY the rewritten text: no preamble, no explanation, no quotes, no markdown fences.")
REWRITE_KIND = {
    "step": "This is the task prompt for one step of a workflow.",
    "role": "This is a subagent's role: ONE short sentence saying what it does and when to use it.",
    "agent": "These are a subagent's standing instructions (its procedure). Numbered steps work well.",
}


def rewrite_text(text, kind="step", context=""):
    """Polish a prompt with the cheapest model; nothing is saved as a session."""
    text = str(text or "").strip()
    if not text:
        raise ValueError("Write something first, then improve it.")
    brief = REWRITE_KIND.get(kind, REWRITE_KIND["step"])
    prompt = (f"{brief}\n{('Context: ' + context) if context else ''}\n\nRewrite the text between the markers. "
              f"Reply with the rewritten text only.\n<<<\n{text}\n>>>")
    cmd = [compat.claude_cmd(), "-p", "--model", "haiku", "--output-format", "json", "--no-session-persistence",
           "--tools", "", "--system-prompt", REWRITE_SYSTEM, "--max-budget-usd", "0.10"]
    try:
        p = subprocess.run(cmd, input=prompt, capture_output=True, text=True, timeout=120,
                           cwd=os.path.expanduser("~"))
        res = json.loads(p.stdout.strip().splitlines()[-1])
    except (subprocess.TimeoutExpired, ValueError, IndexError):
        raise ValueError("The rewrite didn't come back. Try again.")
    if res.get("is_error") or not res.get("result"):
        raise ValueError("Rewrite failed: " + str(res.get("result") or res.get("subtype"))[:200])
    return {"text": str(res["result"]).strip(), "cost": res.get("total_cost_usd")}


def _open_in_browser(path):
    compat.open_in_browser(path)


def _desktop_notify(title, body):
    compat.notify(title, body)


def _clip(text, n):
    text = str(text or "").strip()
    return text if len(text) <= n else text[:n] + f"\n… ({len(text) - n} more characters)"


def _log_lines(d):
    """Turn one transcript record into log entries: text, tool calls, tool results."""
    if d.get("type") not in ("user", "assistant"):
        return []
    t = parse_ts(d.get("timestamp")) or 0
    content = (d.get("message") or {}).get("content")
    if isinstance(content, str):
        content = [{"type": "text", "text": content}]
    out = []
    for b in content if isinstance(content, list) else []:
        kind = b.get("type")
        if d["type"] == "assistant" and kind == "text" and b.get("text", "").strip():
            out.append({"t": t, "kind": "text", "title": _clip(b["text"], 160).split("\n")[0], "body": _clip(b["text"], 4000)})
        elif kind == "tool_use":
            inp = b.get("input") or {}
            name = b.get("name", "")
            key = next((k for k in ("command", "file_path", "notebook_path", "pattern", "query", "url", "description", "prompt")
                        if k in inp), None)
            title = f"{name}: {_clip(inp[key], 140).splitlines()[0]}" if key else name
            body = inp.get("content") or inp.get("new_string") or inp.get("command") or inp.get("prompt") \
                or json.dumps(inp, indent=1)
            out.append({"t": t, "kind": "tool", "title": title, "body": _clip(body, 3000)})
        elif kind == "tool_result":
            c = b.get("content")
            if isinstance(c, list):
                c = "\n".join(x.get("text", "") for x in c if isinstance(x, dict))
            out.append({"t": t, "kind": "error" if b.get("is_error") else "result",
                        "title": _clip(c, 140).split("\n")[0] or "(no output)", "body": _clip(c, 3000)})
        elif d["type"] == "user" and kind == "text" and b.get("text", "").strip() and not d.get("isMeta"):
            out.append({"t": t, "kind": "input", "title": _clip(b["text"], 160).split("\n")[0], "body": _clip(b["text"], 3000)})
    return out


def _usage(res):
    """Token use of one claude -p call, and how much of its input came from the prompt cache."""
    u = res.get("usage") or {}
    fresh, read, write = (int(u.get(k) or 0) for k in ("input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"))
    total = fresh + read + write
    return {"input": total, "output": int(u.get("output_tokens") or 0), "cacheRead": read, "cacheWrite": write,
            "cacheHit": round(read / total, 3) if total else None, "turns": res.get("num_turns"),
            "apiMs": res.get("duration_api_ms")}


def _written_files(session):
    """Files a step session (and its subagents) wrote or edited, read straight from its transcripts."""
    root = os.path.join(os.path.expanduser("~/.claude/projects"), "*")
    paths = glob.glob(os.path.join(root, session + ".jsonl")) + glob.glob(os.path.join(root, session, "subagents", "*.jsonl"))
    out = []
    for path in paths:
        try:
            with open(path, errors="replace") as f:
                for line in f:
                    if '"tool_use"' not in line:
                        continue
                    for b in (json.loads(line).get("message") or {}).get("content") or []:
                        if isinstance(b, dict) and b.get("type") == "tool_use" and b.get("name") in FILE_TOOLS:
                            p = (b.get("input") or {}).get("file_path") or (b.get("input") or {}).get("notebook_path")
                            if p and p not in out:
                                out.append(p)
        except (OSError, ValueError):
            continue
    return out


class Runner:
    def __init__(self, graph, notify):
        self.graph = graph
        self.notify = notify  # wakes the SSE streams
        self.runs = {}
        self.procs = {}
        self.proc_of = {}  # session -> its claude process, so one step can be interrupted to steer it
        self.waits = {}  # run id -> Event a paused run waits on for the user's review
        self.review_locks = {}  # run id -> Lock: parallel steps take turns asking for review
        self.log_cache = {}
        self.lock = threading.Lock()
        self.own = set()  # runs this process is running (the others are history, or live in another process)
        self._disk = {}   # run id -> file time last read
        self.usage = {}   # session -> token usage reported by claude -p
        self.sync_disk()

    def sync_disk(self):
        """Load saved runs, and follow runs that another process is running (`agent-graph run` in a terminal,
        or the app while the terminal runs). Returns True when something changed."""
        try:
            names = sorted(n for n in os.listdir(RUN_DIR) if n.endswith(".json"))[-30:]
        except OSError:
            return False
        changed = False
        for name in names:
            rid, path = name[:-5], os.path.join(RUN_DIR, name)
            if rid in self.own:
                continue
            try:
                mtime = os.path.getmtime(path)
                if self._disk.get(rid) == mtime:
                    continue
                with open(path) as f:
                    run = json.load(f)
                self._disk[rid] = mtime
                pid = run.get("pid")
                if run["status"] in ("running", "waiting"):
                    if pid and pid != os.getpid() and compat.pid_alive(pid):
                        run["external"] = True  # live in a terminal: follow it here, control it there
                    else:
                        run["status"] = "stopped"  # its process was closed mid-run
                        run.pop("external", None)
                with self.lock:
                    self.runs[run["id"]] = run
                self._graph_node(run)
                self._relink(run)
                changed = True
            except (OSError, ValueError, KeyError):
                continue
        return changed

    def _external(self, run, what):
        if run.get("external") and run["status"] in ("running", "waiting"):
            raise ValueError(f"This run is running in a terminal (agent-graph run). {what}")

    def summary(self):
        with self.lock:
            return sorted((_copy(r) for r in self.runs.values()), key=lambda r: r["started"], reverse=True)[:30]

    def start(self, wid, wf=None):
        """wf: an already-validated workflow (e.g. from a file given on the command line)."""
        if wf is None:
            wf = next((w for w in list_workflows() if w["id"] == wid), None)
            if not wf:
                raise ValueError("Workflow not found.")
        wf = validate(wf)
        wid = wf["id"]
        rid = time.strftime("%Y%m%d-%H%M%S-") + uuid.uuid4().hex[:4]
        run = {"id": rid, "workflowId": wid, "name": wf["name"], "status": "running", "started": time.time(),
               "ended": None, "current": None, "cost": 0.0, "attempts": [], "stop": False, "cwd": wf["cwd"],
               "plan": self._plan(wf), "pid": os.getpid(), "file": wf.get("file", "")}
        with self.lock:
            self.runs[rid] = run
            self.own.add(rid)
        self._graph_node(run)
        with self.graph.lock:
            self.graph.edge("you", "w:" + rid, "prompt", time.time(), f"Started workflow “{wf['name']}”")
            self.graph.version += 1
        threading.Thread(target=self._run, args=(wf, run), daemon=True).start()
        self._changed(run)
        return rid

    @staticmethod
    def _plan(wf):
        return [{"id": s["id"], "name": s["name"], "agents": s["agents"],
                 "retries": s["retries"], "check": s["check"], "judge": s.get("judge", ""), "review": s["review"],
                 "dependsOn": s["dependsOn"],
                 "loopBack": s["loopBack"], "onSuccess": s["onSuccess"], "onFailure": s["onFailure"],
                 "kind": s.get("kind", "claude"), "run": s.get("run", "")}
                for s in wf["steps"]]

    def replay(self, rid, step_id, only=False, resume=None):
        """Run one step again (or from it to the end) inside the same run, with the current saved workflow.
        resume: (session, message) to continue that step's conversation with the user's steering instead."""
        wf = None
        with self.lock:
            run = self.runs.get(rid)
            if not run:
                raise ValueError("Run not found.")
            if run["status"] in ("running", "waiting"):
                raise ValueError("This run is still going. Wait for it to finish, or stop it, before replaying a step.")
            wf = next((w for w in list_workflows() if w["id"] == run["workflowId"]), None)
            if not wf and run.get("file") and os.path.isfile(run["file"]):  # a workflow file run from the command line
                wf, _ = read_file(run["file"], run.get("cwd"))
            if not wf:
                raise ValueError("The workflow was deleted, so its steps can't be replayed.")
            wf = validate(wf)
            index = {s["id"]: i for i, s in enumerate(wf["steps"])}
            if step_id not in index:
                raise ValueError("This step is no longer in the workflow.")
            # Steps that aren't replayed keep their last good result, so the replayed ones get the same input.
            by = {x["id"]: x for x in wf["steps"]}
            redo = {step_id} if only else {step_id} | _descendants(wf["steps"], step_id)
            seed = {}
            for a in run["attempts"]:
                if a["status"] == "ok" and a["step"] in by and a["step"] not in redo:
                    rev = a.get("review") or {}
                    seed[a["step"]] = {"ok": True, "out": a.get("output", ""), "session": a["session"], "reason": "",
                                       "notes": rev.get("feedback", "") if rev.get("status") == "approved" else ""}
            run["replay"] = run.get("replay", 0) + 1
            run.update(status="running", ended=None, stop=False, error="", plan=self._plan(wf), pid=os.getpid())
            run.pop("external", None)
            self.own.add(rid)
            run["_steer"] = {}
            run["_resume"] = {step_id: resume} if resume else {}
        with self.graph.lock:
            self.graph.edge("you", "w:" + rid, "prompt", time.time(),
                            ("Steered" if resume else "Replayed") + f" “{wf['steps'][index[step_id]]['name']}”" + ("" if only else " to the end"))
            self.graph.version += 1
        threading.Thread(target=self._run, args=(wf, run), kwargs=dict(todo=redo, seed=seed), daemon=True).start()
        self._changed(run)
        return rid

    def detail(self, rid):
        with self.lock:
            run = self.runs.get(rid)
            if not run:
                raise ValueError("Run not found.")
            run = _copy(run)
        wf = next((w for w in list_workflows() if w["id"] == run["workflowId"]), None)
        run.setdefault("cwd", (wf or {}).get("cwd"))
        if "plan" not in run:  # runs from before plans were recorded
            run["plan"] = [{"id": s["id"], "name": s["name"], "agents": s.get("agents", []),
                            "retries": s["retries"], "check": s["check"]}
                           for s in (wf or {}).get("steps", [])]
        written = {}
        for a in run["attempts"]:
            a["files"], a["agents"] = self.graph.session_detail(a["session"])
            a["reads"] = self.graph.session_reads(a["session"])
            for r in a["reads"]:  # which earlier step produced this input
                if r["path"] in written:
                    r["from"] = written[r["path"]]
            for f in a["files"]:
                written.setdefault(f["path"], a["name"])
        return run

    def _artefact(self, rid, path):
        """Only files a run itself wrote or read may be read or opened."""
        run = self.detail(rid)
        path = str(path)
        if not any(f["path"] == path for a in run["attempts"] for f in a["files"] + a["reads"]):
            raise ValueError("That file isn't an input or artefact of this run.")
        return path

    def read_artefact(self, rid, path, limit=300_000):
        path = self._artefact(rid, path)
        if not os.path.isfile(path):
            return {"path": path, "exists": False}
        size = os.path.getsize(path)
        with open(path, "rb") as f:
            data = f.read(limit)
        if b"\0" in data[:8000]:
            return {"path": path, "exists": True, "size": size, "binary": True}
        return {"path": path, "exists": True, "size": size, "truncated": size > limit,
                "text": data.decode("utf-8", errors="replace")}

    def open_artefact(self, rid, path, folder=False):
        path = self._artefact(rid, path)
        target = os.path.dirname(path) if folder else path
        if not os.path.exists(target):
            raise ValueError("File no longer exists.")
        if not folder and path.lower().endswith((".html", ".htm")):
            return _open_in_browser(path)
        compat.open_path(target)

    def open_ide(self, rid, path=None):
        """Open the run's project folder in the IDE, or one of its files inside that folder."""
        run = self.detail(rid)
        return open_in_ide(self._artefact(rid, path) if path else None, run.get("cwd"))

    def write_artefact(self, rid, path, text):
        """Save edits to a text artefact (Markdown / plain text only)."""
        path = self._artefact(rid, path)
        if not path.lower().endswith(EDITABLE):
            raise ValueError("Only Markdown and text artefacts can be edited here.")
        if len(text) > 2_000_000:
            raise ValueError("That's too large to save from here.")
        tmp = f"{path}.{uuid.uuid4().hex[:8]}.tmp"
        with open(tmp, "w") as f:
            f.write(text)
        os.replace(tmp, path)
        return self.read_artefact(rid, path)

    def log(self, rid, session, limit=400):
        """Recent transcript activity of one step attempt and its subagents, oldest first."""
        run = self.detail(rid)
        att = next((a for a in run["attempts"] if a["session"] == session), None)
        if not att:
            raise ValueError("That session isn't part of this run.")
        if att.get("kind") == "bash":
            return self._bash_log(att)
        entries = []
        for path, who in self.graph.session_paths(session):
            entries += [dict(e, who=who) for e in self._log_entries(path)]
        entries.sort(key=lambda e: e["t"])
        return {"entries": entries[-limit:], "total": len(entries)}

    def session_log(self, sid, limit=600):
        """Recent transcript activity of any session (not just workflow steps) and its helpers, oldest first."""
        paths = self.graph.session_paths(sid)
        if not paths:
            raise ValueError("No log found for that session. It may have been moved or deleted.")
        entries = []
        for path, who in paths:
            entries += [dict(e, who=who) for e in self._log_entries(path)]
        entries.sort(key=lambda e: e["t"])
        return {"entries": entries[-limit:], "total": len(entries)}

    @staticmethod
    def _bash_log(att):
        try:
            with open(att["log"], errors="replace") as f:
                text = f.read()
        except OSError:
            text = ""
        lines = text.splitlines()
        entries = [{"t": att["started"], "kind": "input", "who": "step", "title": "$ " + att["input"].splitlines()[0][2:],
                    "body": att["input"]}]
        if lines:  # one entry per chunk of output so the log stays readable
            for i in range(0, len(lines), 40):
                chunk = lines[i:i + 40]
                entries.append({"t": att["started"], "kind": "text", "who": "step", "title": chunk[-1][:200] or chunk[0][:200],
                                "body": "\n".join(chunk)})
        if att["status"] in ("failed", "stopped") and att.get("error"):
            entries.append({"t": att.get("ended") or att["started"], "kind": "error", "who": "step",
                            "title": att["error"].splitlines()[0][:200], "body": att["error"]})
        return {"entries": entries, "total": len(lines)}

    def _log_entries(self, path):
        """Parse a transcript incrementally (only bytes appended since last time)."""
        cache = self.log_cache.setdefault(path, {"offset": 0, "entries": []})
        try:
            size = os.path.getsize(path)
        except OSError:
            return cache["entries"]
        if size > cache["offset"]:
            with open(path, "rb") as f:
                f.seek(cache["offset"])
                data = f.read(size - cache["offset"])
            end = data.rfind(b"\n")
            if end >= 0:
                cache["offset"] += end + 1
                for raw in data[: end + 1].splitlines():
                    try:
                        cache["entries"] += _log_lines(json.loads(raw))
                    except (ValueError, AttributeError, TypeError):
                        continue
        return cache["entries"]

    def open_folder(self, rid):
        run = self.detail(rid)
        if not os.path.isdir(run.get("cwd") or ""):
            raise ValueError("The workflow folder doesn't exist.")
        compat.open_path(run["cwd"])

    def review(self, rid, decision, feedback=""):
        """The user's verdict on a step that paused for review."""
        if decision not in ("approve", "revise", "stop"):
            raise ValueError("Unknown decision.")
        feedback = str(feedback or "").strip()
        if decision == "revise" and not feedback:
            raise ValueError("Say what should change.")
        with self.lock:
            run = self.runs.get(rid)
            wait = self.waits.get(rid)
            if run:
                self._external(run, "Answer its review there.")
            if not run or run["status"] != "waiting" or not wait:
                raise ValueError("This run isn't waiting for a review.")
            run["_decision"] = (decision, feedback)
        wait.set()

    def steer(self, rid, step_id, message, only=True):
        """Redirect a step with the user's guidance, like steering Claude Code mid-task.
        Running step: interrupt it and continue its conversation with the message (no retry is used up).
        Step paused for review: same as "request changes". Finished run: redo the step (and optionally the
        steps after it), continuing its conversation with the message."""
        message = str(message or "").strip()
        if not message:
            raise ValueError("Write what Claude should do differently.")
        with self.lock:
            run = self.runs.get(rid)
            if not run:
                raise ValueError("Run not found.")
            self._external(run, "Steer it after it finishes, or stop it and replay here.")
            att = next((a for a in reversed(run["attempts"]) if a["step"] == step_id), None)
            if att and att.get("kind") == "bash":
                raise ValueError("Command steps can't be steered. Edit the command, then replay the step.")
            status, proc = run["status"], None
            if status == "waiting":
                if not att or (att.get("review") or {}).get("status") != "pending":
                    raise ValueError("The workflow is waiting for your review of another step. Finish that review first.")
            elif status == "running":
                if not att or att["status"] != "running":
                    raise ValueError("While the workflow runs, only the step that's working now can be steered. "
                                     "Wait for the run to finish (or stop it) to redo other steps.")
                proc = self.proc_of.get(att["session"])
                if not proc or proc.poll() is not None:
                    raise ValueError("This step is just finishing. Steer it again once it's done.")
                run.setdefault("_steer", {})[step_id] = message
                att["steerPending"] = message
        if status == "waiting":
            return self.review(rid, "revise", message)
        if status == "running":
            compat.kill_tree(proc.pid)  # _run_step sees the steer and continues the conversation
            self._changed(run)
            return rid
        if not att:
            raise ValueError("This step hasn't run yet, so there's nothing to steer. Replay the run instead.")
        return self.replay(rid, step_id, only, resume=(att["session"], message))

    @staticmethod
    def _steer_prompt(message, interrupted):
        return (("The user interrupted you while you were working on this step" if interrupted else
                 "The user looked at your result for this step") + " and is steering you:\n\n" + message +
                "\n\nFollow this guidance. Everything you did earlier in this conversation, and the files you wrote, "
                "are still there: continue from them rather than starting over. When you're done, give your final "
                "result for this step.")

    def stop(self, rid):
        with self.lock:
            run = self.runs.get(rid)
            if not run or run["status"] not in ("running", "waiting"):
                raise ValueError("That run isn't running.")
            if run.get("external"):  # ask the terminal that runs it to stop, as if Ctrl+C was pressed there
                compat.interrupt(run["pid"])
                return
            run["stop"] = True
            procs = list(self.procs.get(rid, ()))
            wait = self.waits.get(rid)
        if wait:
            wait.set()
        for proc in procs:
            if proc.poll() is None:
                compat.kill_tree(proc.pid)

    # -- internals --

    def _graph_node(self, run):
        with self.graph.lock:
            n = self.graph.node("w:" + run["id"], kind="workflow")
            n.update(label=run["name"], runStatus=run["status"], lastActive=run.get("ended") or time.time(),
                     started=run["started"], cost=run["cost"])
            self.graph.version += 1

    def _changed(self, run):
        with self.lock:
            _write_json(os.path.join(RUN_DIR, run["id"] + ".json"), _copy(run))
        self._graph_node(run)
        self.notify()

    def _run(self, wf, run, todo=None, seed=None):
        """Run steps in dependency order, in parallel where they don't depend on each other, and go back to
        an earlier step when a loopBack fires. todo: the steps to run (default all); seed: earlier results."""
        steps = wf["steps"]
        by = {s["id"]: s for s in steps}
        order = _topo_order(steps)
        todo = set(todo or by)
        results = dict(seed or {})
        status = {sid: "pending" if sid in todo else "done" for sid in by}
        loops, loop_notes, stale, running = {}, {}, set(), {}
        events = queue.Queue()
        hops, final, ended_early = 0, "succeeded", False

        def worker(sid, inputs, extra, loop_n):
            step = by[sid]
            try:
                if len(inputs) == 1:
                    prev_out, label = inputs[0][1]["out"], ""
                else:
                    prev_out = "\n\n".join(f"### From “{name}”\n{r['out']}" for name, r in inputs if r.get("out"))
                    label = "Outputs from the steps this one depends on:"
                notes = "\n\n".join(r["notes"] for _, r in inputs if r.get("notes"))
                sessions = [r["session"] for _, r in inputs if r.get("session")]
                ok, out, session, reason = self._run_step(wf, run, step, prev_out, sessions, notes=notes, extra=extra,
                                                          loop=loop_n, prev_label=label)
                new_notes = ""
                if ok and step.get("review") and not run["stop"]:
                    ok, out, session, reason, new_notes = self._review_gate(wf, run, step, out, session, prev_out, sessions)
                events.put((sid, {"ok": ok, "out": out, "session": session, "reason": reason, "notes": new_notes}))
            except Exception as exc:  # a crash in one step must not hang the run
                events.put((sid, {"ok": False, "out": "", "session": None, "reason": f"internal error: {exc}", "notes": ""}))

        while True:
            if not run["stop"] and not ended_early and final == "succeeded":
                for sid in order:
                    deps = by[sid]["dependsOn"]
                    if status[sid] != "pending" or not all(status[d] in ("done", "continued") for d in deps):
                        continue
                    hops += 1
                    if hops > MAX_HOPS:
                        final = "failed"
                        run["error"] = f"Stopped after {MAX_HOPS} step runs (the loops keep going round)."
                        break
                    status[sid] = "running"
                    inputs = [(by[d]["name"], results[d]) for d in deps if d in results]
                    t = threading.Thread(target=worker, args=(sid, inputs, loop_notes.pop(sid, ""), loops.get("@" + sid, 0)),
                                         daemon=True)
                    running[sid] = t
                    t.start()
            if not running:
                break
            sid, res = events.get()
            running.pop(sid, None)
            if sid in stale:  # a loop-back reset this step while it was running: run it again later
                stale.discard(sid)
                status[sid] = "pending"
                continue
            step = by[sid]
            results[sid] = res
            if run["stop"]:
                status[sid] = "failed"
                continue
            lb = step["loopBack"]
            fire = lb and ((lb["when"] == "failure" and not res["ok"]) or (lb["when"] == "success" and res["ok"]))
            if fire and loops.get(sid, 0) < lb["max"]:
                n = loops[sid] = loops.get(sid, 0) + 1
                target = lb["to"]
                reset = {target} | _descendants(steps, target)
                for r in reset:
                    if r in running:  # still busy in parallel: rerun it once it reports back
                        stale.add(r)
                    else:
                        status[r] = "pending"
                todo |= reset
                loops["@" + target] = n
                why = (f"step “{step['name']}” failed: {res['reason']}" if not res["ok"]
                       else f"step “{step['name']}” finished and the workflow loops back for another round")
                loop_notes[target] = (f"\n\n---\nLoop-back {n} of {lb['max']}: {why}.\n"
                                      f"Its output:\n{(res['out'] or '')[-3000:]}\n\n"
                                      + ("Fix the cause so that step succeeds this time." if not res["ok"]
                                         else "Improve on the previous round."))
                run.setdefault("loops", []).append({"from": sid, "to": target, "n": n, "max": lb["max"],
                                                    "when": lb["when"], "t": time.time(), "reason": res["reason"]})
                self._loop_edge(run, step, by[target], res.get("session"), n)
                self._changed(run)
                continue
            if res["ok"]:
                status[sid] = "done"
                if step["onSuccess"] == "end":
                    ended_early = True
            elif step["onFailure"] == "continue":
                status[sid] = "continued"
            else:
                status[sid] = "failed"
                if final == "succeeded":
                    final = "failed"
                    run["error"] = f"“{step['name']}” failed: {res['reason']}" + (
                        f" (after {loops[sid]} loop-back{'s' if loops[sid] > 1 else ''})" if loops.get(sid) else "")
                    self._stop_others(run)
        if run["stop"] and final == "succeeded":
            final = "stopped"
        if final == "failed":
            run["stop"] = False
        run.pop("_steer", None)
        run.pop("_resume", None)
        run.update(status=final, ended=time.time(), current=None)
        self._changed(run)

    def _stop_others(self, run):
        """A step failed for good: end the steps still running in parallel."""
        run["stop"] = True
        with self.lock:
            procs = list(self.procs.get(run["id"], ()))
            wait = self.waits.get(run["id"])
        if wait:
            wait.set()
        for proc in procs:
            if proc.poll() is None:
                compat.kill_tree(proc.pid)

    def _loop_edge(self, run, step, target, session, n):
        with self.graph.lock:
            if session:
                self.graph.edge("s:" + session, "w:" + run["id"], "retry", time.time(),
                                f"↺ “{step['name']}” loops back to “{target['name']}” (round {n + 1})")
            self.graph.version += 1

    def _review_gate(self, wf, run, step, out, session, prev_out, prev_session):
        """Pause until the user approves; revise the step with their feedback as often as they ask."""
        revision = 0
        with self.lock:
            gate = self.review_locks.setdefault(run["id"], threading.Lock())
        with gate:
            return self._review_loop(wf, run, step, out, session, prev_out, prev_session, revision)

    def _review_loop(self, wf, run, step, out, session, prev_out, prev_session, revision):
        while True:
            rec = next(a for a in reversed(run["attempts"]) if a["session"] == session)
            rec["review"] = {"status": "pending", "t": time.time()}
            wait = threading.Event()
            with self.lock:
                self.waits[run["id"]] = wait
                run["status"] = "waiting"
            self._changed(run)
            _desktop_notify(f"Review needed: {wf['name']}", f"“{step['name']}” is done. Check its artefacts and approve or ask for changes.")
            wait.wait()
            with self.lock:
                self.waits.pop(run["id"], None)
                decision, feedback = run.pop("_decision", ("stop", ""))
                if run["stop"]:
                    decision = "stop"
                run["status"] = "running"
            if decision == "stop":
                run["stop"] = True
                rec["review"] = {"status": "stopped", "feedback": feedback, "t": time.time()}
                self._changed(run)
                return False, out, session, "stopped by you at review", ""
            if decision == "approve":
                rec["review"] = {"status": "approved", "feedback": feedback, "t": time.time()}
                self._changed(run)
                return True, out, session, "", feedback
            revision += 1
            rec["review"] = {"status": "changes", "feedback": feedback, "t": time.time()}
            extra = ("\n\n---\nYou already did this step once. Your previous result:\n" + out[-3000:] +
                     "\n\nThe user reviewed your result and the files you wrote, and asked for these changes:\n" + feedback +
                     "\n\nApply the changes. The files from your previous attempt are already on disk: edit them rather than starting over.")
            ok, out, session, reason = self._run_step(wf, run, step, prev_out, prev_session, extra=extra, revision=revision)
            if not ok:
                return False, out, session, reason, ""

    def _run_step(self, wf, run, step, prev_out, prev_session, notes="", extra="", revision=0, loop=0, prev_label=""):
        reason, out, session = "", "", None
        resume, steer, interrupted = None, "", False
        with self.lock:  # "redo with my feedback" on a finished step: continue its conversation
            pending = (run.get("_resume") or {}).pop(step["id"], None)
        if pending and step.get("kind") != "bash":
            resume, steer = pending
        attempt = 0
        while attempt < step["retries"] + 1:
            attempt += 1
            if run["stop"]:
                return False, out, session, "stopped"
            session = str(uuid.uuid4())
            if step.get("kind") == "bash":
                ok, out, reason, rec = self._run_bash(wf, run, step, prev_out, notes, attempt, revision, loop, session,
                                                      prev_session, reason)
                if ok:
                    return True, out, session, ""
                if run["stop"]:
                    return False, out, session, "stopped"
                continue
            if resume and providers.split(step["model"] or wf["model"])[0] not in ("claude", "ollama"):
                extra_steer = "\n\n---\n" + self._steer_prompt(steer, interrupted)
                resume = None  # this tool can't continue a conversation: start over, with the guidance added
            else:
                extra_steer = ""
            if resume:
                prompt = self._steer_prompt(steer, interrupted)
            else:
                prompt = step["prompt"] + self._agent_instructions(wf, step)
                if wf["passOutput"] and prev_out:
                    prompt += f"\n\n---\n{prev_label or 'Output from the previous workflow step:'}\n{prev_out}"
                if notes:
                    prompt += f"\n\n---\nNotes from the user's review of the previous step (follow them):\n{notes}"
                prompt += extra
                if attempt > 1:
                    prompt += (f"\n\n---\nThis is retry {attempt - 1} of {step['retries']}. "
                               f"The previous attempt failed: {reason}\nFix the problem and complete the task.")
                prompt += extra_steer
            rec = {"step": step["id"], "name": step["name"], "attempt": attempt, "revision": revision, "session": session,
                   "replay": run.get("replay", 0), "loop": loop,
                   "input": prompt[-20000:], "inputModel": step["model"] or wf["model"] or "default",
                   "status": "running", "started": time.time(), "ended": None, "cost": 0.0, "error": "", "output": ""}
            if resume or extra_steer:
                rec.update(steer=steer, continues=resume)
            run["attempts"].append(rec)
            run["current"] = step["id"]
            self._link(run, step, session, attempt, resume or (prev_session if not revision else None), revision, loop)
            rec["before"] = quality.snapshot(wf["cwd"])  # checkpoint: lets the user see this step's diff and rewind it
            self._changed(run)

            ok, out, reason, cost = self._invoke(wf, run, step, prompt, session, resume=resume)
            rec["after"] = quality.snapshot(wf["cwd"]) if rec["before"] else None
            rec["usage"] = self.usage.pop(session, None)
            with self.lock:
                steered = (run.get("_steer") or {}).pop(step["id"], None)
            if steered is not None and not run["stop"]:  # the user steered it mid-step: continue with their message
                rec.update(status="steered", ended=time.time(), cost=cost, error="", output=out[-4000:])
                rec.pop("steerPending", None)
                run["cost"] += cost
                self._changed(run)
                resume, steer, interrupted = session, steered, True
                attempt -= 1  # steering doesn't use up a retry
                continue
            resume, steer, interrupted = None, "", False
            if ok and step["check"] and not run["stop"]:
                ok, reason = self._check(wf, step)
            if ok and step.get("judge") and not run["stop"]:
                ok, reason, judge_cost = self._judge(wf, run, step, rec, out)
                cost += judge_cost
            rec.update(status="ok" if ok else ("stopped" if run["stop"] else "failed"), ended=time.time(),
                       cost=cost, error="" if ok else reason, output=out[-4000:])
            run["cost"] += cost
            self._changed(run)
            if ok:
                return True, out, session, ""
        return False, out, session, reason

    def _relink(self, run):
        """Restore step labels and links for a run saved by an earlier app session."""
        prev_ok = None
        with self.graph.lock:
            for a in run["attempts"]:
                n = self.graph.node("s:" + a["session"], kind="session")
                n.update(label=a["name"] + (f" (replay {a['replay']})" if a.get("replay") else "")
                         + (f" (revision {a['revision']})" if a.get("revision") else "")
                         + (f" (retry {a['attempt'] - 1})" if a["attempt"] > 1 else ""),
                         fixedLabel=True, workflowStep=True)
                self.graph.edge("w:" + run["id"], n["id"], "retry" if a["attempt"] > 1 else "spawn", a["started"],
                                a["name"], live=False)
                if prev_ok and a["attempt"] == 1:
                    self.graph.edge(prev_ok, n["id"], "handback", a["started"], f"Output handed to “{a['name']}”", live=False)
                if a["status"] in ("ok", "failed"):
                    prev_ok = n["id"]
            self.graph.version += 1

    def _link(self, run, step, session, attempt, prev_session, revision=0, loop=0):
        with self.graph.lock:
            n = self.graph.node("s:" + session, kind="session")
            n.update(label=f"{step['name']}" + (f" (replay {run['replay']})" if run.get("replay") else "")
                     + (f" (round {loop + 1})" if loop else "")
                     + (f" (revision {revision})" if revision else "")
                     + (f" (retry {attempt - 1})" if attempt > 1 else ""),
                     fixedLabel=True, workflowStep=True, lastActive=time.time())
            self.graph.edge("w:" + run["id"], "s:" + session, "retry" if attempt > 1 or revision else "spawn", time.time(),
                            f"{step['name']}" + (f" — retry {attempt - 1}" if attempt > 1 else ""))
            for prev in ([prev_session] if isinstance(prev_session, str) else prev_session or []) if attempt == 1 else []:
                self.graph.edge("s:" + prev, "s:" + session, "handback", time.time(),
                                f"Output handed to “{step['name']}”")
            self.graph.version += 1

    @staticmethod
    def _step_agents(wf, step):
        return [a for a in wf.get("agents", []) if a["name"] in step.get("agents", [])]

    def _agent_instructions(self, wf, step):
        agents = self._step_agents(wf, step)
        if len(agents) < 2:
            return ""  # a single subagent runs the whole step itself (claude --agent)
        listing = "\n".join(f"{i}. {a['name']}: {a['description']}" for i, a in enumerate(agents, 1))
        return ("\n\n---\nYou MUST use every one of these subagents for this task, via the Agent tool "
                "(subagent_type = its name). Give each the part of the task that matches its role, with the context "
                "it needs. Run them in parallel (several Agent calls in one message) unless one needs another's "
                "result; then go in the order listed. Do not do their parts yourself. When all have reported, "
                "combine their results into your final answer.\n" + listing)

    @staticmethod
    def _agents_used(session):
        """Subagent types a step's session actually ran, read from its transcript folder."""
        used = set()
        for meta in glob.glob(os.path.join(os.path.expanduser("~/.claude/projects"), "*", session, "subagents", "*.meta.json")):
            try:
                with open(meta) as f:
                    used.add(json.load(f).get("agentType"))
            except (OSError, ValueError):
                pass
        return used

    def _invoke(self, wf, run, step, prompt, session, resume=None):
        # resume: continue that session's conversation (steering), saved under this new session id
        provider, name = providers.split(step["model"] or wf["model"])
        try:
            providers.check_ready(provider, name)
        except ValueError as exc:
            return False, "", str(exc), 0.0
        agents = self._step_agents(wf, step)
        env, last_file = None, None
        if provider in ("claude", "ollama"):  # Claude Code (Ollama: pointed at the local model)
            cmd = [compat.claude_cmd(), "-p", "--output-format", "json", "--session-id", session,
                   *(["--resume", resume, "--fork-session"] if resume else []),
                   "-n", f"{wf['name']} · {step['name']}", "--permission-mode", wf["permissionMode"]]
            if name:
                cmd += ["--model", name]
            if provider == "ollama":
                env = {**os.environ, **providers.command("ollama", name, claude_cmd="", permission_mode="", cwd="")[1]}
            if wf["maxBudgetUsd"] and provider == "claude":
                cmd += ["--max-budget-usd", str(wf["maxBudgetUsd"])]
            if agents:
                defs = {}
                for a in agents:
                    d = {"description": a["description"], "prompt": a["prompt"]}
                    if a["tools"]:
                        d["tools"] = a["tools"]
                    if a["model"]:
                        d["model"] = a["model"]
                    defs[a["name"]] = d
                cmd += ["--agents", json.dumps(defs)]
                if len(agents) == 1:
                    cmd += ["--agent", agents[0]["name"]]  # the step runs as this subagent, so it is always used
            allowed = list(wf["allowedTools"])
            if agents and allowed and "Agent" not in allowed:
                allowed.append("Agent")  # an allow-list would otherwise block delegating
            if allowed:
                cmd += ["--allowedTools", ",".join(allowed)]
            if wf["disallowedTools"]:
                cmd += ["--disallowedTools", ",".join(wf["disallowedTools"])]
        else:  # Gemini CLI / Codex CLI: no subagents, so the helpers' instructions go into the prompt
            if agents:
                prompt = self._roles_text(agents) + prompt
            os.makedirs(RUN_DIR, exist_ok=True)
            last_file = os.path.join(RUN_DIR, f"{session}.answer.txt")
            cmd, extra = providers.command(provider, name, claude_cmd="", permission_mode=wf["permissionMode"],
                                           cwd=wf["cwd"], last_message_file=last_file)
            env = {**os.environ, **extra} if extra else None
        tool = providers.INFO[provider]["cli"] if provider != "ollama" else "claude"
        try:
            proc = subprocess.Popen(cmd, cwd=wf["cwd"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                    stderr=subprocess.PIPE, text=True, env=env, **compat.detached())
        except OSError as exc:
            return False, "", f"couldn't start {tool}: {exc}", 0.0
        with self.lock:
            self.procs.setdefault(run["id"], set()).add(proc)
            self.proc_of[session] = proc
        stdout, stderr = proc.communicate(prompt)
        with self.lock:
            self.procs.get(run["id"], set()).discard(proc)
            self.proc_of.pop(session, None)
        if provider in ("gemini", "gpt"):
            out = stdout.strip()
            if last_file and os.path.exists(last_file):  # codex: just its final answer, not the whole transcript
                with open(last_file, errors="replace") as f:
                    out = f.read().strip() or out
                os.remove(last_file)
            if proc.returncode != 0:
                tail = (stderr or stdout or "").strip()[-400:]
                return False, out, f"{tool} exited with code {proc.returncode}: {tail}", 0.0
            return True, out, "", 0.0  # these tools don't report a price
        try:
            res = json.loads(stdout.strip().splitlines()[-1])
        except (ValueError, IndexError):
            tail = (stderr or stdout or "").strip()[-400:]
            return False, "", f"claude exited with code {proc.returncode}: {tail}", 0.0
        out = str(res.get("result") or "")
        cost = float(res.get("total_cost_usd") or 0) if provider == "claude" else 0.0  # local models are free
        self.usage[session] = _usage(res)
        if proc.returncode != 0 or res.get("is_error"):
            return False, out, f"{res.get('subtype') or 'error'}: {out[-400:]}", cost
        if len(agents) > 1 and not resume:  # several subagents: each one must have been used (a steer may not need them again)
            missing = [a["name"] for a in agents if a["name"] not in self._agents_used(session)]
            if missing:
                return False, out, f"didn't use the subagent{'s' if len(missing) > 1 else ''} {', '.join(missing)}", cost
        return True, out, "", cost

    @staticmethod
    def _roles_text(agents):
        """Helpers, written out for tools without subagents (Gemini CLI, Codex CLI)."""
        if len(agents) == 1:
            a = agents[0]
            return f"Work as this role for the whole task: {a['name']}: {a['description']}\n{a['prompt']}\n\n---\n"
        roles = "\n\n".join(f"### {a['name']}: {a['description']}\n{a['prompt']}" for a in agents)
        return ("Do this task by working through each of these roles in turn, giving each its part of the work, "
                "then combine the results:\n\n" + roles + "\n\n---\n")

    def _run_bash(self, wf, run, step, prev_out, notes, attempt, revision, loop, session, prev_session, last_reason):
        """A shell step: runs the command with bash in the workflow folder. No Claude, so no cost.
        Exit code 0 = success. The previous step's output is in $STEP_INPUT; output streams to a log file."""
        log_path = os.path.join(RUN_DIR, f"{run['id']}-{session}.log")
        rec = {"step": step["id"], "name": step["name"], "attempt": attempt, "revision": revision, "session": session,
               "replay": run.get("replay", 0), "loop": loop, "kind": "bash", "log": log_path,
               "input": "$ " + step["run"] + (f"\n\n(retry {attempt - 1}: the previous try failed: {last_reason})" if attempt > 1 else ""),
               "inputModel": "bash (no cost)", "status": "running", "started": time.time(), "ended": None, "cost": 0.0,
               "error": "", "output": ""}
        run["attempts"].append(rec)
        run["current"] = step["id"]
        self._link(run, step, session, attempt, prev_session if not revision else None, revision, loop)
        rec["before"] = quality.snapshot(wf["cwd"])
        self._changed(run)
        env = dict(os.environ, STEP_INPUT=prev_out or "", STEP_NOTES=notes or "", WORKFLOW_NAME=wf["name"],
                   WORKFLOW_RUN=run["id"], STEP_ID=step["id"], STEP_ATTEMPT=str(attempt))
        ok, reason = False, ""
        try:
            with open(log_path, "w") as logf:
                proc = subprocess.Popen(compat.shell_command(step["run"]), cwd=wf["cwd"], stdout=logf, stderr=subprocess.STDOUT,
                                        stdin=subprocess.DEVNULL, env=compat.shell_env(env), text=True, **compat.detached())
                with self.lock:
                    self.procs.setdefault(run["id"], set()).add(proc)
                try:
                    code = proc.wait(timeout=step.get("timeout") or BASH_TIMEOUT)
                except subprocess.TimeoutExpired:
                    compat.kill_tree(proc.pid)
                    code = None
                finally:
                    with self.lock:
                        self.procs.get(run["id"], set()).discard(proc)
            with open(log_path, errors="replace") as f:
                out = f.read()[-20000:]
            if code is None:
                reason = f"`{step['run']}` timed out after {step.get('timeout') or BASH_TIMEOUT}s"
            elif code != 0:
                reason = f"`{step['run']}` exited {code}:\n{out.strip()[-1500:]}"
            else:
                ok = True
        except OSError as exc:
            out, reason = "", f"couldn't run the command: {exc}"
        rec["after"] = quality.snapshot(wf["cwd"]) if rec["before"] else None
        if ok and step["check"] and not run["stop"]:
            ok, reason = self._check(wf, step)
        if ok and step.get("judge") and not run["stop"]:
            ok, reason, rec["cost"] = self._judge(wf, run, step, rec, out)
            run["cost"] += rec["cost"]
        rec.update(status="ok" if ok else ("stopped" if run["stop"] else "failed"), ended=time.time(),
                   error="" if ok else reason, output=out[-4000:])
        self._changed(run)
        return ok, out, reason, rec

    def _judge(self, wf, run, step, rec, out):
        """The AI judge grades this attempt against the step's criteria. Returns (ok, reason, cost)."""
        rec["judge"] = {"status": "running"}
        self._changed(run)
        # Retries and revisions build on the files earlier attempts left, so judge everything since the step began
        # (this round of it), not just the last attempt's own changes.
        base = next((a.get("before") for a in run["attempts"] if a["step"] == step["id"] and a.get("before")
                     and a.get("replay", 0) == rec.get("replay", 0) and a.get("loop", 0) == rec.get("loop", 0)),
                    rec.get("before"))
        try:
            v = quality.judge(step["judge"], step["prompt"] or "$ " + step["run"], out, wf["cwd"], base,
                              rec.get("after"), _written_files(rec["session"]), wf.get("judgeModel"), compat.claude_cmd())
        except ValueError as exc:
            rec["judge"] = {"status": "error", "error": str(exc)}
            return False, f"AI judge: {exc}", 0.0
        rec["judge"] = dict(v, status="pass" if v["pass"] else "fail")
        self._changed(run)
        if v["pass"]:
            return True, "", v["cost"]
        unmet = "\n".join(f"- {c.get('criterion')}: {c.get('evidence')}" for c in v["criteria"] if not c.get("met"))
        return False, (f"the AI judge scored it {v['score']}/100 and it doesn't meet the acceptance criteria yet."
                       + (f"\nNot met:\n{unmet}" if unmet else "") + f"\nWhat to fix: {v['feedback']}"), v["cost"]

    def _attempt(self, rid, session):
        with self.lock:
            run = self.runs.get(rid)
            if not run:
                raise ValueError("Run not found.")
            att = next((a for a in run["attempts"] if a["session"] == session), None)
        if not att:
            raise ValueError("That step attempt isn't part of this run.")
        return run, att

    def changes(self, rid, session):
        """The exact diff one step attempt made to the project folder (from its before/after checkpoints)."""
        run, att = self._attempt(rid, session)
        if not att.get("before"):
            return {"available": False, "reason": "No checkpoints for this step: the project folder isn't a git "
                                                  "repository, or the run is from before checkpoints existed."}
        if not att.get("after"):
            return {"available": False, "reason": "This step is still working. Its changes show when it finishes."}
        try:
            return dict(quality.diff(run["cwd"], att["before"], att["after"]), available=True,
                        rewound=(run.get("rewound") or {}).get("session") == session)
        except (OSError, subprocess.TimeoutExpired) as exc:
            return {"available": False, "reason": f"Couldn't read the checkpoints: {exc}"}

    def rewind(self, rid, session):
        """Put the project folder back as it was just before this step attempt started."""
        run, att = self._attempt(rid, session)
        if run["status"] in ("running", "waiting"):
            raise ValueError("Stop the run (or let it finish) before rewinding, so no step is changing files.")
        if not att.get("before"):
            raise ValueError("This step has no checkpoint to go back to.")
        try:
            undo = quality.restore(run["cwd"], att["before"])
        except (OSError, subprocess.TimeoutExpired) as exc:
            raise ValueError(f"Couldn't rewind: {exc}")
        run["rewound"] = {"session": session, "name": att["name"], "undo": undo, "t": time.time()}
        self._changed(run)
        return run["rewound"]

    def undo_rewind(self, rid):
        with self.lock:
            run = self.runs.get(rid)
        if not run or not run.get("rewound"):
            raise ValueError("There's no rewind to undo.")
        if run["status"] in ("running", "waiting"):
            raise ValueError("Stop the run before undoing the rewind.")
        try:
            quality.restore(run["cwd"], run["rewound"]["undo"])
        except (OSError, subprocess.TimeoutExpired) as exc:
            raise ValueError(f"Couldn't undo the rewind: {exc}")
        run.pop("rewound")
        self._changed(run)

    def _check(self, wf, step):
        try:
            p = subprocess.run(compat.shell_command(step["check"]), cwd=wf["cwd"], env=compat.shell_env(os.environ), capture_output=True, text=True,
                               timeout=CHECK_TIMEOUT)
        except subprocess.TimeoutExpired:
            return False, f"check timed out after {CHECK_TIMEOUT}s: {step['check']}"
        if p.returncode == 0:
            return True, ""
        tail = (p.stdout + p.stderr).strip()[-1500:]
        return False, f"check `{step['check']}` exited {p.returncode}:\n{tail}"
