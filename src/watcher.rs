//! Tails Claude Code transcripts under ~/.claude and builds a live agent graph.
//!
//! Nodes:  "you", sessions ("s:<sessionId>") and subagents ("a:<agentId>"), workflow runs ("w:<runId>").
//! Edges:  prompt (you -> session), spawn (parent -> subagent),
//!         message (SendMessage / peer message), handback (subagent report -> parent).

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::sync::{Mutex, MutexGuard, OnceLock};

use indexmap::IndexMap;
use regex::Regex;
use serde_json::{json, Map, Value};

use crate::compat;
use crate::util::*;

const MAX_EVENTS: usize = 400;

/// Tools that write files, and how a first use is described.
pub fn file_tool(name: &str) -> Option<&'static str> {
    match name {
        "Write" => Some("write"),
        "Edit" | "MultiEdit" | "NotebookEdit" => Some("edit"),
        _ => None,
    }
}

pub fn projects_dir() -> String {
    join(&claude_dir(), "projects")
}
fn live_sessions_dir() -> String {
    join(&claude_dir(), "sessions")
}
fn hidden_file() -> String {
    join(&config_dir(), "hidden.json")
}

fn load_hidden() -> HashSet<String> {
    read_json(&hidden_file()).map(|v| v.as_array().map(|a| a.iter().map(py_str).collect()).unwrap_or_default()).unwrap_or_default()
}

/// Killed sessions are hidden from the graph the next time the app starts.
fn remember_killed(nid: &str) {
    let mut ids: Vec<String> = load_hidden().into_iter().chain([nid.to_string()]).collect();
    ids.sort();
    ids.dedup();
    let _ = std::fs::create_dir_all(config_dir());
    let _ = std::fs::write(hidden_file(), serde_json::to_string(&ids).unwrap_or_default());
}

/// An ISO-8601 timestamp as Unix seconds.
pub fn parse_ts(v: Option<&Value>) -> Option<f64> {
    let text = v?.as_str()?;
    if text.is_empty() {
        return None;
    }
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(text) {
        return Some(t.timestamp_micros() as f64 / 1e6);
    }
    let naive = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f").ok()?;
    naive.and_local_timezone(chrono::Local).single().map(|t| t.timestamp_micros() as f64 / 1e6)
}

fn uuid_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new("^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$").unwrap())
}

fn agent_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new("^a[0-9a-f]{16}$").unwrap())
}

/// Claude names project folders after the path: "-home-me-Desktop-code-reclaim-life" -> "reclaim-life"
pub fn project_label(dirname: &str) -> String {
    static HOME_KEY: OnceLock<String> = OnceLock::new();
    let home_key = HOME_KEY.get_or_init(|| Regex::new("[^A-Za-z0-9]").unwrap().replace_all(&home(), "-").to_string() + "-");
    const CODE_FOLDERS: [&str; 12] =
        ["Desktop-code-", "Documents-code-", "Documents-", "Desktop-", "code-", "projects-", "Projects-", "src-", "dev-", "repos-", "git-", "workspace-"];
    let mut name = dirname.to_string();
    if name.to_lowercase().starts_with(&home_key.to_lowercase()) {
        name = name[home_key.len()..].to_string();
        if let Some(prefix) = CODE_FOLDERS.iter().find(|p| name.starts_with(*p)) {
            name = name[prefix.len()..].to_string();
        }
    }
    if name.contains("scratch") {
        return "scratch".into();
    }
    let name = name.trim_matches('-');
    if name.is_empty() { "~".into() } else { name.into() }
}

fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => py_str(other),
    }
}

fn describe_tool(block: &Value) -> String {
    let inp = block.get("input").cloned().unwrap_or(json!({}));
    let name = s(block, "name");
    for key in ["description", "command", "file_path", "pattern", "query", "url", "summary"] {
        if let Some(v) = inp.get(key) {
            return format!("{name}: {}", short(&value_text(v), 60));
        }
    }
    name
}

struct FileState {
    offset: u64,
    node: String,
}

pub struct GraphState {
    pub nodes: IndexMap<String, Map<String, Value>>,
    edges: IndexMap<String, Map<String, Value>>,
    events: Vec<Value>,
    event_seq: u64,
    files: IndexMap<String, FileState>,
    tool_owner: HashMap<String, String>, // tool_use id -> node id (for Agent calls)
    names: HashMap<String, String>,      // live session name -> node id
    pub version: u64,
    hidden: HashSet<String>, // fixed at startup so a kill stays visible until restart
}

pub struct Graph {
    state: Mutex<GraphState>,
}

impl Default for Graph {
    fn default() -> Self {
        Self::new()
    }
}

impl Graph {
    pub fn new() -> Self {
        let mut nodes = IndexMap::new();
        nodes.insert("you".to_string(), json!({"id": "you", "kind": "you", "label": "You", "lastActive": 0}).as_object().unwrap().clone());
        Graph {
            state: Mutex::new(GraphState {
                nodes,
                edges: IndexMap::new(),
                events: vec![],
                event_seq: 0,
                files: IndexMap::new(),
                tool_owner: HashMap::new(),
                names: HashMap::new(),
                version: 0,
                hidden: load_hidden(),
            }),
        }
    }

    pub fn lock(&self) -> MutexGuard<'_, GraphState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn scan(&self, live: bool) -> bool {
        let mut g = self.lock();
        let mut changed = false;
        // Session transcripts first so Agent tool_use owners are known before subagent metas.
        let projects = projects_dir();
        let mut sessions = vec![];
        let mut agents = vec![];
        for proj in list_dir(&projects) {
            let pdir = join(&projects, &proj);
            for name in list_dir(&pdir) {
                let p = join(&pdir, &name);
                if name.ends_with(".jsonl") {
                    sessions.push(p);
                } else {
                    let sub = join(&p, "subagents");
                    for a in list_dir(&sub) {
                        if a.starts_with("agent-") && a.ends_with(".jsonl") {
                            agents.push(join(&sub, &a));
                        }
                    }
                }
            }
        }
        sessions.sort();
        agents.sort();
        for path in sessions.iter().chain(agents.iter()) {
            changed |= g.scan_file(path, live);
        }
        changed |= g.scan_live_sessions();
        if changed {
            g.version += 1;
        }
        changed
    }

    /// Stop a live session's process. Returns (ok, message).
    pub fn kill_session(&self, nid: &str) -> (bool, String) {
        let pid = {
            let g = self.lock();
            match g.nodes.get(nid) {
                Some(n) if n.get("kind") == Some(&json!("session")) && n.get("pid").and_then(Value::as_u64).is_some() => n["pid"].as_u64().unwrap() as u32,
                _ => return (false, "That session isn't running.".into()),
            }
        };
        let sid = &nid[2..];
        // Re-check the registry right before killing so a recycled pid is never hit.
        let reg = read_json(&join(&live_sessions_dir(), &format!("{pid}.json")));
        match reg {
            None => return (false, format!("Couldn't kill process {pid}: its session file is gone")),
            Some(r) if r.get("sessionId").and_then(Value::as_str) != Some(sid) => {
                return (false, "Session registry changed; refresh and try again.".into())
            }
            _ => {}
        }
        if !compat::pid_alive(pid) {
            return (false, "Session already exited.".into());
        }
        if !compat::looks_like_claude(pid) {
            return (false, format!("Process {pid} isn't Claude Code; not killing it."));
        }
        if let Err(e) = compat::terminate(pid) {
            return (false, format!("Couldn't kill process {pid}: {e}"));
        }
        for _ in 0..30 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if !compat::pid_alive(pid) {
                {
                    let mut g = self.lock();
                    if let Some(n) = g.nodes.get_mut(nid) {
                        n.insert("liveStatus".into(), Value::Null);
                        n.insert("pid".into(), Value::Null);
                    }
                    g.event("killed", "you", nid, Some(now()), "Session killed from Agent Graph", true);
                    g.version += 1;
                }
                remember_killed(nid);
                return (true, format!("Killed session (pid {pid})."));
            }
        }
        (false, format!("Asked pid {pid} to exit, but it is still running."))
    }

    /// Transcript files for a session and every subagent under it, as [(path, who)].
    pub fn session_paths(&self, sid: &str) -> Vec<(String, String)> {
        let g = self.lock();
        let root = format!("s:{sid}");
        let mut out = vec![];
        for (path, st) in &g.files {
            if st.node == root {
                out.push((path.clone(), "step".into()));
            } else if g.nodes.get(&st.node).is_some_and(|n| n.get("kind") == Some(&json!("agent"))) && g.root_id(&st.node) == root {
                out.push((path.clone(), py_str(&g.nodes[&st.node]["label"])));
            }
        }
        out
    }

    fn members<'a>(g: &'a GraphState, root: &'a str) -> impl Iterator<Item = (&'a String, &'a Map<String, Value>)> + 'a {
        g.nodes.iter().filter(move |(nid, n)| *nid == root || (n.get("kind") == Some(&json!("agent")) && g.root_id(nid) == root))
    }

    /// Files written by a session and everything it spawned, plus its subagents.
    pub fn session_detail(&self, sid: &str) -> (Vec<Value>, Vec<Value>) {
        let g = self.lock();
        let root = format!("s:{sid}");
        let mut files: IndexMap<String, Value> = IndexMap::new();
        let mut agents = vec![];
        let now = now();
        for (nid, n) in Self::members(&g, &root) {
            let by = if *nid == root { "step".to_string() } else { py_str(&n["label"]) };
            if let Some(Value::Object(fs)) = n.get("files") {
                for (path, fv) in fs {
                    let cur = files
                        .entry(path.clone())
                        .or_insert_with(|| json!({"path": path, "first": fv["first"], "changes": 0, "t": 0, "by": []}));
                    cur["changes"] = json!(cur["changes"].as_i64().unwrap_or(0) + fv["changes"].as_i64().unwrap_or(0));
                    cur["t"] = json!(f(cur, "t").max(f(fv, "t")));
                    let byv = json!(by);
                    if !cur["by"].as_array().unwrap().contains(&byv) {
                        cur["by"].as_array_mut().unwrap().push(byv);
                    }
                }
            }
            if *nid != root {
                agents.push(json!({"id": nid, "label": n["label"], "type": n.get("agentType").cloned().unwrap_or(Value::Null),
                                   "status": agent_status(n, now), "parent": n.get("parent").cloned().unwrap_or(Value::Null)}));
            }
        }
        let mut files: Vec<Value> = files.into_values().collect();
        files.sort_by(|a, b| f(a, "t").total_cmp(&f(b, "t")));
        (files, agents)
    }

    /// Files read by a session and its subagents, in the order first read.
    pub fn session_reads(&self, sid: &str) -> Vec<Value> {
        let g = self.lock();
        let root = format!("s:{sid}");
        let mut reads: IndexMap<String, Value> = IndexMap::new();
        for (nid, n) in Self::members(&g, &root) {
            let by = if *nid == root { "step".to_string() } else { py_str(&n["label"]) };
            if let Some(Value::Object(rs)) = n.get("reads") {
                for (path, r) in rs {
                    let cur = reads.entry(path.clone()).or_insert_with(|| json!({"path": path, "count": 0, "t": r["t"], "by": []}));
                    cur["count"] = json!(cur["count"].as_i64().unwrap_or(0) + r["count"].as_i64().unwrap_or(0));
                    let (a, b) = (cur["t"].as_f64(), r["t"].as_f64());
                    let m = a.unwrap_or(0.0).min(b.unwrap_or(0.0));
                    cur["t"] = if m != 0.0 { json!(m) } else if a.is_some_and(|x| x != 0.0) { json!(a) } else { json!(b) };
                    let byv = json!(by);
                    if !cur["by"].as_array().unwrap().contains(&byv) {
                        cur["by"].as_array_mut().unwrap().push(byv);
                    }
                }
            }
        }
        let mut out: Vec<Value> = reads.into_values().collect();
        out.sort_by(|a, b| f(a, "t").total_cmp(&f(b, "t")));
        out
    }

    pub fn project_dirs(&self) -> HashSet<String> {
        self.lock().nodes.values().filter_map(|n| n.get("cwd").filter(|c| truthy(c)).map(py_str)).collect()
    }

    pub fn snapshot(&self, since_seq: u64) -> Value {
        let g = self.lock();
        let now = now();
        let hidden: HashSet<&String> =
            if g.hidden.is_empty() { HashSet::new() } else { g.nodes.keys().filter(|nid| g.hidden.contains(&g.root_id(nid))).collect() };
        let mut nodes = vec![];
        for (id, n) in &g.nodes {
            if hidden.contains(id) {
                continue;
            }
            let mut n = n.clone();
            n.shift_remove("files"); // served per run via the API instead
            n.shift_remove("reads");
            let status = match n.get("kind").and_then(Value::as_str) {
                Some("agent") => Some(json!(agent_status(&n, now))),
                Some("session") => Some(n.get("liveStatus").filter(|v| truthy(v)).cloned().unwrap_or(json!("ended"))),
                Some("workflow") => Some(n.get("runStatus").cloned().unwrap_or(Value::Null)),
                _ => None,
            };
            if let Some(st) = status {
                n.insert("status".into(), st);
            }
            nodes.push(Value::Object(n));
        }
        let visible = |e: &Value| !hidden.contains(&s(e, "src")) && !hidden.contains(&s(e, "dst"));
        json!({
            "now": now,
            "version": g.version,
            "nodes": nodes,
            "edges": g.edges.values().map(|e| Value::Object(e.clone())).filter(|e| visible(e)).collect::<Vec<_>>(),
            "events": g.events.iter().filter(|e| e["seq"].as_u64().unwrap_or(0) > since_seq && visible(e)).cloned().collect::<Vec<_>>(),
            "seq": g.event_seq,
        })
    }
}

fn agent_status(n: &Map<String, Value>, now: f64) -> &'static str {
    if n.get("done").is_some_and(truthy) {
        return "done";
    }
    if now - n.get("lastActive").and_then(Value::as_f64).unwrap_or(0.0) < 90.0 { "running" } else { "stopped" }
}

fn text_of_blocks(content: &[Value], only_text_type: bool) -> String {
    content
        .iter()
        .filter(|b| !only_text_type || b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.as_object())
        .map(|b| b.get("text").map(value_text).unwrap_or_default())
        .collect::<Vec<_>>()
        .join(" ")
}

impl GraphState {
    pub fn node(&mut self, nid: &str, kind: &str) -> &mut Map<String, Value> {
        self.nodes.entry(nid.to_string()).or_insert_with(|| {
            let label: String = nid.chars().skip(2).take(8).collect();
            json!({"id": nid, "lastActive": 0, "turns": 0, "label": label, "kind": kind}).as_object().unwrap().clone()
        })
    }

    fn touch(&mut self, nid: &str, ts: Option<f64>) {
        if let (Some(n), Some(ts)) = (self.nodes.get_mut(nid), ts) {
            if ts > n.get("lastActive").and_then(Value::as_f64).unwrap_or(0.0) {
                n.insert("lastActive".into(), json!(ts));
            }
        }
    }

    pub fn edge(&mut self, src: &str, dst: &str, kind: &str, ts: Option<f64>, text: &str, live: bool) {
        if src == dst {
            return;
        }
        let key = format!("{src}|{dst}|{kind}");
        let e = self
            .edges
            .entry(key.clone())
            .or_insert_with(|| json!({"id": key, "src": src, "dst": dst, "kind": kind, "count": 0, "last": 0}).as_object().unwrap().clone());
        e.insert("count".into(), json!(e["count"].as_i64().unwrap_or(0) + 1));
        e.insert("last".into(), json!(e["last"].as_f64().unwrap_or(0.0).max(ts.unwrap_or(0.0))));
        self.event(kind, src, dst, ts, text, live);
    }

    pub fn event(&mut self, kind: &str, src: &str, dst: &str, ts: Option<f64>, text: &str, live: bool) {
        self.event_seq += 1;
        self.events.push(json!({"seq": self.event_seq, "kind": kind, "src": src, "dst": dst,
                                "t": ts.filter(|t| *t != 0.0).unwrap_or_else(now), "text": short(text, 140), "live": live}));
        if self.events.len() > MAX_EVENTS {
            let extra = self.events.len() - MAX_EVENTS;
            self.events.drain(..extra);
        }
    }

    /// Map a SendMessage target / peer sender to a node id.
    fn resolve(&mut self, reference: &str) -> String {
        let r = reference.trim();
        if agent_re().is_match(r) {
            let nid = format!("a:{r}");
            self.node(&nid, "agent");
            return nid;
        }
        if uuid_re().is_match(r) {
            let nid = format!("s:{r}");
            self.node(&nid, "session");
            return nid;
        }
        if let Some(nid) = self.names.get(r) {
            return nid.clone();
        }
        let nid = format!("n:{r}");
        if !self.nodes.contains_key(&nid) {
            self.node(&nid, "session").insert("label".into(), json!(r));
        }
        nid
    }

    pub fn root_id(&self, nid: &str) -> String {
        let mut seen = HashSet::new();
        let mut nid = nid.to_string();
        while let Some(n) = self.nodes.get(&nid) {
            if n.get("kind") != Some(&json!("agent")) || !seen.insert(nid.clone()) {
                break;
            }
            let next = n.get("parent").filter(|v| truthy(v)).or_else(|| n.get("session").filter(|v| truthy(v))).map(py_str);
            match next {
                Some(p) => nid = p,
                None => break,
            }
        }
        nid
    }

    fn file_node(&mut self, path: &str) -> String {
        let projects = projects_dir();
        let rel: Vec<String> = std::path::Path::new(path)
            .strip_prefix(&projects)
            .map(|r| r.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        let project = project_label(rel.first().map(String::as_str).unwrap_or(""));
        if rel.len() >= 4 && rel[2] == "subagents" {
            let base = basename(path);
            let agent_id = base.trim_start_matches("agent-").trim_end_matches(".jsonl").to_string();
            let nid = format!("a:{agent_id}");
            let session = format!("s:{}", rel[1]);
            {
                let n = self.node(&nid, "agent");
                n.insert("project".into(), json!(project));
                n.insert("session".into(), json!(session));
            }
            let meta_path = format!("{}.meta.json", path.trim_end_matches(".jsonl"));
            if let Some(meta) = read_json(&meta_path) {
                let owner = meta.get("toolUseId").and_then(Value::as_str).and_then(|t| self.tool_owner.get(t).cloned());
                let n = self.nodes.get_mut(&nid).unwrap();
                if b(&meta, "description") {
                    n.insert("label".into(), meta["description"].clone());
                }
                n.insert("agentType".into(), meta.get("agentType").cloned().unwrap_or(Value::Null));
                n.insert("background".into(), json!(meta.get("requestShape").and_then(Value::as_str) == Some("background")));
                if let Some(owner) = owner {
                    if !n.get("parent").is_some_and(truthy) {
                        n.insert("parent".into(), json!(owner));
                    }
                }
            }
            return nid;
        }
        let sid = rel.get(1).map(|r| r.trim_end_matches(".jsonl").to_string()).unwrap_or_default();
        let nid = format!("s:{sid}");
        self.node(&nid, "session").insert("project".into(), json!(project));
        nid
    }

    fn ingest_line(&mut self, nid: &str, d: &Value, live: bool) {
        let typ = d.get("type").and_then(Value::as_str).unwrap_or("");
        let ts = parse_ts(d.get("timestamp"));
        let kind = self.nodes[nid].get("kind").and_then(Value::as_str).unwrap_or("").to_string();
        {
            let n = self.nodes.get_mut(nid).unwrap();
            if typ == "ai-title" && kind == "session" && !n.get("liveName").is_some_and(truthy) && !n.get("fixedLabel").is_some_and(truthy) {
                if b(d, "aiTitle") {
                    n.insert("label".into(), d["aiTitle"].clone());
                }
                return;
            }
            if typ == "cost-state" {
                n.insert("cost".into(), d.get("totalCostUSD").cloned().unwrap_or(Value::Null));
                return;
            }
        }
        if typ != "user" && typ != "assistant" {
            return;
        }
        self.touch(nid, ts);
        {
            let n = self.nodes.get_mut(nid).unwrap();
            if kind == "session" && b(d, "cwd") && !n.get("cwd").is_some_and(truthy) {
                n.insert("cwd".into(), d["cwd"].clone()); // the project folder, used to find workflows stored there
            }
            if !n.get("started").is_some_and(truthy) {
                if let Some(ts) = ts {
                    n.insert("started".into(), json!(ts));
                }
            }
        }
        let empty = json!({});
        let msg = d.get("message").filter(|m| m.is_object()).unwrap_or(&empty);
        let content = msg.get("content");
        let origin = d.get("origin").filter(|m| m.is_object()).unwrap_or(&empty);

        if typ == "assistant" {
            let n = self.nodes.get_mut(nid).unwrap();
            n.insert("turns".into(), json!(n.get("turns").and_then(Value::as_i64).unwrap_or(0) + 1));
            let Some(Value::Array(blocks)) = content else { return };
            for blk in blocks {
                if blk.get("type").and_then(Value::as_str) != Some("tool_use") {
                    continue;
                }
                let name = s(blk, "name");
                let inp = blk.get("input").filter(|i| i.is_object()).cloned().unwrap_or(json!({}));
                let n = self.nodes.get_mut(nid).unwrap();
                n.insert("activity".into(), json!(describe_tool(blk)));
                let path = file_tool(&name).map(|_| if b(&inp, "file_path") { s(&inp, "file_path") } else { s(&inp, "notebook_path") }).filter(|p| !p.is_empty());
                if let Some(path) = path {
                    // artefacts: files this session wrote or edited
                    let files = n.entry("files").or_insert(json!({})).as_object_mut().unwrap();
                    let fe = files.entry(path).or_insert(json!({"first": file_tool(&name), "changes": 0}));
                    fe["changes"] = json!(fe["changes"].as_i64().unwrap_or(0) + 1);
                    fe["t"] = json!(ts);
                } else if name == "Read" && b(&inp, "file_path") {
                    // inputs: files this session read
                    let reads = n.entry("reads").or_insert(json!({})).as_object_mut().unwrap();
                    let r = reads.entry(s(&inp, "file_path")).or_insert(json!({"count": 0, "t": ts}));
                    r["count"] = json!(r["count"].as_i64().unwrap_or(0) + 1);
                }
                if name == "Agent" || name == "Task" {
                    self.tool_owner.insert(s(blk, "id"), nid.to_string());
                } else if name == "SendMessage" && b(&inp, "to") {
                    let dst = self.resolve(&s(&inp, "to"));
                    let text = if b(&inp, "summary") { s(&inp, "summary") } else { inp.get("message").map(value_text).unwrap_or_default() };
                    self.edge(nid, &dst, "message", ts, &text, live);
                }
            }
            return;
        }

        // user records
        if let Some(result) = d.get("toolUseResult").filter(|r| r.is_object() && b(r, "agentId")) {
            let child = format!("a:{}", s(result, "agentId"));
            {
                let c = self.node(&child, "agent");
                c.insert("parent".into(), json!(nid));
                if b(result, "description") {
                    c.insert("label".into(), result["description"].clone());
                }
            }
            self.edge(nid, &child, "spawn", ts, &s(result, "description"), live);
            if result.get("status").and_then(Value::as_str) == Some("completed") {
                // foreground agent: its result comes back in the same record
                self.nodes.get_mut(&child).unwrap().insert("done".into(), json!(true));
                let text = match result.get("content") {
                    Some(Value::Array(a)) => text_of_blocks(a, false),
                    Some(v) if truthy(v) => value_text(v),
                    _ => String::new(),
                };
                self.edge(&child, nid, "handback", ts, if text.is_empty() { "Result returned" } else { &text }, live);
            }
            return;
        }

        let okind = origin.get("kind").and_then(Value::as_str);
        let text = match content {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(a)) => text_of_blocks(a, true),
            _ => String::new(),
        };
        if okind == Some("peer") && b(origin, "from") {
            let src = self.resolve(&s(origin, "from"));
            if text.contains("[Subagent hand-back]") {
                self.nodes.get_mut(&src).unwrap().insert("done".into(), json!(true));
                let body = text.split_once("\n\n").map(|(_, rest)| rest.to_string()).unwrap_or(text.clone());
                self.edge(&src, nid, "handback", ts, &body, live);
            } else {
                self.edge(&src, nid, "message", ts, &text, live);
            }
        } else if kind == "session" && !self.nodes[nid].get("workflowStep").is_some_and(truthy) && !b(d, "isSidechain") && !b(d, "isMeta") {
            let human = okind == Some("human") || (okind.is_none() && matches!(content, Some(Value::String(c)) if !c.starts_with('<')));
            if human && !text.trim().is_empty() {
                self.touch("you", ts);
                self.edge("you", nid, "prompt", ts, &text, live);
            }
        }
    }

    fn scan_file(&mut self, path: &str, live: bool) -> bool {
        if !self.files.contains_key(path) {
            let node = self.file_node(path);
            self.files.insert(path.to_string(), FileState { offset: 0, node });
        }
        let Some(size) = file_size(path) else { return false };
        let (offset, node) = {
            let st = &self.files[path];
            (st.offset, st.node.clone())
        };
        if size <= offset {
            return false;
        }
        let mut data = vec![];
        let Ok(mut file) = std::fs::File::open(path) else { return false };
        if file.seek(SeekFrom::Start(offset)).is_err() || file.take(size - offset).read_to_end(&mut data).is_err() {
            return false;
        }
        let Some(end) = data.iter().rposition(|&c| c == b'\n') else { return false };
        self.files.get_mut(path).unwrap().offset += end as u64 + 1;
        for raw in data[..=end].split(|&c| c == b'\n') {
            if raw.is_empty() {
                continue;
            }
            if let Ok(d) = serde_json::from_slice::<Value>(raw) {
                if d.is_object() {
                    self.ingest_line(&node, &d, live);
                }
            }
        }
        true
    }

    fn scan_live_sessions(&mut self) -> bool {
        let mut changed = false;
        let mut alive: HashMap<String, Value> = HashMap::new();
        let dir = live_sessions_dir();
        for name in list_dir(&dir).into_iter().filter(|n| n.ends_with(".json")) {
            let Some(info) = read_json(&join(&dir, &name)) else { continue };
            let Some(pid) = info.get("pid").and_then(|p| py_int(p).ok()) else { continue };
            if pid <= 0 || !compat::pid_alive(pid as u32) {
                continue;
            }
            alive.insert(format!("s:{}", s(&info, "sessionId")), info);
        }
        let mut names = vec![];
        for (nid, n) in self.nodes.iter_mut() {
            if n.get("kind") != Some(&json!("session")) {
                continue;
            }
            let info = alive.get(nid);
            let status = info.map(|i| i.get("status").cloned().unwrap_or(json!("live")));
            let status = status.unwrap_or(Value::Null);
            if n.get("liveStatus").unwrap_or(&Value::Null) != &status {
                n.insert("liveStatus".into(), status);
                changed = true;
            }
            n.insert("pid".into(), info.and_then(|i| i.get("pid").and_then(|p| py_int(p).ok())).map(|p| json!(p)).unwrap_or(Value::Null));
            if let Some(i) = info.filter(|i| b(i, "name")) {
                if !n.get("fixedLabel").is_some_and(truthy) {
                    names.push((s(i, "name"), nid.clone()));
                    n.insert("liveName".into(), i["name"].clone());
                }
            }
        }
        self.names.extend(names);
        changed
    }
}
