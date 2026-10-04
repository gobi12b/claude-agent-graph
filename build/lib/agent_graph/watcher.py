"""Tails Claude Code transcripts under ~/.claude and builds a live agent graph.

Nodes:  "you", sessions ("s:<sessionId>") and subagents ("a:<agentId>").
Edges:  prompt (you -> session), spawn (parent -> subagent),
        message (SendMessage / peer message), handback (subagent report -> parent).
"""

import glob
import json
import os
import re
import threading
import time
from datetime import datetime

from . import compat

CLAUDE_DIR = os.path.expanduser("~/.claude")
PROJECTS = os.path.join(CLAUDE_DIR, "projects")
LIVE_SESSIONS = os.path.join(CLAUDE_DIR, "sessions")
UUID_RE = re.compile(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")
AGENT_RE = re.compile(r"^a[0-9a-f]{16}$")
MAX_EVENTS = 400
FILE_TOOLS = {"Write": "write", "Edit": "edit", "MultiEdit": "edit", "NotebookEdit": "edit"}
HIDDEN_FILE = os.path.expanduser("~/.config/claude-agent-graph/hidden.json")


def load_hidden():
    try:
        with open(HIDDEN_FILE) as f:
            return set(json.load(f))
    except (OSError, ValueError):
        return set()


def remember_killed(nid):
    """Killed sessions are hidden from the graph the next time the app starts."""
    ids = load_hidden() | {nid}
    os.makedirs(os.path.dirname(HIDDEN_FILE), exist_ok=True)
    with open(HIDDEN_FILE, "w") as f:
        json.dump(sorted(ids), f)


def parse_ts(s):
    if not s:
        return None
    try:
        return datetime.fromisoformat(s.replace("Z", "+00:00")).timestamp()
    except ValueError:
        return None


def short(text, n=140):
    text = " ".join(str(text).split())
    return text if len(text) <= n else text[: n - 1] + "…"


HOME_KEY = re.sub(r"[^A-Za-z0-9]", "-", os.path.expanduser("~")) + "-"
CODE_FOLDERS = ("Desktop-code-", "Documents-code-", "Documents-", "Desktop-", "code-", "projects-", "Projects-",
                "src-", "dev-", "repos-", "git-", "workspace-")


def project_label(dirname):
    # Claude names project folders after the path: "-home-me-Desktop-code-reclaim-life" -> "reclaim-life"
    name = dirname
    if name.lower().startswith(HOME_KEY.lower()):
        name = name[len(HOME_KEY):]
        for prefix in CODE_FOLDERS:
            if name.startswith(prefix):
                name = name[len(prefix):]
                break
    if "scratch" in name:
        return "scratch"
    return name.strip("-") or "~"


def describe_tool(block):
    inp = block.get("input") or {}
    name = block.get("name", "")
    for key in ("description", "command", "file_path", "pattern", "query", "url", "summary"):
        if key in inp:
            return f"{name}: {short(inp[key], 60)}"
    return name


def pid_running(pid):
    return compat.pid_alive(pid)


class Graph:
    def __init__(self):
        self.lock = threading.Lock()
        self.nodes = {"you": {"id": "you", "kind": "you", "label": "You", "lastActive": 0}}
        self.edges = {}
        self.events = []
        self.event_seq = 0
        self.files = {}  # path -> {"offset", "node"}
        self.tool_owner = {}  # tool_use id -> node id (for Agent calls)
        self.names = {}  # live session name -> node id
        self.version = 0
        self.hidden = load_hidden()  # fixed at startup so a kill stays visible until restart

    # ---- graph mutation -------------------------------------------------

    def node(self, nid, **defaults):
        n = self.nodes.get(nid)
        if n is None:
            n = {"id": nid, "lastActive": 0, "turns": 0, "label": nid[2:10], **defaults}
            self.nodes[nid] = n
        return n

    def touch(self, nid, ts):
        n = self.nodes.get(nid)
        if n and ts and ts > n.get("lastActive", 0):
            n["lastActive"] = ts

    def edge(self, src, dst, kind, ts, text="", live=True):
        if src == dst:
            return
        key = f"{src}|{dst}|{kind}"
        e = self.edges.get(key)
        if e is None:
            e = self.edges[key] = {"id": key, "src": src, "dst": dst, "kind": kind, "count": 0, "last": 0}
        e["count"] += 1
        e["last"] = max(e["last"], ts or 0)
        self.event(kind, src, dst, ts, text, live)

    def event(self, kind, src, dst, ts, text, live):
        self.event_seq += 1
        self.events.append({"seq": self.event_seq, "kind": kind, "src": src, "dst": dst,
                            "t": ts or time.time(), "text": short(text), "live": live})
        if len(self.events) > MAX_EVENTS:
            del self.events[: len(self.events) - MAX_EVENTS]

    def resolve(self, ref):
        """Map a SendMessage target / peer sender to a node id."""
        ref = str(ref).strip()
        if AGENT_RE.match(ref):
            self.node("a:" + ref, kind="agent")
            return "a:" + ref
        if UUID_RE.match(ref):
            self.node("s:" + ref, kind="session")
            return "s:" + ref
        if ref in self.names:
            return self.names[ref]
        nid = "n:" + ref
        self.node(nid, kind="session", label=ref)
        return nid

    # ---- transcript parsing ---------------------------------------------

    def file_node(self, path):
        rel = os.path.relpath(path, PROJECTS).split(os.sep)
        project = project_label(rel[0])
        if len(rel) >= 4 and rel[2] == "subagents":
            agent_id = os.path.basename(path)[len("agent-"):-len(".jsonl")]
            n = self.node("a:" + agent_id, kind="agent")
            n["project"] = project
            n["session"] = "s:" + rel[1]
            meta_path = path[: -len(".jsonl")] + ".meta.json"
            try:
                with open(meta_path) as f:
                    meta = json.load(f)
                n["label"] = meta.get("description") or n["label"]
                n["agentType"] = meta.get("agentType")
                n["background"] = meta.get("requestShape") == "background"
                owner = self.tool_owner.get(meta.get("toolUseId"))
                if owner and not n.get("parent"):
                    n["parent"] = owner
            except (OSError, ValueError):
                pass
            return n["id"]
        sid = rel[1][: -len(".jsonl")]
        n = self.node("s:" + sid, kind="session")
        n["project"] = project
        return n["id"]

    def ingest_line(self, nid, d, live):
        n = self.nodes[nid]
        typ = d.get("type")
        ts = parse_ts(d.get("timestamp"))

        if typ == "ai-title" and n["kind"] == "session" and not n.get("liveName") and not n.get("fixedLabel"):
            n["label"] = d.get("aiTitle") or n["label"]
            return
        if typ == "cost-state":
            n["cost"] = d.get("totalCostUSD")
            return
        if typ not in ("user", "assistant"):
            return

        self.touch(nid, ts)
        if n["kind"] == "session" and d.get("cwd") and not n.get("cwd"):
            n["cwd"] = d["cwd"]  # the project folder, used to find workflows stored there
        if not n.get("started") and ts:
            n["started"] = ts
        msg = d.get("message") or {}
        content = msg.get("content")
        origin = d.get("origin") or {}

        if typ == "assistant":
            n["turns"] += 1
            if isinstance(content, list):
                for b in content:
                    if b.get("type") != "tool_use":
                        continue
                    n["activity"] = describe_tool(b)
                    name, inp = b.get("name"), b.get("input") or {}
                    path = (inp.get("file_path") or inp.get("notebook_path")) if name in FILE_TOOLS else None
                    if path:  # artefacts: files this session wrote or edited
                        f = n.setdefault("files", {}).setdefault(path, {"first": FILE_TOOLS[name], "changes": 0})
                        f["changes"] += 1
                        f["t"] = ts
                    elif name == "Read" and inp.get("file_path"):  # inputs: files this session read
                        r = n.setdefault("reads", {}).setdefault(inp["file_path"], {"count": 0, "t": ts})
                        r["count"] += 1
                    if name in ("Agent", "Task"):
                        self.tool_owner[b.get("id")] = nid
                    elif name == "SendMessage" and inp.get("to"):
                        dst = self.resolve(inp["to"])
                        self.edge(nid, dst, "message", ts, inp.get("summary") or inp.get("message", ""), live)
            return

        # user records
        result = d.get("toolUseResult")
        if isinstance(result, dict) and result.get("agentId"):
            child = self.node("a:" + result["agentId"], kind="agent")
            child["parent"] = nid
            if result.get("description"):
                child["label"] = result["description"]
            self.edge(nid, child["id"], "spawn", ts, result.get("description", ""), live)
            if result.get("status") == "completed":  # foreground agent: its result comes back in the same record
                child["done"] = True
                text = result.get("content")
                if isinstance(text, list):
                    text = " ".join(b.get("text", "") for b in text if isinstance(b, dict))
                self.edge(child["id"], nid, "handback", ts, text or "Result returned", live)
            return

        kind = origin.get("kind")
        text = content if isinstance(content, str) else ""
        if isinstance(content, list):
            text = " ".join(b.get("text", "") for b in content if b.get("type") == "text")
        if kind == "peer" and origin.get("from"):
            src = self.resolve(origin["from"])
            if "[Subagent hand-back]" in text:
                self.nodes[src]["done"] = True
                body = text.split("\n\n", 1)[-1] if "\n\n" in text else text
                self.edge(src, nid, "handback", ts, body, live)
            else:
                self.edge(src, nid, "message", ts, text, live)
        elif n["kind"] == "session" and not n.get("workflowStep") and not d.get("isSidechain") and not d.get("isMeta"):
            human = kind == "human" or (not kind and isinstance(content, str) and not content.startswith("<"))
            if human and text.strip():
                self.touch("you", ts)
                self.edge("you", nid, "prompt", ts, text, live)

    def scan_file(self, path, live):
        st = self.files.get(path)
        if st is None:
            st = self.files[path] = {"offset": 0, "node": self.file_node(path)}
        try:
            size = os.path.getsize(path)
        except OSError:
            return False
        if size <= st["offset"]:
            return False
        with open(path, "rb") as f:
            f.seek(st["offset"])
            data = f.read(size - st["offset"])
        end = data.rfind(b"\n")
        if end < 0:
            return False
        st["offset"] += end + 1
        for raw in data[: end + 1].splitlines():
            try:
                self.ingest_line(st["node"], json.loads(raw), live)
            except (ValueError, AttributeError, TypeError):
                continue
        return True

    def scan_live_sessions(self):
        changed = False
        alive = {}
        for path in glob.glob(os.path.join(LIVE_SESSIONS, "*.json")):
            try:
                with open(path) as f:
                    info = json.load(f)
                if not compat.pid_alive(int(info["pid"])):
                    continue
            except (OSError, ValueError, KeyError):
                continue
            alive["s:" + info.get("sessionId", "")] = info
        for nid, n in self.nodes.items():
            if n["kind"] != "session":
                continue
            info = alive.get(nid)
            status = info.get("status", "live") if info else None
            if n.get("liveStatus") != status:
                n["liveStatus"] = status
                changed = True
            n["pid"] = int(info["pid"]) if info else None
            if info and info.get("name") and not n.get("fixedLabel"):
                self.names[info["name"]] = nid
                n["liveName"] = info["name"]
        return changed

    def scan(self, live=True):
        with self.lock:
            changed = False
            # Session transcripts first so Agent tool_use owners are known before subagent metas.
            for path in sorted(glob.glob(os.path.join(PROJECTS, "*", "*.jsonl"))):
                changed |= self.scan_file(path, live)
            for path in sorted(glob.glob(os.path.join(PROJECTS, "*", "*", "subagents", "agent-*.jsonl"))):
                changed |= self.scan_file(path, live)
            changed |= self.scan_live_sessions()
            if changed:
                self.version += 1
            return changed

    def kill_session(self, nid):
        """Stop a live session's process. Returns (ok, message)."""
        with self.lock:
            n = self.nodes.get(nid)
            if not n or n["kind"] != "session" or not n.get("pid"):
                return False, "That session isn't running."
            pid, sid = n["pid"], nid[2:]
        # Re-check the registry right before killing so a recycled pid is never hit.
        try:
            with open(os.path.join(LIVE_SESSIONS, f"{pid}.json")) as f:
                if json.load(f).get("sessionId") != sid:
                    return False, "Session registry changed; refresh and try again."
            if not compat.looks_like_claude(pid):
                return False, f"Process {pid} isn't Claude Code; not killing it."
            compat.terminate(pid)
        except ProcessLookupError:
            return False, "Session already exited."
        except (OSError, ValueError) as exc:
            return False, f"Couldn't kill process {pid}: {exc}"
        for _ in range(30):
            time.sleep(0.1)
            if not pid_running(pid):
                with self.lock:
                    n["liveStatus"] = None
                    n["pid"] = None
                    self.event("killed", "you", nid, time.time(), "Session killed from Agent Graph", True)
                    self.version += 1
                remember_killed(nid)
                return True, f"Killed session (pid {pid})."
        return False, f"Asked pid {pid} to exit, but it is still running."

    def root_id(self, nid):
        seen = set()
        while nid in self.nodes and self.nodes[nid]["kind"] == "agent" and nid not in seen:
            seen.add(nid)
            n = self.nodes[nid]
            nid = n.get("parent") or n.get("session") or nid
        return nid

    def agent_status(self, n, now=None):
        if n.get("done"):
            return "done"
        return "running" if (now or time.time()) - n["lastActive"] < 90 else "stopped"

    def session_paths(self, sid):
        """Transcript files for a session and every subagent under it, as [(path, who)]."""
        with self.lock:
            root = "s:" + sid
            out = []
            for path, st in self.files.items():
                nid = st["node"]
                if nid == root:
                    out.append((path, "step"))
                elif self.nodes.get(nid, {}).get("kind") == "agent" and self.root_id(nid) == root:
                    out.append((path, self.nodes[nid]["label"]))
            return out

    def session_detail(self, sid):
        """Files written by a session and everything it spawned, plus its subagents."""
        with self.lock:
            root = "s:" + sid
            files, agents = {}, []
            for nid, n in self.nodes.items():
                if nid != root and not (n["kind"] == "agent" and self.root_id(nid) == root):
                    continue
                by = "step" if nid == root else n["label"]
                for path, f in n.get("files", {}).items():
                    cur = files.setdefault(path, {"path": path, "first": f["first"], "changes": 0, "t": 0, "by": []})
                    cur["changes"] += f["changes"]
                    cur["t"] = max(cur["t"], f.get("t") or 0)
                    if by not in cur["by"]:
                        cur["by"].append(by)
                if nid != root:
                    agents.append({"id": nid, "label": n["label"], "type": n.get("agentType"),
                                   "status": self.agent_status(n), "parent": n.get("parent")})
            return sorted(files.values(), key=lambda f: f["t"]), agents

    def project_dirs(self):
        with self.lock:
            return {n["cwd"] for n in self.nodes.values() if n.get("cwd")}

    def session_reads(self, sid):
        """Files read by a session and its subagents, in the order first read."""
        with self.lock:
            root = "s:" + sid
            reads = {}
            for nid, n in self.nodes.items():
                if nid != root and not (n["kind"] == "agent" and self.root_id(nid) == root):
                    continue
                by = "step" if nid == root else n["label"]
                for path, r in n.get("reads", {}).items():
                    cur = reads.setdefault(path, {"path": path, "count": 0, "t": r["t"], "by": []})
                    cur["count"] += r["count"]
                    cur["t"] = min(cur["t"] or 0, r["t"] or 0) or cur["t"] or r["t"]
                    if by not in cur["by"]:
                        cur["by"].append(by)
            return sorted(reads.values(), key=lambda r: r["t"] or 0)

    def snapshot(self, since_seq=0):
        with self.lock:
            now = time.time()
            hidden = {nid for nid in self.nodes if self.root_id(nid) in self.hidden} if self.hidden else set()
            nodes = []
            for n in self.nodes.values():
                if n["id"] in hidden:
                    continue
                n = dict(n)
                n.pop("files", None)  # served per run via the API instead
                n.pop("reads", None)
                if n["kind"] == "agent":
                    n["status"] = self.agent_status(n, now)
                elif n["kind"] == "session":
                    n["status"] = n.get("liveStatus") or "ended"
                elif n["kind"] == "workflow":
                    n["status"] = n.get("runStatus")
                nodes.append(n)
            return {
                "now": now,
                "version": self.version,
                "nodes": nodes,
                "edges": [e for e in self.edges.values() if e["src"] not in hidden and e["dst"] not in hidden],
                "events": [e for e in self.events
                           if e["seq"] > since_seq and e["src"] not in hidden and e["dst"] not in hidden],
                "seq": self.event_seq,
            }
