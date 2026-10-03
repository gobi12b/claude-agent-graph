"""Checkpoints and the AI judge: what each step changed, a way back, and a second opinion on its work.

Checkpoints: before and after every step the project folder is snapshotted as a git tree, using a private
index file, so your branch, index, HEAD and stash are never touched. Untracked files are included and
.gitignore is respected. Two snapshots give the step's exact diff; rewinding puts the folder back to a
snapshot (and snapshots the current state first, so the rewind itself can be undone). Folders that aren't
git repositories simply get no checkpoints.

Judge: an independent model (fresh context, no tools) grades a step against plain-language criteria,
looking at the real diff rather than the step's own account of what it did. A failing verdict fails the
step, and its feedback becomes the reason given to the retry or loop-back.
"""

import json
import os
import shutil
import subprocess
import tempfile


GIT_TIMEOUT = 60
DIFF_LIMIT = 200_000      # characters of patch sent to the UI
JUDGE_DIFF_LIMIT = 60_000  # characters of patch / file content the judge reads
JUDGE_SCHEMA = {
    "type": "object",
    "properties": {
        "pass": {"type": "boolean"},
        "score": {"type": "integer", "minimum": 0, "maximum": 100},
        "criteria": {"type": "array", "items": {"type": "object", "properties": {
            "criterion": {"type": "string"}, "met": {"type": "boolean"}, "evidence": {"type": "string"}},
            "required": ["criterion", "met", "evidence"]}},
        "feedback": {"type": "string"},
    },
    "required": ["pass", "score", "criteria", "feedback"],
}
JUDGE_SYSTEM = (
    "You are a strict, independent reviewer of work done by an AI coding agent. You did not do the work and have "
    "no stake in it. Grade it ONLY against the acceptance criteria you are given.\n"
    "Rules:\n"
    "- Split the criteria into separate checks and judge each one on evidence from the DIFF / FILES. The agent's own "
    "summary is a claim, not evidence: agents often say they did things they didn't. If the evidence for a criterion "
    "is missing, it is not met.\n"
    "- pass is true only if every criterion is met. Don't fail work for things the criteria don't ask for.\n"
    "- score: 0-100 for how fully the criteria are met.\n"
    "- feedback: if it fails, tell the agent exactly what to fix, concretely (files, functions, what's missing), "
    "in a few sentences addressed to it. If it passes, one short sentence.")


# ---- checkpoints ----------------------------------------------------------------

def _git(cwd, *args, env=None, input=None):
    p = subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True, timeout=GIT_TIMEOUT, input=input,
                       env={**os.environ, "GIT_OPTIONAL_LOCKS": "0", **(env or {})})
    if p.returncode != 0:
        raise OSError((p.stderr or p.stdout).strip()[:300] or f"git {args[0]} failed")
    return p.stdout


def is_repo(cwd):
    if not shutil.which("git") or not os.path.isdir(cwd or ""):
        return False
    try:
        return _git(cwd, "rev-parse", "--is-inside-work-tree").strip() == "true"
    except (OSError, subprocess.TimeoutExpired):
        return False


def _with_index(cwd, fn):
    """Run fn(env) with a throwaway copy of the repo's index (copied so git can reuse its file-stat cache)."""
    fd, tmp = tempfile.mkstemp(prefix="agent-graph-index-")
    os.close(fd)
    try:
        real = os.path.join(cwd, _git(cwd, "rev-parse", "--git-path", "index").strip())
        if os.path.exists(real):
            shutil.copyfile(real, tmp)
        else:
            os.remove(tmp)  # a new repo: git starts the index from scratch
        return fn({"GIT_INDEX_FILE": tmp})
    finally:
        if os.path.exists(tmp):
            os.remove(tmp)


def snapshot(cwd):
    """The folder's current contents as a git tree id (tracked + untracked, minus ignored), or None."""
    if not is_repo(cwd):
        return None
    try:
        top = _git(cwd, "rev-parse", "--show-toplevel").strip()
        return _with_index(top, lambda env: (_git(top, "add", "-A", env=env), _git(top, "write-tree", env=env))[1].strip())
    except (OSError, subprocess.TimeoutExpired):
        return None


def diff(cwd, before, after):
    """What changed between two snapshots: per-file line counts and the unified patch."""
    top = _git(cwd, "rev-parse", "--show-toplevel").strip()
    files = []
    for line in _git(top, "diff", "--no-renames", "--numstat", before, after).splitlines():
        add, rem, path = line.split("\t", 2)
        files.append({"path": path, "added": None if add == "-" else int(add), "removed": None if rem == "-" else int(rem)})
    status = dict(reversed(l.split("\t", 1)) for l in _git(top, "diff", "--no-renames", "--name-status", before, after).splitlines())
    for f in files:
        f["status"] = {"A": "added", "D": "deleted"}.get(status.get(f["path"], "M")[0], "modified")
    patch = _git(top, "diff", "--no-color", "--no-renames", "-U3", before, after)
    return {"files": files, "patch": patch[:DIFF_LIMIT], "truncated": len(patch) > DIFF_LIMIT,
            "added": sum(f["added"] or 0 for f in files), "removed": sum(f["removed"] or 0 for f in files)}


def restore(cwd, tree):
    """Put the folder's files back to a snapshot. Only paths that differ are written or deleted; HEAD, the index
    and ignored files are left alone. Returns a snapshot of the state before, so this can be undone."""
    top = _git(cwd, "rev-parse", "--show-toplevel").strip()
    _git(top, "cat-file", "-e", tree + "^{tree}")  # fails if git has pruned it
    current = snapshot(top)
    if not current:
        raise OSError("couldn't snapshot the folder before rewinding")
    changes = [l.split("\t", 1) for l in _git(top, "diff", "--no-renames", "--name-status", tree, current).splitlines()]
    for st, path in changes:
        if st.startswith("A"):  # created since the snapshot: remove it
            full = os.path.join(top, path)
            if os.path.lexists(full):
                os.remove(full)
            _prune_dirs(top, os.path.dirname(full))
    back = [path for st, path in changes if not st.startswith("A")]
    if back:
        def write(env):
            _git(top, "read-tree", tree, env=env)
            _git(top, "checkout-index", "-f", "--stdin", env=env, input="\n".join(back) + "\n")
        _with_index(top, write)
    return current


def _prune_dirs(top, d):
    while d.startswith(top + os.sep) and os.path.isdir(d) and not os.listdir(d):
        os.rmdir(d)
        d = os.path.dirname(d)


# ---- judge --------------------------------------------------------------------------

def _evidence(cwd, before, after, files):
    """The judge's view of the work: the git diff when there are checkpoints, else the files the step wrote."""
    if before and after:
        try:
            d = diff(cwd, before, after)
            if not d["files"]:
                return "DIFF: the step changed no files."
            head = "\n".join(f"{f['status']:>8}  {f['path']}  (+{f['added'] or 0} -{f['removed'] or 0})" for f in d["files"])
            return f"DIFF of everything changed in the project since the step began (all its attempts so far):\n{head}\n\n{d['patch'][:JUDGE_DIFF_LIMIT]}"
        except (OSError, subprocess.TimeoutExpired, ValueError):
            pass
    parts, budget = [], JUDGE_DIFF_LIMIT
    for path in files:
        try:
            with open(path, errors="replace") as f:
                text = f.read(budget)
        except OSError:
            continue
        parts.append(f"=== {path}\n{text}")
        budget -= len(text)
        if budget <= 0:
            break
    return "FILES the step wrote or edited (current content):\n\n" + "\n\n".join(parts) if parts else \
        "FILES: the step wrote no files."


def judge(criteria, task, output, cwd, before=None, after=None, files=(), model="", claude_cmd="claude"):
    """Grade a step. Returns {pass, score, criteria, feedback, cost, model}; raises ValueError if it can't."""
    model = model or "haiku"
    prompt = (f"ACCEPTANCE CRITERIA:\n{criteria.strip()}\n\n"
              f"THE TASK the agent was given:\n{task.strip()[:6000]}\n\n"
              f"THE AGENT'S FINAL ANSWER (a claim, check it against the evidence):\n{(output or '(none)').strip()[-6000:]}\n\n"
              f"{_evidence(cwd, before, after, files)}")
    cmd = [claude_cmd, "-p", "--model", model, "--output-format", "json", "--no-session-persistence", "--tools", "",
           "--system-prompt", JUDGE_SYSTEM, "--json-schema", json.dumps(JUDGE_SCHEMA), "--max-budget-usd", "1"]
    try:
        p = subprocess.run(cmd, input=prompt, capture_output=True, text=True, timeout=300,
                           cwd=os.path.expanduser("~"))
        res = json.loads(p.stdout.strip().splitlines()[-1])
    except (subprocess.TimeoutExpired, ValueError, IndexError, OSError) as exc:
        raise ValueError(f"the judge didn't answer ({exc.__class__.__name__})")
    verdict = res.get("structured_output")
    if res.get("is_error") or not isinstance(verdict, dict):
        raise ValueError("the judge failed: " + str(res.get("result") or res.get("subtype"))[:200])
    return {"pass": bool(verdict.get("pass")), "score": int(verdict.get("score") or 0),
            "criteria": verdict.get("criteria") or [], "feedback": str(verdict.get("feedback") or "").strip(),
            "cost": float(res.get("total_cost_usd") or 0), "model": model}
