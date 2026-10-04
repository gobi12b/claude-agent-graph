//! The workflow runner: steps run as headless `claude -p` sessions (or Gemini / Codex / Ollama, or shell
//! commands) in dependency order, in parallel where they don't depend on each other, with retries, checks,
//! the AI judge, review pauses, steering and loop-backs. Runs are saved as JSON and drawn into the live graph.

use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::util::*;
use crate::watcher::Graph;
use crate::workflows::{self, descendants, find_workflow, read_file, topo_order, validate, BASH_TIMEOUT};
use crate::{compat, providers, quality};

const MAX_HOPS: usize = 50; // guards against workflows whose directions loop forever
const CHECK_TIMEOUT: u64 = 600;
const EDITABLE: [&str; 3] = [".md", ".markdown", ".txt"];

pub fn run_dir() -> String {
    join(&config_dir(), "runs")
}

/// A running step process, so it can be stopped (or interrupted to steer it).
struct Proc {
    pid: u32,
    done: AtomicBool,
}

/// What a paused run waits on for the user's review.
#[derive(Default)]
struct Waiter {
    set: Mutex<bool>,
    cv: Condvar,
}

impl Waiter {
    fn set(&self) {
        *self.set.lock().unwrap() = true;
        self.cv.notify_all();
    }
    fn wait(&self) {
        let mut g = self.set.lock().unwrap();
        while !*g {
            g = self.cv.wait(g).unwrap();
        }
    }
}

#[derive(Clone, Default)]
struct StepResult {
    ok: bool,
    out: String,
    session: Option<String>,
    reason: String,
    notes: String,
}

#[derive(Default)]
struct State {
    runs: HashMap<String, Value>,
    procs: HashMap<String, Vec<Arc<Proc>>>,
    proc_of: HashMap<String, Arc<Proc>>, // session -> its process, so one step can be interrupted to steer it
    waits: HashMap<String, Arc<Waiter>>,
    review_locks: HashMap<String, Arc<Mutex<()>>>, // parallel steps take turns asking for review
    own: HashSet<String>,                          // runs this process is running (the others are history, or live elsewhere)
    disk: HashMap<String, Option<f64>>,            // run id -> file time last read
    steer: HashMap<String, HashMap<String, String>>,
    resume: HashMap<String, HashMap<String, (String, String)>>,
    decision: HashMap<String, (String, String)>,
    usage: HashMap<String, Value>, // session -> token usage reported by claude -p
}

pub struct Runner {
    graph: Arc<Graph>,
    notify: Box<dyn Fn() + Send + Sync>, // wakes the event streams
    st: Mutex<State>,
    log_cache: Mutex<HashMap<String, (u64, Vec<Value>)>>,
}

type Outcome = (bool, String, Option<String>, String);

impl Runner {
    pub fn new(graph: Arc<Graph>, notify: Box<dyn Fn() + Send + Sync>) -> Arc<Runner> {
        let r = Arc::new(Runner { graph, notify, st: Mutex::new(State::default()), log_cache: Mutex::new(HashMap::new()) });
        r.sync_disk();
        r
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn get_run(&self, rid: &str) -> Option<Value> {
        self.lock().runs.get(rid).cloned()
    }

    fn with_run<R: Default>(&self, rid: &str, f: impl FnOnce(&mut Value) -> R) -> R {
        self.lock().runs.get_mut(rid).map(f).unwrap_or_default()
    }

    fn with_att<R: Default>(&self, rid: &str, session: &str, f: impl FnOnce(&mut Value) -> R) -> R {
        self.with_run(rid, |run| {
            run["attempts"].as_array_mut().and_then(|a| a.iter_mut().rev().find(|a| a["session"] == session)).map(f).unwrap_or_default()
        })
    }

    fn stopped(&self, rid: &str) -> bool {
        self.with_run(rid, |r| b(r, "stop"))
    }

    fn add_cost(&self, rid: &str, cost: f64) {
        self.with_run(rid, |r| r["cost"] = json!(f(r, "cost") + cost));
    }

    /// Load saved runs, and follow runs that another process is running (`agent-graph run` in a terminal,
    /// or the app while the terminal runs). Returns true when something changed.
    pub fn sync_disk(&self) -> bool {
        let dir = run_dir();
        let mut names: Vec<String> = list_dir(&dir).into_iter().filter(|n| n.ends_with(".json")).collect();
        let skip = names.len().saturating_sub(30);
        names.drain(..skip);
        let mut changed = false;
        for name in names {
            let rid = name.trim_end_matches(".json").to_string();
            let path = join(&dir, &name);
            {
                let st = self.lock();
                if st.own.contains(&rid) {
                    continue;
                }
                let mt = mtime(&path);
                if st.disk.get(&rid) == Some(&mt) {
                    continue;
                }
            }
            let mt = mtime(&path);
            let Some(mut run) = read_json(&path).filter(|r| r.get("id").is_some() && r.get("status").is_some()) else { continue };
            let status = s(&run, "status");
            if status == "running" || status == "waiting" {
                let pid = run.get("pid").and_then(Value::as_u64).unwrap_or(0) as u32;
                if pid != 0 && pid != std::process::id() && compat::pid_alive(pid) {
                    run["external"] = json!(true); // live in a terminal: follow it here, control it there
                } else {
                    run["status"] = json!("stopped"); // its process was closed mid-run
                    run.as_object_mut().unwrap().shift_remove("external");
                }
            }
            {
                let mut st = self.lock();
                st.disk.insert(rid, mt);
                st.runs.insert(s(&run, "id"), run.clone());
            }
            self.graph_node(&run);
            self.relink(&run);
            changed = true;
        }
        changed
    }

    fn external(run: &Value, what: &str) -> Res<()> {
        let status = s(run, "status");
        if b(run, "external") && (status == "running" || status == "waiting") {
            return Err(format!("This run is running in a terminal (agent-graph run). {what}"));
        }
        Ok(())
    }

    pub fn summary(&self) -> Vec<Value> {
        let mut runs: Vec<Value> = self.lock().runs.values().cloned().collect();
        runs.sort_by(|a, b| f(b, "started").total_cmp(&f(a, "started")));
        runs.truncate(30);
        runs
    }

    /// wf: an already-validated workflow (e.g. from a file given on the command line).
    pub fn start(self: &Arc<Self>, wid: &str, wf: Option<Value>) -> Res<String> {
        let wf = match wf {
            Some(w) => w,
            None => find_workflow(wid).ok_or("Workflow not found.")?,
        };
        let wf = validate(&wf)?;
        let rid = format!("{}{}", local_time("%Y%m%d-%H%M%S-"), hex_id(4));
        let run = json!({"id": rid, "workflowId": wf["id"], "name": wf["name"], "status": "running", "started": now(),
            "ended": null, "current": null, "cost": 0.0, "attempts": [], "stop": false, "cwd": wf["cwd"],
            "plan": plan(&wf), "pid": std::process::id(), "file": s(&wf, "file")});
        {
            let mut st = self.lock();
            st.runs.insert(rid.clone(), run.clone());
            st.own.insert(rid.clone());
        }
        self.graph_node(&run);
        {
            let mut g = self.graph.lock();
            g.edge("you", &format!("w:{rid}"), "prompt", Some(now()), &format!("Started workflow “{}”", s(&wf, "name")), true);
            g.version += 1;
        }
        let me = self.clone();
        let id = rid.clone();
        std::thread::spawn(move || me.run_all(Arc::new(wf), &id, None, HashMap::new()));
        self.changed(&rid);
        Ok(rid)
    }

    /// Run one step again (or from it to the end) inside the same run, with the current saved workflow.
    /// resume: (session, message) to continue that step's conversation with the user's steering instead.
    pub fn replay(self: &Arc<Self>, rid: &str, step_id: &str, only: bool, resume: Option<(String, String)>) -> Res<Value> {
        let run = self.get_run(rid).ok_or("Run not found.")?;
        let status = s(&run, "status");
        if status == "running" || status == "waiting" {
            return err("This run is still going. Wait for it to finish, or stop it, before replaying a step.");
        }
        let mut wf = find_workflow(&s(&run, "workflowId"));
        if wf.is_none() && is_file(&s(&run, "file")) {
            // a workflow file run from the command line
            wf = Some(read_file(&s(&run, "file"), Some(&s(&run, "cwd")).filter(|c| !c.is_empty()).map(String::as_str))?.0);
        }
        let wf = validate(&wf.ok_or("The workflow was deleted, so its steps can't be replayed.")?)?;
        let steps = arr(&wf, "steps");
        let Some(step) = steps.iter().find(|st| st["id"] == step_id) else {
            return err("This step is no longer in the workflow.");
        };
        // Steps that aren't replayed keep their last good result, so the replayed ones get the same input.
        let ids: HashSet<String> = steps.iter().map(|st| s(st, "id")).collect();
        let mut redo = HashSet::from([step_id.to_string()]);
        if !only {
            redo.extend(descendants(steps, step_id));
        }
        let mut seed = HashMap::new();
        for a in arr(&run, "attempts") {
            let sid = s(a, "step");
            if s(a, "status") == "ok" && ids.contains(&sid) && !redo.contains(&sid) {
                let rev = a.get("review").cloned().unwrap_or(json!({}));
                let notes = if s(&rev, "status") == "approved" { s(&rev, "feedback") } else { String::new() };
                seed.insert(sid, StepResult { ok: true, out: s(a, "output"), session: Some(s(a, "session")), reason: String::new(), notes });
            }
        }
        {
            let mut st = self.lock();
            let run = st.runs.get_mut(rid).ok_or("Run not found.")?;
            run["replay"] = json!(run.get("replay").and_then(Value::as_i64).unwrap_or(0) + 1);
            for (k, v) in [("status", json!("running")), ("ended", Value::Null), ("stop", json!(false)), ("error", json!("")), ("plan", plan(&wf)), ("pid", json!(std::process::id()))] {
                run[k] = v;
            }
            run.as_object_mut().unwrap().shift_remove("external");
            st.own.insert(rid.into());
            st.steer.insert(rid.into(), HashMap::new());
            st.resume.insert(rid.into(), resume.iter().map(|r| (step_id.to_string(), r.clone())).collect());
        }
        {
            let mut g = self.graph.lock();
            let text = format!("{} “{}”{}", if resume.is_some() { "Steered" } else { "Replayed" }, s(step, "name"), if only { "" } else { " to the end" });
            g.edge("you", &format!("w:{rid}"), "prompt", Some(now()), &text, true);
            g.version += 1;
        }
        let me = self.clone();
        let id = rid.to_string();
        std::thread::spawn(move || me.run_all(Arc::new(wf), &id, Some(redo), seed));
        self.changed(rid);
        Ok(json!(rid))
    }

    pub fn detail(&self, rid: &str) -> Res<Value> {
        let mut run = self.get_run(rid).ok_or("Run not found.")?;
        let wf = find_workflow(&s(&run, "workflowId"));
        if run.get("cwd").is_none() {
            run["cwd"] = wf.as_ref().map(|w| w["cwd"].clone()).unwrap_or(Value::Null);
        }
        if run.get("plan").is_none() {
            // runs from before plans were recorded
            run["plan"] = json!(wf.as_ref().map(|w| arr(w, "steps")).unwrap_or(&[]).iter()
                .map(|st| json!({"id": st["id"], "name": st["name"], "agents": st.get("agents").cloned().unwrap_or(json!([])),
                                 "retries": st["retries"], "check": st["check"]}))
                .collect::<Vec<_>>());
        }
        let mut written: HashMap<String, Value> = HashMap::new();
        if let Some(atts) = run["attempts"].as_array_mut() {
            for a in atts.iter_mut() {
                let session = s(a, "session");
                let (files, agents) = self.graph.session_detail(&session);
                let mut reads = self.graph.session_reads(&session);
                for r in reads.iter_mut() {
                    // which earlier step produced this input
                    if let Some(from) = written.get(&s(r, "path")) {
                        r["from"] = from.clone();
                    }
                }
                for fl in &files {
                    written.entry(s(fl, "path")).or_insert_with(|| a["name"].clone());
                }
                a["files"] = json!(files);
                a["agents"] = json!(agents);
                a["reads"] = json!(reads);
            }
        }
        Ok(run)
    }

    /// Only files a run itself wrote or read may be read or opened.
    fn artefact(&self, rid: &str, path: &str) -> Res<String> {
        let run = self.detail(rid)?;
        let known = arr(&run, "attempts").iter().any(|a| arr(a, "files").iter().chain(arr(a, "reads")).any(|fl| fl["path"] == path));
        if !known {
            return err("That file isn't an input or artefact of this run.");
        }
        Ok(path.to_string())
    }

    pub fn read_artefact(&self, rid: &str, path: &str) -> Res<Value> {
        const LIMIT: u64 = 300_000;
        let path = self.artefact(rid, path)?;
        if !is_file(&path) {
            return Ok(json!({"path": path, "exists": false}));
        }
        let size = file_size(&path).unwrap_or(0);
        let mut data = std::fs::read(&path).map_err(|e| e.to_string())?;
        data.truncate(LIMIT as usize);
        if data[..data.len().min(8000)].contains(&0) {
            return Ok(json!({"path": path, "exists": true, "size": size, "binary": true}));
        }
        Ok(json!({"path": path, "exists": true, "size": size, "truncated": size > LIMIT, "text": String::from_utf8_lossy(&data)}))
    }

    pub fn open_artefact(&self, rid: &str, path: &str, folder: bool) -> Res<Value> {
        let path = self.artefact(rid, path)?;
        let target = if folder { dirname(&path) } else { path.clone() };
        if !exists(&target) {
            return err("File no longer exists.");
        }
        let lower = path.to_lowercase();
        if !folder && (lower.ends_with(".html") || lower.ends_with(".htm")) {
            compat::open_in_browser(&path)?;
        } else {
            compat::open_path(&target)?;
        }
        Ok(Value::Null)
    }

    /// Open the run's project folder in the IDE, or one of its files inside that folder.
    pub fn open_ide(&self, rid: &str, path: Option<&str>) -> Res<Value> {
        let run = self.detail(rid)?;
        let file = match path.filter(|p| !p.is_empty()) {
            Some(p) => Some(self.artefact(rid, p)?),
            None => None,
        };
        workflows::open_in_ide(file.as_deref(), Some(&s(&run, "cwd"))).map(Value::from)
    }

    /// Save edits to a text artefact (Markdown / plain text only).
    pub fn write_artefact(&self, rid: &str, path: &str, text: &str) -> Res<Value> {
        let path = self.artefact(rid, path)?;
        if !EDITABLE.iter().any(|e| path.to_lowercase().ends_with(e)) {
            return err("Only Markdown and text artefacts can be edited here.");
        }
        if char_len(text) > 2_000_000 {
            return err("That's too large to save from here.");
        }
        write_atomic(&path, text)?;
        self.read_artefact(rid, &path)
    }

    /// Recent transcript activity of one step attempt and its subagents, oldest first.
    pub fn log(&self, rid: &str, session: &str) -> Res<Value> {
        let run = self.detail(rid)?;
        let att = arr(&run, "attempts").iter().find(|a| a["session"] == session).ok_or("That session isn't part of this run.")?;
        if s(att, "kind") == "bash" {
            return Ok(bash_log(att));
        }
        Ok(self.entries(self.graph.session_paths(session), 400))
    }

    /// Recent transcript activity of any session (not just workflow steps) and its helpers, oldest first.
    pub fn session_log(&self, sid: &str) -> Res<Value> {
        let paths = self.graph.session_paths(sid);
        if paths.is_empty() {
            return err("No log found for that session. It may have been moved or deleted.");
        }
        Ok(self.entries(paths, 600))
    }

    fn entries(&self, paths: Vec<(String, String)>, limit: usize) -> Value {
        let mut entries = vec![];
        for (path, who) in paths {
            for mut e in self.log_entries(&path) {
                e["who"] = json!(who);
                entries.push(e);
            }
        }
        entries.sort_by(|a, b| f(a, "t").total_cmp(&f(b, "t")));
        let total = entries.len();
        let skip = total.saturating_sub(limit);
        json!({"entries": entries[skip..], "total": total})
    }

    /// Parse a transcript incrementally (only bytes appended since last time).
    fn log_entries(&self, path: &str) -> Vec<Value> {
        let mut cache = self.log_cache.lock().unwrap();
        let entry = cache.entry(path.to_string()).or_insert((0, vec![]));
        let Some(size) = file_size(path) else { return entry.1.clone() };
        if size > entry.0 {
            if let Ok(data) = std::fs::read(path) {
                let data = &data[(entry.0 as usize).min(data.len())..];
                if let Some(end) = data.iter().rposition(|&c| c == b'\n') {
                    entry.0 += end as u64 + 1;
                    for raw in data[..=end].split(|&c| c == b'\n').filter(|r| !r.is_empty()) {
                        if let Ok(d) = serde_json::from_slice::<Value>(raw) {
                            entry.1.extend(workflows::log_lines(&d));
                        }
                    }
                }
            }
        }
        entry.1.clone()
    }

    pub fn open_folder(&self, rid: &str) -> Res<Value> {
        let run = self.detail(rid)?;
        let cwd = s(&run, "cwd");
        if !is_dir(&cwd) {
            return err("The workflow folder doesn't exist.");
        }
        compat::open_path(&cwd)?;
        Ok(Value::Null)
    }

    /// The user's verdict on a step that paused for review.
    pub fn review(&self, rid: &str, decision: &str, feedback: &str) -> Res<Value> {
        if !["approve", "revise", "stop"].contains(&decision) {
            return err("Unknown decision.");
        }
        let feedback = feedback.trim();
        if decision == "revise" && feedback.is_empty() {
            return err("Say what should change.");
        }
        let wait = {
            let mut st = self.lock();
            let run = st.runs.get(rid).cloned();
            if let Some(run) = &run {
                Self::external(run, "Answer its review there.")?;
            }
            let wait = st.waits.get(rid).cloned();
            match (run, wait) {
                (Some(run), Some(wait)) if s(&run, "status") == "waiting" => {
                    st.decision.insert(rid.into(), (decision.into(), feedback.into()));
                    wait
                }
                _ => return err("This run isn't waiting for a review."),
            }
        };
        wait.set();
        Ok(Value::Null)
    }

    /// Redirect a step with the user's guidance, like steering Claude Code mid-task.
    /// Running step: interrupt it and continue its conversation with the message (no retry is used up).
    /// Step paused for review: same as "request changes". Finished run: redo the step (and optionally the
    /// steps after it), continuing its conversation with the message.
    pub fn steer(self: &Arc<Self>, rid: &str, step_id: &str, message: &str, only: bool) -> Res<Value> {
        let message = message.trim();
        if message.is_empty() {
            return err("Write what Claude should do differently.");
        }
        let (status, att, proc) = {
            let mut st = self.lock();
            let run = st.runs.get(rid).ok_or("Run not found.")?.clone();
            Self::external(&run, "Steer it after it finishes, or stop it and replay here.")?;
            let att = arr(&run, "attempts").iter().rev().find(|a| a["step"] == step_id).cloned();
            if att.as_ref().is_some_and(|a| s(a, "kind") == "bash") {
                return err("Command steps can't be steered. Edit the command, then replay the step.");
            }
            let status = s(&run, "status");
            let mut proc = None;
            if status == "waiting" {
                if !att.as_ref().is_some_and(|a| a.get("review").map(|r| s(r, "status")) == Some("pending".into())) {
                    return err("The workflow is waiting for your review of another step. Finish that review first.");
                }
            } else if status == "running" {
                let Some(a) = att.as_ref().filter(|a| s(a, "status") == "running") else {
                    return err("While the workflow runs, only the step that's working now can be steered. \
                                Wait for the run to finish (or stop it) to redo other steps.");
                };
                let p = st.proc_of.get(&s(a, "session")).cloned().filter(|p| !p.done.load(Ordering::SeqCst));
                let Some(p) = p else { return err("This step is just finishing. Steer it again once it's done.") };
                proc = Some(p);
                st.steer.entry(rid.into()).or_default().insert(step_id.into(), message.into());
                let session = s(a, "session");
                if let Some(rec) = st.runs.get_mut(rid).and_then(|r| r["attempts"].as_array_mut()).and_then(|v| v.iter_mut().rev().find(|x| x["session"] == session.as_str())) {
                    rec["steerPending"] = json!(message);
                }
            }
            (status, att, proc)
        };
        if status == "waiting" {
            return self.review(rid, "revise", message);
        }
        if status == "running" {
            compat::kill_tree(proc.unwrap().pid); // run_step sees the steer and continues the conversation
            self.changed(rid);
            return Ok(json!(rid));
        }
        let att = att.ok_or("This step hasn't run yet, so there's nothing to steer. Replay the run instead.")?;
        self.replay(rid, step_id, only, Some((s(&att, "session"), message.to_string())))
    }

    pub fn stop(&self, rid: &str) -> Res<Value> {
        let (procs, wait) = {
            let mut st = self.lock();
            let run = st.runs.get_mut(rid).filter(|r| ["running", "waiting"].contains(&s(r, "status").as_str())).ok_or("That run isn't running.")?;
            if b(run, "external") {
                // ask the terminal that runs it to stop, as if Ctrl+C was pressed there
                compat::interrupt(run["pid"].as_u64().unwrap_or(0) as u32)?;
                return Ok(Value::Null);
            }
            run["stop"] = json!(true);
            (st.procs.get(rid).cloned().unwrap_or_default(), st.waits.get(rid).cloned())
        };
        if let Some(w) = wait {
            w.set();
        }
        for p in procs.iter().filter(|p| !p.done.load(Ordering::SeqCst)) {
            compat::kill_tree(p.pid);
        }
        Ok(Value::Null)
    }

    // -- internals --

    fn graph_node(&self, run: &Value) {
        let mut g = self.graph.lock();
        let n = g.node(&format!("w:{}", s(run, "id")), "workflow");
        n.insert("label".into(), run["name"].clone());
        n.insert("runStatus".into(), run["status"].clone());
        n.insert("lastActive".into(), json!(run.get("ended").and_then(Value::as_f64).unwrap_or_else(now)));
        n.insert("started".into(), run["started"].clone());
        n.insert("cost".into(), run["cost"].clone());
        g.version += 1;
    }

    fn changed(&self, rid: &str) {
        let run = {
            let st = self.lock();
            let Some(run) = st.runs.get(rid).cloned() else { return };
            if let Err(e) = write_json(&join(&run_dir(), &format!("{rid}.json")), &run) {
                eprintln!("couldn't save run {rid}: {e}");
            }
            run
        };
        self.graph_node(&run);
        (self.notify)();
    }

    /// Run steps in dependency order, in parallel where they don't depend on each other, and go back to
    /// an earlier step when a loopBack fires. todo: the steps to run (default all); seed: earlier results.
    fn run_all(self: &Arc<Self>, wf: Arc<Value>, rid: &str, todo: Option<HashSet<String>>, seed: HashMap<String, StepResult>) {
        let steps = arr(&wf, "steps").to_vec();
        let by: HashMap<String, Value> = steps.iter().map(|st| (s(st, "id"), st.clone())).collect();
        let order = topo_order(&steps).unwrap_or_default();
        let mut todo: HashSet<String> = todo.unwrap_or_else(|| by.keys().cloned().collect());
        let mut results = seed;
        let mut status: HashMap<String, &str> = by.keys().map(|k| (k.clone(), if todo.contains(k) { "pending" } else { "done" })).collect();
        let (mut loops, mut loop_notes, mut stale, mut running): (HashMap<String, i64>, HashMap<String, String>, HashSet<String>, HashSet<String>) = Default::default();
        let (tx, rx) = mpsc::channel::<(String, StepResult)>();
        let (mut hops, mut fin, mut ended_early) = (0usize, "succeeded", false);
        let mut error = None;

        loop {
            if !self.stopped(rid) && !ended_early && fin == "succeeded" {
                for sid in &order {
                    let deps = str_list(&by[sid], "dependsOn");
                    if status[sid] != "pending" || !deps.iter().all(|d| ["done", "continued"].contains(&status[d])) {
                        continue;
                    }
                    hops += 1;
                    if hops > MAX_HOPS {
                        fin = "failed";
                        error = Some(format!("Stopped after {MAX_HOPS} step runs (the loops keep going round)."));
                        break;
                    }
                    status.insert(sid.clone(), "running");
                    let inputs: Vec<(String, StepResult)> = deps.iter().filter_map(|d| results.get(d).map(|r| (s(&by[d], "name"), r.clone()))).collect();
                    let extra = loop_notes.remove(sid).unwrap_or_default();
                    let loop_n = loops.get(&format!("@{sid}")).copied().unwrap_or(0);
                    let (me, wf, rid, sid2, tx) = (self.clone(), wf.clone(), rid.to_string(), sid.clone(), tx.clone());
                    running.insert(sid.clone());
                    std::thread::spawn(move || {
                        let res = std::panic::catch_unwind(AssertUnwindSafe(|| me.worker(&wf, &rid, &sid2, inputs, &extra, loop_n)));
                        let res = res.unwrap_or_else(|p| StepResult {
                            reason: format!("internal error: {}", p.downcast_ref::<String>().cloned().or_else(|| p.downcast_ref::<&str>().map(|x| x.to_string())).unwrap_or_default()),
                            ..Default::default()
                        });
                        let _ = tx.send((sid2, res));
                    });
                }
            }
            if let Some(e) = error.take() {
                self.with_run(rid, |r| r["error"] = json!(e));
            }
            if running.is_empty() {
                break;
            }
            let Ok((sid, res)) = rx.recv() else { break };
            running.remove(&sid);
            if stale.remove(&sid) {
                // a loop-back reset this step while it was running: run it again later
                status.insert(sid, "pending");
                continue;
            }
            let step = &by[&sid];
            results.insert(sid.clone(), res.clone());
            if self.stopped(rid) {
                status.insert(sid, "failed");
                continue;
            }
            let lb = step.get("loopBack").filter(|l| l.is_object());
            let fire = lb.is_some_and(|lb| (s(lb, "when") == "failure" && !res.ok) || (s(lb, "when") == "success" && res.ok));
            if let (true, Some(lb)) = (fire, lb) {
                let max = lb["max"].as_i64().unwrap_or(3);
                if loops.get(&sid).copied().unwrap_or(0) < max {
                    let n = loops.get(&sid).copied().unwrap_or(0) + 1;
                    loops.insert(sid.clone(), n);
                    let target = s(lb, "to");
                    let mut reset = descendants(&steps, &target);
                    reset.insert(target.clone());
                    for r in &reset {
                        if running.contains(r) {
                            stale.insert(r.clone()); // still busy in parallel: rerun it once it reports back
                        } else {
                            status.insert(r.clone(), "pending");
                        }
                    }
                    todo.extend(reset);
                    loops.insert(format!("@{target}"), n);
                    let why = if !res.ok {
                        format!("step “{}” failed: {}", s(step, "name"), res.reason)
                    } else {
                        format!("step “{}” finished and the workflow loops back for another round", s(step, "name"))
                    };
                    loop_notes.insert(
                        target.clone(),
                        format!(
                            "\n\n---\nLoop-back {n} of {max}: {why}.\nIts output:\n{}\n\n{}",
                            last_chars(&res.out, 3000),
                            if !res.ok { "Fix the cause so that step succeeds this time." } else { "Improve on the previous round." }
                        ),
                    );
                    let entry = json!({"from": sid, "to": target, "n": n, "max": max, "when": lb["when"], "t": now(), "reason": res.reason});
                    self.with_run(rid, |r| {
                        if !r.get("loops").is_some_and(Value::is_array) {
                            r["loops"] = json!([]);
                        }
                        r["loops"].as_array_mut().unwrap().push(entry);
                    });
                    self.loop_edge(rid, step, &by[&target], res.session.as_deref(), n);
                    self.changed(rid);
                    continue;
                }
            }
            if res.ok {
                status.insert(sid.clone(), "done");
                if s(step, "onSuccess") == "end" {
                    ended_early = true;
                }
            } else if s(step, "onFailure") == "continue" {
                status.insert(sid.clone(), "continued");
            } else {
                status.insert(sid.clone(), "failed");
                if fin == "succeeded" {
                    fin = "failed";
                    let after = match loops.get(&sid) {
                        Some(&n) if n > 0 => format!(" (after {n} loop-back{})", if n > 1 { "s" } else { "" }),
                        _ => String::new(),
                    };
                    let e = format!("“{}” failed: {}{after}", s(step, "name"), res.reason);
                    self.with_run(rid, |r| r["error"] = json!(e));
                    self.stop_others(rid);
                }
            }
        }
        if self.stopped(rid) && fin == "succeeded" {
            fin = "stopped";
        }
        {
            let mut st = self.lock();
            st.steer.remove(rid);
            st.resume.remove(rid);
            if let Some(r) = st.runs.get_mut(rid) {
                if fin == "failed" {
                    r["stop"] = json!(false);
                }
                r["status"] = json!(fin);
                r["ended"] = json!(now());
                r["current"] = Value::Null;
            }
        }
        self.changed(rid);
    }

    fn worker(self: &Arc<Self>, wf: &Value, rid: &str, sid: &str, inputs: Vec<(String, StepResult)>, extra: &str, loop_n: i64) -> StepResult {
        let step = arr(wf, "steps").iter().find(|st| st["id"] == sid).cloned().unwrap_or_default();
        let (prev_out, label) = if inputs.len() == 1 {
            (inputs[0].1.out.clone(), "")
        } else {
            let parts: Vec<String> = inputs.iter().filter(|(_, r)| !r.out.is_empty()).map(|(name, r)| format!("### From “{name}”\n{}", r.out)).collect();
            (parts.join("\n\n"), "Outputs from the steps this one depends on:")
        };
        let notes = inputs.iter().filter(|(_, r)| !r.notes.is_empty()).map(|(_, r)| r.notes.clone()).collect::<Vec<_>>().join("\n\n");
        let sessions: Vec<String> = inputs.iter().filter_map(|(_, r)| r.session.clone()).collect();
        let (mut ok, mut out, mut session, mut reason) = self.run_step(wf, rid, &step, &prev_out, &sessions, &notes, extra, 0, loop_n, label);
        let mut new_notes = String::new();
        if ok && b(&step, "review") && !self.stopped(rid) {
            (ok, out, session, reason, new_notes) = self.review_gate(wf, rid, &step, out, session.unwrap_or_default(), &prev_out, &sessions);
        }
        StepResult { ok, out, session, reason, notes: new_notes }
    }

    fn stop_others(&self, rid: &str) {
        let (procs, wait) = {
            let mut st = self.lock();
            if let Some(r) = st.runs.get_mut(rid) {
                r["stop"] = json!(true);
            }
            (st.procs.get(rid).cloned().unwrap_or_default(), st.waits.get(rid).cloned())
        };
        if let Some(w) = wait {
            w.set();
        }
        for p in procs.iter().filter(|p| !p.done.load(Ordering::SeqCst)) {
            compat::kill_tree(p.pid);
        }
    }

    fn loop_edge(&self, rid: &str, step: &Value, target: &Value, session: Option<&str>, n: i64) {
        let mut g = self.graph.lock();
        if let Some(session) = session {
            let text = format!("↺ “{}” loops back to “{}” (round {})", s(step, "name"), s(target, "name"), n + 1);
            g.edge(&format!("s:{session}"), &format!("w:{rid}"), "retry", Some(now()), &text, true);
        }
        g.version += 1;
    }

    /// Pause until the user approves; revise the step with their feedback as often as they ask.
    #[allow(clippy::too_many_arguments)]
    fn review_gate(self: &Arc<Self>, wf: &Value, rid: &str, step: &Value, mut out: String, mut session: String, prev_out: &str, prev_session: &[String]) -> (bool, String, Option<String>, String, String) {
        let gate = self.lock().review_locks.entry(rid.into()).or_default().clone();
        let _turn = gate.lock().unwrap_or_else(|e| e.into_inner());
        let mut revision = 0;
        loop {
            self.with_att(rid, &session, |rec| rec["review"] = json!({"status": "pending", "t": now()}));
            let wait = Arc::new(Waiter::default());
            {
                let mut st = self.lock();
                st.waits.insert(rid.into(), wait.clone());
                if let Some(r) = st.runs.get_mut(rid) {
                    r["status"] = json!("waiting");
                    if b(r, "stop") {
                        wait.set(); // stopped while this step was finishing
                    }
                }
            }
            self.changed(rid);
            compat::notify(
                &format!("Review needed: {}", s(wf, "name")),
                &format!("“{}” is done. Check its artefacts and approve or ask for changes.", s(step, "name")),
            );
            wait.wait();
            let (decision, feedback) = {
                let mut st = self.lock();
                st.waits.remove(rid);
                let (mut decision, feedback) = st.decision.remove(rid).unwrap_or(("stop".into(), String::new()));
                if let Some(r) = st.runs.get_mut(rid) {
                    if b(r, "stop") {
                        decision = "stop".into();
                    }
                    r["status"] = json!("running");
                }
                (decision, feedback)
            };
            if decision == "stop" {
                self.with_run(rid, |r| r["stop"] = json!(true));
                self.with_att(rid, &session, |rec| rec["review"] = json!({"status": "stopped", "feedback": feedback, "t": now()}));
                self.changed(rid);
                return (false, out, Some(session), "stopped by you at review".into(), String::new());
            }
            if decision == "approve" {
                self.with_att(rid, &session, |rec| rec["review"] = json!({"status": "approved", "feedback": feedback, "t": now()}));
                self.changed(rid);
                return (true, out, Some(session), String::new(), feedback);
            }
            revision += 1;
            self.with_att(rid, &session, |rec| rec["review"] = json!({"status": "changes", "feedback": feedback, "t": now()}));
            let extra = format!(
                "\n\n---\nYou already did this step once. Your previous result:\n{}\n\nThe user reviewed your result and the files you wrote, and asked for these changes:\n{feedback}\n\nApply the changes. The files from your previous attempt are already on disk: edit them rather than starting over.",
                last_chars(&out, 3000)
            );
            let (ok, o, sess, reason) = self.run_step(wf, rid, step, prev_out, prev_session, "", &extra, revision, 0, "");
            out = o;
            if !ok {
                return (false, out, sess, reason, String::new());
            }
            session = sess.unwrap_or_default();
        }
    }

    fn steer_prompt(message: &str, interrupted: bool) -> String {
        format!(
            "{} and is steering you:\n\n{message}\n\nFollow this guidance. Everything you did earlier in this conversation, and the files you wrote, \
             are still there: continue from them rather than starting over. When you're done, give your final result for this step.",
            if interrupted { "The user interrupted you while you were working on this step" } else { "The user looked at your result for this step" }
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn run_step(self: &Arc<Self>, wf: &Value, rid: &str, step: &Value, prev_out: &str, prev_session: &[String], notes: &str, extra: &str, revision: i64, loop_n: i64, prev_label: &str) -> Outcome {
        let (mut reason, mut out, mut session): (String, String, Option<String>) = Default::default();
        let step_id = s(step, "id");
        let bash = s(step, "kind") == "bash";
        // "redo with my feedback" on a finished step: continue its conversation
        let pending = self.lock().resume.get_mut(rid).and_then(|m| m.remove(&step_id));
        let (mut resume, mut steer, mut interrupted) = match pending.filter(|_| !bash) {
            Some((sess, msg)) => (Some(sess), msg, false),
            None => (None, String::new(), false),
        };
        let retries = step["retries"].as_i64().unwrap_or(0);
        let model = if b(step, "model") { s(step, "model") } else { s(wf, "model") };
        let mut attempt = 0;
        while attempt < retries + 1 {
            attempt += 1;
            if self.stopped(rid) {
                return (false, out, session, "stopped".into());
            }
            let sess = uuid::Uuid::new_v4().to_string();
            session = Some(sess.clone());
            if bash {
                let (ok, o, r) = self.run_bash(wf, rid, step, prev_out, notes, attempt, revision, loop_n, &sess, prev_session, &reason);
                (out, reason) = (o, r);
                if ok {
                    return (true, out, session, String::new());
                }
                if self.stopped(rid) {
                    return (false, out, session, "stopped".into());
                }
                continue;
            }
            let mut extra_steer = String::new();
            if resume.is_some() && !["claude", "ollama"].contains(&providers::split(&model).0.as_str()) {
                extra_steer = format!("\n\n---\n{}", Self::steer_prompt(&steer, interrupted));
                resume = None; // this tool can't continue a conversation: start over, with the guidance added
            }
            let prompt = if resume.is_some() {
                Self::steer_prompt(&steer, interrupted)
            } else {
                let mut p = s(step, "prompt") + &agent_instructions(wf, step);
                if wf.get("passOutput").map(truthy).unwrap_or(true) && !prev_out.is_empty() {
                    p += &format!("\n\n---\n{}\n{prev_out}", if prev_label.is_empty() { "Output from the previous workflow step:" } else { prev_label });
                }
                if !notes.is_empty() {
                    p += &format!("\n\n---\nNotes from the user's review of the previous step (follow them):\n{notes}");
                }
                p += extra;
                if attempt > 1 {
                    p += &format!("\n\n---\nThis is retry {} of {retries}. The previous attempt failed: {reason}\nFix the problem and complete the task.", attempt - 1);
                }
                p + &extra_steer
            };
            let replay = self.with_run(rid, |r| r.get("replay").and_then(Value::as_i64).unwrap_or(0));
            let mut rec = json!({"step": step_id, "name": step["name"], "attempt": attempt, "revision": revision, "session": sess,
                "replay": replay, "loop": loop_n, "input": last_chars(&prompt, 20000),
                "inputModel": if model.is_empty() { "default".to_string() } else { model.clone() },
                "status": "running", "started": now(), "ended": null, "cost": 0.0, "error": "", "output": ""});
            if resume.is_some() || !extra_steer.is_empty() {
                rec["steer"] = json!(steer);
                rec["continues"] = json!(resume);
            }
            self.with_run(rid, |r| {
                r["attempts"].as_array_mut().unwrap().push(rec);
                r["current"] = json!(step_id);
            });
            let linked: Vec<String> = match &resume {
                Some(r) => vec![r.clone()],
                None if revision == 0 => prev_session.to_vec(),
                None => vec![],
            };
            self.link(rid, step, &sess, attempt, &linked, revision, loop_n);
            let before = quality::snapshot(&s(wf, "cwd")); // checkpoint: lets the user see this step's diff and rewind it
            self.with_att(rid, &sess, |rec| rec["before"] = json!(before));
            self.changed(rid);

            let (ok0, o, r, mut cost) = self.invoke(wf, rid, step, &prompt, &sess, resume.as_deref());
            let (mut ok, mut r) = (ok0, r);
            out = o;
            let after = if before.is_some() { quality::snapshot(&s(wf, "cwd")) } else { None };
            let usage = self.lock().usage.remove(&sess).unwrap_or(Value::Null);
            self.with_att(rid, &sess, |rec| {
                rec["after"] = json!(after);
                rec["usage"] = usage;
            });
            let steered = self.lock().steer.get_mut(rid).and_then(|m| m.remove(&step_id));
            if let Some(msg) = steered.filter(|_| !self.stopped(rid)) {
                // the user steered it mid-step: continue with their message
                let output = last_chars(&out, 4000);
                self.with_att(rid, &sess, |rec| {
                    for (k, v) in [("status", json!("steered")), ("ended", json!(now())), ("cost", json!(cost)), ("error", json!("")), ("output", json!(output))] {
                        rec[k] = v;
                    }
                    rec.as_object_mut().unwrap().shift_remove("steerPending");
                });
                self.add_cost(rid, cost);
                self.changed(rid);
                (resume, steer, interrupted) = (Some(sess), msg, true);
                attempt -= 1; // steering doesn't use up a retry
                continue;
            }
            (resume, steer, interrupted) = (None, String::new(), false);
            if ok && b(step, "check") && !self.stopped(rid) {
                (ok, r) = self.check(wf, step);
            }
            if ok && b(step, "judge") && !self.stopped(rid) {
                let (jok, jr, jc) = self.judge(wf, rid, step, &sess, &out);
                (ok, r) = (jok, jr);
                cost += jc;
            }
            reason = r;
            let status = if ok { "ok" } else if self.stopped(rid) { "stopped" } else { "failed" };
            let (error, output) = (if ok { String::new() } else { reason.clone() }, last_chars(&out, 4000));
            self.with_att(rid, &sess, |rec| {
                for (k, v) in [("status", json!(status)), ("ended", json!(now())), ("cost", json!(cost)), ("error", json!(error)), ("output", json!(output))] {
                    rec[k] = v;
                }
            });
            self.add_cost(rid, cost);
            self.changed(rid);
            if ok {
                return (true, out, session, String::new());
            }
        }
        (false, out, session, reason)
    }

    /// Restore step labels and links for a run saved by an earlier app session.
    fn relink(&self, run: &Value) {
        let mut g = self.graph.lock();
        let w = format!("w:{}", s(run, "id"));
        let mut prev_ok: Option<String> = None;
        for a in arr(run, "attempts") {
            let nid = format!("s:{}", s(a, "session"));
            let attempt = a["attempt"].as_i64().unwrap_or(1);
            let mut label = s(a, "name");
            if b(a, "replay") {
                label += &format!(" (replay {})", py_str(&a["replay"]));
            }
            if b(a, "revision") {
                label += &format!(" (revision {})", py_str(&a["revision"]));
            }
            if attempt > 1 {
                label += &format!(" (retry {})", attempt - 1);
            }
            let n = g.node(&nid, "session");
            n.insert("label".into(), json!(label));
            n.insert("fixedLabel".into(), json!(true));
            n.insert("workflowStep".into(), json!(true));
            let started = a.get("started").and_then(Value::as_f64);
            g.edge(&w, &nid, if attempt > 1 { "retry" } else { "spawn" }, started, &s(a, "name"), false);
            if let (Some(p), 1) = (&prev_ok, attempt) {
                g.edge(p, &nid, "handback", started, &format!("Output handed to “{}”", s(a, "name")), false);
            }
            if ["ok", "failed"].contains(&s(a, "status").as_str()) {
                prev_ok = Some(nid);
            }
        }
        g.version += 1;
    }

    #[allow(clippy::too_many_arguments)]
    fn link(&self, rid: &str, step: &Value, session: &str, attempt: i64, prev: &[String], revision: i64, loop_n: i64) {
        let replay = self.with_run(rid, |r| r.get("replay").and_then(Value::as_i64).unwrap_or(0));
        let name = s(step, "name");
        let mut label = name.clone();
        if replay > 0 {
            label += &format!(" (replay {replay})");
        }
        if loop_n > 0 {
            label += &format!(" (round {})", loop_n + 1);
        }
        if revision > 0 {
            label += &format!(" (revision {revision})");
        }
        if attempt > 1 {
            label += &format!(" (retry {})", attempt - 1);
        }
        let mut g = self.graph.lock();
        let nid = format!("s:{session}");
        let n = g.node(&nid, "session");
        n.insert("label".into(), json!(label));
        n.insert("fixedLabel".into(), json!(true));
        n.insert("workflowStep".into(), json!(true));
        n.insert("lastActive".into(), json!(now()));
        let text = if attempt > 1 { format!("{name} — retry {}", attempt - 1) } else { name.clone() };
        g.edge(&format!("w:{rid}"), &nid, if attempt > 1 || revision > 0 { "retry" } else { "spawn" }, Some(now()), &text, true);
        if attempt == 1 {
            for p in prev {
                g.edge(&format!("s:{p}"), &nid, "handback", Some(now()), &format!("Output handed to “{name}”"), true);
            }
        }
        g.version += 1;
    }

    fn register(&self, rid: &str, session: Option<&str>, pid: u32) -> Arc<Proc> {
        let p = Arc::new(Proc { pid, done: AtomicBool::new(false) });
        let mut st = self.lock();
        st.procs.entry(rid.into()).or_default().push(p.clone());
        if let Some(sess) = session {
            st.proc_of.insert(sess.into(), p.clone());
        }
        if st.runs.get(rid).is_some_and(|r| b(r, "stop")) {
            compat::kill_tree(pid); // Stop was pressed while this step was still starting
        }
        p
    }

    fn unregister(&self, rid: &str, session: Option<&str>, p: &Arc<Proc>) {
        p.done.store(true, Ordering::SeqCst);
        let mut st = self.lock();
        if let Some(v) = st.procs.get_mut(rid) {
            v.retain(|x| !Arc::ptr_eq(x, p));
        }
        if let Some(sess) = session {
            st.proc_of.remove(sess);
        }
    }

    /// resume: continue that session's conversation (steering), saved under this new session id.
    fn invoke(&self, wf: &Value, rid: &str, step: &Value, prompt: &str, session: &str, resume: Option<&str>) -> (bool, String, String, f64) {
        let model = if b(step, "model") { s(step, "model") } else { s(wf, "model") };
        let (provider, name) = providers::split(&model);
        if let Err(e) = providers::check_ready(&provider, &name) {
            return (false, String::new(), e, 0.0);
        }
        let agents = step_agents(wf, step);
        let mut prompt = prompt.to_string();
        let mut last_file = None;
        let mut args: Vec<String>;
        let mut env: Vec<(String, String)> = vec![];
        if provider == "claude" || provider == "ollama" {
            // Claude Code (Ollama: pointed at the local model)
            args = vec![compat::claude_cmd(), "-p".into(), "--output-format".into(), "json".into(), "--session-id".into(), session.into()];
            if let Some(r) = resume {
                args.extend(["--resume".into(), r.into(), "--fork-session".into()]);
            }
            args.extend(["-n".into(), format!("{} · {}", s(wf, "name"), s(step, "name")), "--permission-mode".into(), s(wf, "permissionMode")]);
            if !name.is_empty() {
                args.extend(["--model".into(), name.clone()]);
            }
            if provider == "ollama" {
                env = providers::command("ollama", &name, "", "", None).map(|c| c.1).unwrap_or_default();
            }
            if b(wf, "maxBudgetUsd") && provider == "claude" {
                args.extend(["--max-budget-usd".into(), py_str(&wf["maxBudgetUsd"])]);
            }
            if !agents.is_empty() {
                let mut defs = Map::new();
                for a in &agents {
                    let mut d = Map::new();
                    d.insert("description".into(), a["description"].clone());
                    d.insert("prompt".into(), a["prompt"].clone());
                    if b(a, "tools") {
                        d.insert("tools".into(), a["tools"].clone());
                    }
                    if b(a, "model") {
                        d.insert("model".into(), a["model"].clone());
                    }
                    defs.insert(s(a, "name"), Value::Object(d));
                }
                args.extend(["--agents".into(), Value::Object(defs).to_string()]);
                if agents.len() == 1 {
                    args.extend(["--agent".into(), s(&agents[0], "name")]); // the step runs as this subagent, so it is always used
                }
            }
            let mut allowed = str_list(wf, "allowedTools");
            if !agents.is_empty() && !allowed.is_empty() && !allowed.contains(&"Agent".to_string()) {
                allowed.push("Agent".into()); // an allow-list would otherwise block delegating
            }
            if !allowed.is_empty() {
                args.extend(["--allowedTools".into(), allowed.join(",")]);
            }
            if b(wf, "disallowedTools") {
                args.extend(["--disallowedTools".into(), str_list(wf, "disallowedTools").join(",")]);
            }
        } else {
            // Gemini CLI / Codex CLI: no subagents, so the helpers' instructions go into the prompt
            if !agents.is_empty() {
                prompt = roles_text(&agents) + &prompt;
            }
            let _ = std::fs::create_dir_all(run_dir());
            let lf = join(&run_dir(), &format!("{session}.answer.txt"));
            match providers::command(&provider, &name, &s(wf, "permissionMode"), &s(wf, "cwd"), Some(&lf)) {
                Ok((a, e)) => (args, env) = (a, e),
                Err(e) => return (false, String::new(), e, 0.0),
            }
            last_file = Some(lf);
        }
        let tool = if provider == "ollama" { "claude" } else { providers::info(&provider).cli };
        let mut cmd = Command::new(&args[0]);
        cmd.args(&args[1..]).current_dir(s(wf, "cwd")).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).envs(env);
        compat::detached(&mut cmd);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return (false, String::new(), format!("couldn't start {tool}: {e}"), 0.0),
        };
        let proc = self.register(rid, Some(session), child.id());
        let result = collect(&mut child, Some(&prompt), None, |_| {});
        self.unregister(rid, Some(session), &proc);
        let Output { code, stdout, stderr } = match result {
            Ok(o) => o,
            Err(e) => return (false, String::new(), format!("{tool} failed: {e}"), 0.0),
        };
        let code = code.unwrap_or(-1);
        let tail = || last_chars((if stderr.trim().is_empty() { &stdout } else { &stderr }).trim(), 400);
        if provider == "gemini" || provider == "gpt" {
            let mut out = stdout.trim().to_string();
            if let Some(lf) = last_file.filter(|p| exists(p)) {
                // codex: just its final answer, not the whole transcript
                let answer = read_lossy(&lf).unwrap_or_default().trim().to_string();
                if !answer.is_empty() {
                    out = answer;
                }
                let _ = std::fs::remove_file(&lf);
            }
            if code != 0 {
                return (false, out, format!("{tool} exited with code {code}: {}", tail()), 0.0);
            }
            return (true, out, String::new(), 0.0); // these tools don't report a price
        }
        let Some(res) = last_json_line(&stdout) else {
            return (false, String::new(), format!("claude exited with code {code}: {}", tail()), 0.0);
        };
        let out = s(&res, "result");
        let cost = if provider == "claude" { f(&res, "total_cost_usd") } else { 0.0 }; // local models are free
        self.lock().usage.insert(session.into(), workflows::usage(&res));
        if code != 0 || b(&res, "is_error") {
            return (false, out.clone(), format!("{}: {}", s_or(&res, "subtype", "error"), last_chars(&out, 400)), cost);
        }
        if agents.len() > 1 && resume.is_none() {
            // several subagents: each one must have been used (a steer may not need them again)
            let used = workflows::agents_used(session);
            let missing: Vec<String> = agents.iter().map(|a| s(a, "name")).filter(|n| !used.contains(n)).collect();
            if !missing.is_empty() {
                return (false, out, format!("didn't use the subagent{} {}", if missing.len() > 1 { "s" } else { "" }, missing.join(", ")), cost);
            }
        }
        (true, out, String::new(), cost)
    }

    /// A shell step: runs the command with bash in the workflow folder. No Claude, so no cost.
    /// Exit code 0 = success. The previous step's output is in $STEP_INPUT; output streams to a log file.
    #[allow(clippy::too_many_arguments)]
    fn run_bash(&self, wf: &Value, rid: &str, step: &Value, prev_out: &str, notes: &str, attempt: i64, revision: i64, loop_n: i64, session: &str, prev_session: &[String], last_reason: &str) -> (bool, String, String) {
        let _ = std::fs::create_dir_all(run_dir());
        let log_path = join(&run_dir(), &format!("{rid}-{session}.log"));
        let run_cmd = s(step, "run");
        let timeout = step.get("timeout").and_then(Value::as_i64).filter(|t| *t > 0).unwrap_or(BASH_TIMEOUT);
        let replay = self.with_run(rid, |r| r.get("replay").and_then(Value::as_i64).unwrap_or(0));
        let mut input = format!("$ {run_cmd}");
        if attempt > 1 {
            input += &format!("\n\n(retry {}: the previous try failed: {last_reason})", attempt - 1);
        }
        let rec = json!({"step": step["id"], "name": step["name"], "attempt": attempt, "revision": revision, "session": session,
            "replay": replay, "loop": loop_n, "kind": "bash", "log": log_path, "input": input,
            "inputModel": "bash (no cost)", "status": "running", "started": now(), "ended": null, "cost": 0.0, "error": "", "output": ""});
        self.with_run(rid, |r| {
            r["attempts"].as_array_mut().unwrap().push(rec);
            r["current"] = step["id"].clone();
        });
        self.link(rid, step, session, attempt, if revision == 0 { prev_session } else { &[] }, revision, loop_n);
        let cwd = s(wf, "cwd");
        let before = quality::snapshot(&cwd);
        self.with_att(rid, session, |rec| rec["before"] = json!(before));
        self.changed(rid);
        let (mut ok, mut reason, mut out) = (false, String::new(), String::new());
        let spawned = (|| -> std::io::Result<Option<i32>> {
            let logf = std::fs::File::create(&log_path)?;
            let mut cmd = compat::shell_command(&run_cmd);
            cmd.current_dir(&cwd)
                .stdin(Stdio::null())
                .stdout(logf.try_clone()?)
                .stderr(logf)
                .env("STEP_INPUT", prev_out)
                .env("STEP_NOTES", notes)
                .env("WORKFLOW_NAME", s(wf, "name"))
                .env("WORKFLOW_RUN", rid)
                .env("STEP_ID", s(step, "id"))
                .env("STEP_ATTEMPT", attempt.to_string());
            compat::detached(&mut cmd);
            let mut child = cmd.spawn()?;
            let proc = self.register(rid, None, child.id());
            let code = wait_child(&mut child, Some(Duration::from_secs(timeout as u64)));
            self.unregister(rid, None, &proc);
            code
        })();
        match spawned {
            Ok(code) => {
                out = last_chars(&read_lossy(&log_path).unwrap_or_default(), 20000);
                match code {
                    None => reason = format!("`{run_cmd}` timed out after {timeout}s"),
                    Some(0) => ok = true,
                    Some(c) => reason = format!("`{run_cmd}` exited {c}:\n{}", last_chars(out.trim(), 1500)),
                }
            }
            Err(e) => reason = format!("couldn't run the command: {e}"),
        }
        let after = if before.is_some() { quality::snapshot(&cwd) } else { None };
        self.with_att(rid, session, |rec| rec["after"] = json!(after));
        if ok && b(step, "check") && !self.stopped(rid) {
            (ok, reason) = self.check(wf, step);
        }
        if ok && b(step, "judge") && !self.stopped(rid) {
            let (jok, jr, jc) = self.judge(wf, rid, step, session, &out);
            (ok, reason) = (jok, jr);
            self.with_att(rid, session, |rec| rec["cost"] = json!(jc));
            self.add_cost(rid, jc);
        }
        let status = if ok { "ok" } else if self.stopped(rid) { "stopped" } else { "failed" };
        let (error, output) = (if ok { String::new() } else { reason.clone() }, last_chars(&out, 4000));
        self.with_att(rid, session, |rec| {
            for (k, v) in [("status", json!(status)), ("ended", json!(now())), ("error", json!(error)), ("output", json!(output))] {
                rec[k] = v;
            }
        });
        self.changed(rid);
        (ok, out, reason)
    }

    /// The AI judge grades this attempt against the step's criteria. Returns (ok, reason, cost).
    fn judge(&self, wf: &Value, rid: &str, step: &Value, session: &str, out: &str) -> (bool, String, f64) {
        self.with_att(rid, session, |rec| rec["judge"] = json!({"status": "running"}));
        self.changed(rid);
        // Retries and revisions build on the files earlier attempts left, so judge everything since the step began
        // (this round of it), not just the last attempt's own changes.
        let run = self.get_run(rid).unwrap_or_default();
        let rec = arr(&run, "attempts").iter().rev().find(|a| a["session"] == session).cloned().unwrap_or_default();
        let num = |a: &Value, k: &str| a.get(k).and_then(Value::as_i64).unwrap_or(0);
        let base = arr(&run, "attempts")
            .iter()
            .find(|a| a["step"] == step["id"] && b(a, "before") && num(a, "replay") == num(&rec, "replay") && num(a, "loop") == num(&rec, "loop"))
            .map(|a| s(a, "before"))
            .or_else(|| rec.get("before").and_then(Value::as_str).map(str::to_string));
        let after = rec.get("after").and_then(Value::as_str).map(str::to_string);
        let task = if b(step, "prompt") { s(step, "prompt") } else { format!("$ {}", s(step, "run")) };
        let verdict = quality::judge(&s(step, "judge"), &task, out, &s(wf, "cwd"), base.as_deref(), after.as_deref(), &workflows::written_files(session), &s(wf, "judgeModel"), &compat::claude_cmd());
        let v = match verdict {
            Ok(v) => v,
            Err(e) => {
                self.with_att(rid, session, |rec| rec["judge"] = json!({"status": "error", "error": e}));
                return (false, format!("AI judge: {e}"), 0.0);
            }
        };
        let mut shown = v.clone();
        shown["status"] = json!(if b(&v, "pass") { "pass" } else { "fail" });
        self.with_att(rid, session, |rec| rec["judge"] = shown);
        self.changed(rid);
        let cost = f(&v, "cost");
        if b(&v, "pass") {
            return (true, String::new(), cost);
        }
        let unmet: Vec<String> = arr(&v, "criteria").iter().filter(|c| !b(c, "met")).map(|c| format!("- {}: {}", py_str(c.get("criterion").unwrap_or(&Value::Null)), py_str(c.get("evidence").unwrap_or(&Value::Null)))).collect();
        let mut reason = format!("the AI judge scored it {}/100 and it doesn't meet the acceptance criteria yet.", py_str(&v["score"]));
        if !unmet.is_empty() {
            reason += &format!("\nNot met:\n{}", unmet.join("\n"));
        }
        reason += &format!("\nWhat to fix: {}", s(&v, "feedback"));
        (false, reason, cost)
    }

    fn attempt(&self, rid: &str, session: &str) -> Res<(Value, Value)> {
        let run = self.get_run(rid).ok_or("Run not found.")?;
        let att = arr(&run, "attempts").iter().find(|a| a["session"] == session).cloned().ok_or("That step attempt isn't part of this run.")?;
        Ok((run, att))
    }

    /// The exact diff one step attempt made to the project folder (from its before/after checkpoints).
    pub fn changes(&self, rid: &str, session: &str) -> Res<Value> {
        let (run, att) = self.attempt(rid, session)?;
        if !b(&att, "before") {
            return Ok(json!({"available": false, "reason": "No checkpoints for this step: the project folder isn't a git \
                                                          repository, or the run is from before checkpoints existed."}));
        }
        if !b(&att, "after") {
            return Ok(json!({"available": false, "reason": "This step is still working. Its changes show when it finishes."}));
        }
        match quality::diff(&s(&run, "cwd"), &s(&att, "before"), &s(&att, "after")) {
            Ok(mut d) => {
                d["available"] = json!(true);
                d["rewound"] = json!(run.get("rewound").map(|r| s(r, "session")) == Some(session.to_string()));
                Ok(d)
            }
            Err(e) => Ok(json!({"available": false, "reason": format!("Couldn't read the checkpoints: {e}")})),
        }
    }

    /// Put the project folder back as it was just before this step attempt started.
    pub fn rewind(&self, rid: &str, session: &str) -> Res<Value> {
        let (run, att) = self.attempt(rid, session)?;
        if ["running", "waiting"].contains(&s(&run, "status").as_str()) {
            return err("Stop the run (or let it finish) before rewinding, so no step is changing files.");
        }
        if !b(&att, "before") {
            return err("This step has no checkpoint to go back to.");
        }
        let undo = quality::restore(&s(&run, "cwd"), &s(&att, "before")).map_err(|e| format!("Couldn't rewind: {e}"))?;
        let rewound = json!({"session": session, "name": att["name"], "undo": undo, "t": now()});
        self.with_run(rid, |r| r["rewound"] = rewound.clone());
        self.changed(rid);
        Ok(rewound)
    }

    pub fn undo_rewind(&self, rid: &str) -> Res<Value> {
        let run = self.get_run(rid).filter(|r| b(r, "rewound")).ok_or("There's no rewind to undo.")?;
        if ["running", "waiting"].contains(&s(&run, "status").as_str()) {
            return err("Stop the run before undoing the rewind.");
        }
        quality::restore(&s(&run, "cwd"), &s(&run["rewound"], "undo")).map_err(|e| format!("Couldn't undo the rewind: {e}"))?;
        self.with_run(rid, |r| r.as_object_mut().map(|o| o.shift_remove("rewound")));
        self.changed(rid);
        Ok(Value::Null)
    }

    fn check(&self, wf: &Value, step: &Value) -> (bool, String) {
        let check = s(step, "check");
        let mut cmd = compat::shell_command(&check);
        cmd.current_dir(s(wf, "cwd"));
        match run_capture(cmd, None, Some(Duration::from_secs(CHECK_TIMEOUT))) {
            Ok(Output { code: None, .. }) => (false, format!("check timed out after {CHECK_TIMEOUT}s: {check}")),
            Ok(Output { code: Some(0), .. }) => (true, String::new()),
            Ok(Output { code: Some(c), stdout, stderr }) => (false, format!("check `{check}` exited {c}:\n{}", last_chars((stdout + &stderr).trim(), 1500))),
            Err(e) => (false, format!("couldn't run the check: {e}")),
        }
    }
}

fn plan(wf: &Value) -> Value {
    json!(arr(wf, "steps").iter().map(|st| json!({"id": st["id"], "name": st["name"], "agents": st["agents"],
        "retries": st["retries"], "check": st["check"], "judge": s(st, "judge"), "review": st["review"],
        "dependsOn": st["dependsOn"], "loopBack": st["loopBack"], "onSuccess": st["onSuccess"], "onFailure": st["onFailure"],
        "kind": s_or(st, "kind", "claude"), "run": s(st, "run")})).collect::<Vec<_>>())
}

fn step_agents(wf: &Value, step: &Value) -> Vec<Value> {
    let names = str_list(step, "agents");
    arr(wf, "agents").iter().filter(|a| names.contains(&s(a, "name"))).cloned().collect()
}

fn agent_instructions(wf: &Value, step: &Value) -> String {
    let agents = step_agents(wf, step);
    if agents.len() < 2 {
        return String::new(); // a single subagent runs the whole step itself (claude --agent)
    }
    let listing: Vec<String> = agents.iter().enumerate().map(|(i, a)| format!("{}. {}: {}", i + 1, s(a, "name"), s(a, "description"))).collect();
    format!(
        "\n\n---\nYou MUST use every one of these subagents for this task, via the Agent tool \
         (subagent_type = its name). Give each the part of the task that matches its role, with the context \
         it needs. Run them in parallel (several Agent calls in one message) unless one needs another's \
         result; then go in the order listed. Do not do their parts yourself. When all have reported, \
         combine their results into your final answer.\n{}",
        listing.join("\n")
    )
}

/// Helpers, written out for tools without subagents (Gemini CLI, Codex CLI).
fn roles_text(agents: &[Value]) -> String {
    if agents.len() == 1 {
        let a = &agents[0];
        return format!("Work as this role for the whole task: {}: {}\n{}\n\n---\n", s(a, "name"), s(a, "description"), s(a, "prompt"));
    }
    let roles: Vec<String> = agents.iter().map(|a| format!("### {}: {}\n{}", s(a, "name"), s(a, "description"), s(a, "prompt"))).collect();
    format!("Do this task by working through each of these roles in turn, giving each its part of the work, then combine the results:\n\n{}\n\n---\n", roles.join("\n\n"))
}

fn bash_log(att: &Value) -> Value {
    let text = read_lossy(&s(att, "log")).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let input = s(att, "input");
    let started = att["started"].clone();
    let mut entries = vec![json!({"t": started, "kind": "input", "who": "step", "title": format!("$ {}", first_line(&input).chars().skip(2).collect::<String>()), "body": input})];
    for chunk in lines.chunks(40) {
        // one entry per chunk of output so the log stays readable
        let last = first_chars(chunk[chunk.len() - 1], 200);
        let title = if last.is_empty() { first_chars(chunk[0], 200) } else { last };
        entries.push(json!({"t": started, "kind": "text", "who": "step", "title": title, "body": chunk.join("\n")}));
    }
    if ["failed", "stopped"].contains(&s(att, "status").as_str()) && b(att, "error") {
        let e = s(att, "error");
        entries.push(json!({"t": if b(att, "ended") { att["ended"].clone() } else { started }, "kind": "error", "who": "step",
                            "title": first_chars(&first_line(&e), 200), "body": e}));
    }
    json!({"entries": entries, "total": lines.len()})
}
