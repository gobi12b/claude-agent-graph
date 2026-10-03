"""Workflow and defaults storage, all as YAML files you can edit by hand or from the app.

Two stores:
  - App store (inside the app): defaults/agents.yaml (the subagent library) and defaults/steps.yaml
    (step templates). Your additions/overrides go in ~/.config/claude-agent-graph/{agents,steps}.yaml.
  - Project store: each workflow lives in its project folder, <project>/.claude/workflows/<id>.yaml,
    so it is versioned with the code. Projects are found from the folders you saved workflows to and
    from your Claude Code sessions' working folders.

The app re-reads files whenever they change on disk. Saves from the builder merge into the existing
file, so your comments and layout survive.
"""

import io
import json
import os
import re
import shutil
import threading
import time
import uuid

from ruamel.yaml import YAML
from ruamel.yaml.comments import CommentedMap, CommentedSeq
from ruamel.yaml.error import MarkedYAMLError
from ruamel.yaml.scalarstring import LiteralScalarString

APP_DIR = os.path.dirname(os.path.abspath(__file__))
DEFAULTS_DIR = os.path.join(APP_DIR, "defaults")
CONFIG_DIR = os.path.expanduser("~/.config/claude-agent-graph")
USER_AGENTS = os.path.join(CONFIG_DIR, "agents.yaml")
USER_STEPS = os.path.join(CONFIG_DIR, "steps.yaml")
SETTINGS = os.path.join(CONFIG_DIR, "settings.json")
LEGACY_DIR = os.environ.get("AGENT_GRAPH_WORKFLOW_DIR") or os.path.join(CONFIG_DIR, "workflows")
TRASH_DIR = os.path.join(CONFIG_DIR, "deleted")
PROJECT_SUBDIR = os.path.join(".claude", "workflows")
ID_RE = re.compile(r"^[a-z0-9][a-z0-9-]{0,60}$")
HOME = os.path.expanduser("~")

HEADER = """\
Claude Agent Graph workflow. Edit it here or in the app: both stay in sync,
and comments you add here are kept when the app saves.

  steps run in dependency order; steps whose dependencies are done run in parallel.
  Per step (id, name and prompt are required; or run instead of prompt for a free shell step):
    run: shell command   runs with bash in the workflow folder, no Claude, no cost; exit 0 = success.
                         Gets the previous output in $STEP_INPUT. timeout: seconds (default 1800)
    dependsOn: [step ids]   default: the step above it. [] = starts right away
    model: opus | sonnet | haiku     retries: 0-10      review: true (pause for you)
    check: shell command that must succeed for the step to pass
    judge: acceptance criteria in plain words; an AI judge grades the step's diff against them,
           and a failing verdict fails the step (its feedback goes to the retry / loop-back)
    agents: [subagent names]   one: the step runs as that subagent; several: it must use them all
    loopBack: {to: <earlier step id>, when: failure | success, max: 3}
        go back and redo from that step (and everything after it), up to max times
    onSuccess: next | end           onFailure: stop | continue
  agents: subagents the steps can delegate to (name: description, prompt, tools, model)
  permissionMode: acceptEdits | auto | dontAsk | plan | bypassPermissions
  judgeModel: the Claude model the AI judge uses (default haiku)
"""

STEP_DEFAULTS = {"model": "", "retries": 0, "check": "", "judge": "", "review": False, "agents": [],
                 "onSuccess": "next", "onFailure": "stop", "loopBack": None}

_lock = threading.Lock()
_cache = {}    # path -> (mtime, size, workflow or None, error or None)
_version = [0, None]  # bump counter, last directory signature


def _yaml():
    y = YAML()
    y.indent(mapping=2, sequence=4, offset=2)
    y.width = 110
    y.preserve_quotes = True
    return y


def path_for(wid):
    """The file a workflow id lives in (ids are file names, made unique across projects)."""
    if not ID_RE.match(str(wid)):
        raise ValueError("Bad workflow id.")
    path = _index().get(wid)
    if not path:
        raise ValueError("Workflow not found.")
    return path


def _tilde(p):
    return "~" + p[len(HOME):] if p == HOME or p.startswith(HOME + os.sep) else p


# ---- workflow dict <-> YAML document ----------------------------------------

def to_doc(wf):
    """The tidy shape written to disk: defaults left out, subagents keyed by name."""
    d = {"name": wf["name"], "cwd": _tilde(wf["cwd"])}
    if wf.get("model"):
        d["model"] = wf["model"]
    d["permissionMode"] = wf["permissionMode"]
    if not wf.get("passOutput", True):
        d["passOutput"] = False
    for k in ("allowedTools", "disallowedTools"):
        if wf.get(k):
            d[k] = list(wf[k])
    if wf.get("maxBudgetUsd") is not None:
        d["maxBudgetUsd"] = wf["maxBudgetUsd"]
    if wf.get("judgeModel"):
        d["judgeModel"] = wf["judgeModel"]
    if wf.get("agents"):
        d["agents"] = {a["name"]: {k: a[k] for k in ("description", "model", "tools", "prompt") if a.get(k)}
                       for a in wf["agents"]}
    steps, prev = [], None
    for s in wf["steps"]:
        out = {"id": s["id"], "name": s["name"]}
        default_deps = [prev] if prev else []
        if "dependsOn" in s and list(s["dependsOn"]) != default_deps:
            out["dependsOn"] = list(s["dependsOn"])
        if s.get("kind") == "bash":  # shell step: just the command (no Claude, no cost)
            out["run"] = s["run"]
            if s.get("timeout") and s["timeout"] != 1800:
                out["timeout"] = s["timeout"]
        else:
            out["prompt"] = s["prompt"]
        for k, default in STEP_DEFAULTS.items():
            if s.get(k, default) != default and not (s.get("kind") == "bash" and k in ("model", "agents")):
                out[k] = s[k]
        steps.append(out)
        prev = s["id"]
    d["steps"] = steps
    return d


def from_doc(doc, wid):
    """Accepts the tidy shape (and the older JSON shape) and returns the app's workflow dict."""
    if not isinstance(doc, dict):
        raise ValueError("The file should be a mapping with name, cwd and steps.")
    wf = json.loads(json.dumps(doc, default=str))  # plain types, no ruamel wrappers
    agents = wf.get("agents") or []
    if isinstance(agents, dict):
        agents = [{"name": name, **(spec or {})} for name, spec in agents.items()]
    for a in agents:
        if isinstance(a.get("tools"), str):
            a["tools"] = [t.strip() for t in a["tools"].split(",")]
    wf["agents"] = agents
    steps = wf.get("steps")
    if not isinstance(steps, list):
        raise ValueError("“steps” should be a list.")
    for i, s in enumerate(steps):
        if not isinstance(s, dict):
            raise ValueError(f"Step {i + 1} should be a mapping (id, name, prompt…).")
        if isinstance(s.get("agents"), str):
            s["agents"] = [s["agents"]]
        if not s.get("id"):
            s["id"] = _slug(s.get("name") or f"step-{i + 1}")
    wf["id"] = wid
    return wf


def _slug(text):
    return re.sub(r"[^a-z0-9]+", "-", str(text).lower()).strip("-")[:40] or "step"


# ---- round-trip merge (keeps comments and formatting) -------------------------

def _fresh(v):
    if isinstance(v, str):
        return LiteralScalarString(v) if "\n" in v or len(v) > 90 else v  # prompts read best as blocks
    if isinstance(v, dict):
        m = CommentedMap()
        for k, x in v.items():
            m[k] = _fresh(x)
        return m
    if isinstance(v, list):
        s = CommentedSeq(_fresh(x) for x in v)
        if all(isinstance(x, (str, int, float)) and "\n" not in str(x) for x in v):
            s.fa.set_flow_style()  # short lists on one line: [Read, Grep]
        return s
    return v


def _key(x):
    return x.get("id") or x.get("name") if isinstance(x, dict) else None


def _merge(old, new):
    if isinstance(old, dict) and isinstance(new, dict):
        for k in [k for k in old if k not in new]:
            del old[k]
        for k, v in new.items():
            old[k] = _merge(old[k], v) if k in old else _fresh(v)
        return old
    if isinstance(old, list) and isinstance(new, list) and new and all(isinstance(x, dict) for x in new):
        by = {_key(x): x for x in old if isinstance(x, dict)}
        merged = [_merge(by[_key(n)], n) if _key(n) in by else _fresh(n) for n in new]
        for i, item in enumerate(merged):  # assign in place: rebuilding the list would drop comments
            if i < len(old):
                if old[i] is not item:
                    old[i] = item
            else:
                old.append(item)
        while len(old) > len(merged):
            old.pop()
        return old
    if isinstance(old, list) and isinstance(new, list) and list(old) == new:
        return old
    if (not isinstance(old, (dict, list)) and not isinstance(new, (dict, list)) and old == new
            and isinstance(old, bool) == isinstance(new, bool)):
        return old  # unchanged: keep its quoting / block style
    return _fresh(new)


# ---- settings and projects ---------------------------------------------------------

project_hint = lambda: set()  # set by the app: folders of Claude Code sessions, where workflows may live


def read_settings():
    try:
        with open(SETTINGS) as f:
            return json.load(f)
    except (OSError, ValueError):
        return {}


def write_settings(data):
    os.makedirs(CONFIG_DIR, exist_ok=True)
    tmp = f"{SETTINGS}.{uuid.uuid4().hex[:8]}.tmp"
    with open(tmp, "w") as f:
        json.dump(data, f, indent=2)
    os.replace(tmp, SETTINGS)


def register_project(folder):
    folder = os.path.realpath(os.path.expanduser(folder))
    s = read_settings()
    projects = s.setdefault("projects", [])
    if folder not in projects:
        projects.append(folder)
        write_settings(s)


def project_dirs():
    """Folders that hold (or may hold) .claude/workflows: registered ones plus session folders that have one."""
    registered = [p for p in read_settings().get("projects", []) if os.path.isdir(p)]
    found = [p for p in project_hint() if os.path.isdir(os.path.join(p, PROJECT_SUBDIR))]
    return sorted(set(registered) | set(map(os.path.realpath, found)))


def _workflow_files():
    files = []
    for proj in project_dirs():
        d = os.path.join(proj, PROJECT_SUBDIR)
        try:
            files += [os.path.join(d, n) for n in sorted(os.listdir(d)) if n.endswith(".yaml")]
        except OSError:
            pass
    if os.path.isdir(LEGACY_DIR):  # files not yet moved into a project (e.g. their folder is missing)
        files += [os.path.join(LEGACY_DIR, n) for n in sorted(os.listdir(LEGACY_DIR)) if n.endswith(".yaml")]
    return files


_index_cache = [None, {}]


def _index():
    """id -> path. The id is the file name; a clash across projects gets a short suffix."""
    files = _workflow_files()
    key = tuple(files)
    if _index_cache[0] == key:
        return _index_cache[1]
    index = {}
    for path in files:
        stem = os.path.basename(path)[:-5]
        wid = stem
        if wid in index:
            wid = f"{stem}-{abs(hash(path)) % 10000:04d}"
        index[wid] = path
    _index_cache[0], _index_cache[1] = key, index
    return index


# ---- workflow files -----------------------------------------------------------------

def _load(path, wid):
    st = os.stat(path)
    hit = _cache.get(path)
    if hit and hit[0] == st.st_mtime_ns and hit[1] == st.st_size and hit[4] == wid:
        return hit[2], hit[3]
    wf, err = None, None
    try:
        if not ID_RE.match(wid):
            raise ValueError("Rename the file to lowercase letters, digits and dashes (e.g. my-flow.yaml).")
        with open(path) as f:
            doc = _yaml().load(f)
        wf = from_doc(doc, wid)
        wf["updated"] = st.st_mtime
        wf["file"] = path
    except MarkedYAMLError as exc:
        mark = exc.problem_mark
        err = f"line {mark.line + 1}, column {mark.column + 1}: {exc.problem}" if mark else str(exc)
    except (ValueError, OSError) as exc:
        err = str(exc)
    _cache[path] = (st.st_mtime_ns, st.st_size, wf, err, wid)
    return wf, err


def _short(path):
    return _tilde(path)


def load_all():
    """Returns (raw workflows, errors). Raw: parsed but not yet validated."""
    migrate()
    out, errors = [], []
    with _lock:
        for wid, path in _index().items():
            try:
                wf, err = _load(path, wid)
            except OSError:
                continue
            if wf:
                out.append(wf)
            else:
                errors.append({"file": path, "name": _short(path), "id": wid, "error": err})
    return out, errors


def _target_dir(cwd):
    return os.path.join(os.path.realpath(os.path.expanduser(cwd)), PROJECT_SUBDIR)


def write(wf, base_updated=None):
    """Save a validated workflow into <its folder>/.claude/workflows, merging into the existing file.
    base_updated is the file time the editor started from: if the file changed on disk since then,
    refuse rather than overwrite those edits. Changing the folder moves the file."""
    old_path = _index().get(wf["id"])
    target_dir = _target_dir(wf["cwd"])
    with _lock:
        y = _yaml()
        old = None
        if old_path and os.path.exists(old_path):
            if base_updated and os.path.getmtime(old_path) > float(base_updated) + 0.001:
                raise ValueError("This workflow's file was changed in your editor since you opened it here. "
                                 "Reload to get those changes, then make your edits again.")
            try:
                with open(old_path) as f:
                    old = y.load(f)
            except MarkedYAMLError:
                old = None
        if old_path and os.path.dirname(old_path) == target_dir:
            path = old_path
        else:  # new, or moved to another project
            stem = os.path.basename(old_path)[:-5] if old_path else wf["id"]
            path, n = os.path.join(target_dir, stem + ".yaml"), 2
            while os.path.exists(path):
                path, n = os.path.join(target_dir, f"{stem}-{n}.yaml"), n + 1
        doc = to_doc(wf)
        if isinstance(old, dict):
            data = _merge(old, doc)
        else:
            data = _fresh(doc)
            data.yaml_set_start_comment(HEADER)
        os.makedirs(target_dir, exist_ok=True)
        buf = io.StringIO()
        y.dump(data, buf)
        tmp = f"{path}.{uuid.uuid4().hex[:8]}.tmp"
        with open(tmp, "w") as f:
            f.write(buf.getvalue())
        os.replace(tmp, path)
        if old_path and old_path != path and os.path.exists(old_path):
            _trash(old_path)
    register_project(os.path.dirname(os.path.dirname(target_dir)))
    _index_cache[0] = None
    return path


def _trash(path):
    os.makedirs(TRASH_DIR, exist_ok=True)
    stem = os.path.basename(path)[:-5]
    shutil.move(path, os.path.join(TRASH_DIR, f"{stem}.{time.strftime('%Y%m%d-%H%M%S')}.yaml"))


def remove(wid):
    """Deleting keeps a copy in ~/.config/claude-agent-graph/deleted, in case it was a mistake."""
    path = path_for(wid)
    if os.path.exists(path):
        _trash(path)
    _index_cache[0] = None


def new_id(name):
    base = _slug(name)
    taken = set(_index())
    wid, n = base, 2
    while wid in taken:
        wid, n = f"{base}-{n}", n + 1
    return wid


def migrate():
    """One-time moves: old <id>.json files → YAML, and YAML in ~/.config → its project's .claude/workflows."""
    if not os.path.isdir(LEGACY_DIR):
        return
    backup = os.path.join(CONFIG_DIR, "workflows-backup")
    for name in sorted(os.listdir(LEGACY_DIR)):
        src = os.path.join(LEGACY_DIR, name)
        try:
            if name.endswith(".json"):
                with open(src) as f:
                    wf = json.load(f)
                wf["id"] = wf.get("id") or name[:-5]
            elif name.endswith(".yaml"):
                with open(src) as f:
                    wf = from_doc(_yaml().load(f), name[:-5])
            else:
                continue
            cwd = os.path.expanduser(str(wf.get("cwd") or ""))
            if not cwd or not os.path.isdir(cwd):
                continue  # stays in the old folder (still listed) until its folder exists
            dest = os.path.join(_target_dir(cwd), wf["id"] + ".yaml")
            if not os.path.exists(dest):
                os.makedirs(os.path.dirname(dest), exist_ok=True)
                if name.endswith(".yaml"):
                    shutil.copy2(src, dest)  # keeps comments exactly
                else:
                    wf["cwd"] = cwd
                    _index_cache[0] = None
                    _write_new(dest, wf)
            register_project(cwd)
            os.makedirs(backup, exist_ok=True)
            shutil.move(src, os.path.join(backup, name))
            _index_cache[0] = None
        except (OSError, ValueError, KeyError, MarkedYAMLError) as exc:
            print("workflow migration skipped", name, exc)


def _write_new(path, wf):
    data = _fresh(to_doc(wf))
    data.yaml_set_start_comment(HEADER)
    buf = io.StringIO()
    _yaml().dump(data, buf)
    with open(path, "w") as f:
        f.write(buf.getvalue())


# ---- defaults: agent library and step templates ------------------------------------

_defaults_cache = [None, None]


def _read_yaml(path):
    try:
        with open(path) as f:
            return _yaml().load(f) or {}
    except FileNotFoundError:
        return {}


def _plain(x):
    return json.loads(json.dumps(x, default=str))


def load_defaults():
    """The agent library and step templates: app defaults, then your files on top."""
    files = [os.path.join(DEFAULTS_DIR, "agents.yaml"), os.path.join(DEFAULTS_DIR, "steps.yaml"), USER_AGENTS, USER_STEPS]
    samples_file = os.path.join(DEFAULTS_DIR, "samples.yaml")
    key = tuple((f, os.path.getmtime(f)) for f in files + [samples_file] if os.path.exists(f))
    if _defaults_cache[0] == key:
        return _defaults_cache[1]
    errors, categories, steps = [], [], {}
    builtin_agents, builtin_steps = set(), set()
    for path in files[0], USER_AGENTS:
        try:
            doc = _plain(_read_yaml(path))
        except MarkedYAMLError as exc:
            errors.append({"name": _tilde(path), "file": path, "error": str(exc.problem)})
            continue
        for cat in doc.get("categories") or []:
            mine = path == USER_AGENTS
            agents = [{"name": n, **(a or {}), "source": "user" if mine else "builtin",
                       "overrides": mine and n in builtin_agents} for n, a in (cat.get("agents") or {}).items()]
            if not mine:
                builtin_agents.update(a["name"] for a in agents)
            for a in agents:  # a role with the same name replaces the earlier one
                for c in categories:
                    c["agents"] = [x for x in c["agents"] if x["name"] != a["name"]]
            same = next((c for c in categories if c["name"] == cat.get("name")), None)
            if same:
                same["agents"] += agents
            else:
                categories.append({"name": cat.get("name") or "My agents", "icon": cat.get("icon") or "⑂",
                                   "agents": agents, "user": path == USER_AGENTS})
    for path in files[1], USER_STEPS:
        try:
            doc = _plain(_read_yaml(path))
        except MarkedYAMLError as exc:
            errors.append({"name": _tilde(path), "file": path, "error": str(exc.problem)})
            continue
        mine = path == USER_STEPS
        for k, v in (doc.get("steps") or {}).items():
            steps[k] = {**(v or {}), "source": "user" if mine else "builtin", "overrides": mine and k in builtin_steps}
            if not mine:
                builtin_steps.add(k)
    samples = []  # ready-made workflows for "Start from a sample"
    try:
        for k, v in (_plain(_read_yaml(samples_file)).get("samples") or {}).items():
            samples.append({"id": k, **(v or {})})
    except MarkedYAMLError as exc:
        errors.append({"name": _tilde(samples_file), "file": samples_file, "error": str(exc.problem)})
    result = {"categories": [c for c in categories if c["agents"]], "steps": steps, "errors": errors, "samples": samples,
              "files": {"agents": files[0], "steps": files[1], "userAgents": USER_AGENTS, "userSteps": USER_STEPS}}
    _defaults_cache[0], _defaults_cache[1] = key, result
    return result


AGENT_NAME = re.compile(r"^[a-z][a-z0-9-]{0,39}$")


def _dump(path, doc):
    _defaults_cache[0] = None  # file times can be too coarse to notice a quick second save
    os.makedirs(CONFIG_DIR, exist_ok=True)
    buf = io.StringIO()
    _yaml().dump(doc, buf)
    with open(path, "w") as f:
        f.write(buf.getvalue())


def save_user_agent(agent, category="My helpers", old_name=None):
    """Add or replace a role in your own library file (~/.config/claude-agent-graph/agents.yaml)."""
    if not AGENT_NAME.match(str(agent.get("name") or "")):
        raise ValueError("Helper names use lowercase letters, digits and dashes, and start with a letter (e.g. test-writer).")
    if not str(agent.get("description") or "").strip() or not str(agent.get("prompt") or "").strip():
        raise ValueError("Give the helper a short description and its instructions.")
    if old_name and old_name != agent["name"]:
        delete_user_agent(old_name, missing_ok=True)
    doc = _read_yaml(USER_AGENTS)
    if not isinstance(doc, dict) or not doc:
        doc = CommentedMap()
        doc.yaml_set_start_comment("Your own subagent roles. Same shape as defaults/agents.yaml in the app;\n"
                                   "a role with the same name as a default one replaces it.")
    cats = doc.setdefault("categories", CommentedSeq())
    cat = next((c for c in cats if c.get("name") == category), None)
    if cat is None:
        cat = _fresh({"name": category, "icon": "⭐", "agents": {}})
        cats.append(cat)
    spec = {k: agent[k] for k in ("description", "model", "tools", "prompt") if agent.get(k)}
    if isinstance(spec.get("tools"), str):
        spec["tools"] = [t.strip() for t in spec["tools"].split(",") if t.strip()]
    for c in cats:  # one place per name
        if c is not cat and agent["name"] in (c.get("agents") or {}):
            del c["agents"][agent["name"]]
    cat["agents"][agent["name"]] = _fresh(spec)
    _dump(USER_AGENTS, doc)
    return load_defaults()


def delete_user_agent(name, missing_ok=False):
    """Remove one of your helpers. If it replaced a built-in one, the built-in one comes back."""
    doc = _read_yaml(USER_AGENTS)
    for c in (doc.get("categories") or []) if isinstance(doc, dict) else []:
        if name in (c.get("agents") or {}):
            del c["agents"][name]
            doc["categories"] = CommentedSeq([x for x in doc["categories"] if x.get("agents")])
            _dump(USER_AGENTS, doc)
            return load_defaults()
    if missing_ok:
        return load_defaults()
    raise ValueError("Only helpers you made can be deleted; built-in ones stay.")


def save_user_step(key, step, old_key=None):
    """Add or replace a step template in ~/.config/claude-agent-graph/steps.yaml."""
    key = _slug(key or step.get("name") or "")
    if not key or not str(step.get("name") or "").strip():
        raise ValueError("Give the template a name.")
    if not str(step.get("prompt") or step.get("run") or "").strip():
        raise ValueError("Say what the step should do (or the command it runs).")
    if old_key and old_key != key:
        delete_user_step(old_key, missing_ok=True)
    doc = _read_yaml(USER_STEPS)
    if not isinstance(doc, dict) or not doc:
        doc = CommentedMap()
        doc.yaml_set_start_comment("Your own step templates. Same shape as defaults/steps.yaml in the app;\n"
                                   "a template with the same key as a default one replaces it.")
    steps = doc.setdefault("steps", CommentedMap())
    spec = {}
    for k in ("icon", "name", "prompt", "run", "check", "judge", "model", "retries", "review", "agents"):
        v = step.get(k)
        if v not in (None, "", [], False, 0):
            spec[k] = LiteralScalarString(v) if k in ("prompt", "run", "judge") and "\n" in str(v) else v
    steps[key] = _fresh(spec)
    _dump(USER_STEPS, doc)
    return load_defaults()


def delete_user_step(key, missing_ok=False):
    doc = _read_yaml(USER_STEPS)
    if isinstance(doc, dict) and key in (doc.get("steps") or {}):
        del doc["steps"][key]
        _dump(USER_STEPS, doc)
        return load_defaults()
    if missing_ok:
        return load_defaults()
    raise ValueError("Only templates you made can be deleted; built-in ones stay.")


def version():
    """A counter that goes up whenever a workflow or defaults file is added, changed or removed."""
    sig = []
    for path in _workflow_files() + [os.path.join(DEFAULTS_DIR, "agents.yaml"), os.path.join(DEFAULTS_DIR, "steps.yaml"),
                                     USER_AGENTS, USER_STEPS]:
        try:
            st = os.stat(path)
            sig.append((path, st.st_mtime_ns, st.st_size))
        except OSError:
            pass
    sig = tuple(sig)
    if sig != _version[1]:
        _version[0] += 1
        _version[1] = sig
    return _version[0]
