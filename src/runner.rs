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
use crate::guard::{self, Guard};
use crate::{compat, providers, quality, template, worktree};

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
    resume: HashMap<String, HashMap<String, (String, String, bool)>>, // step -> (session, message, after a crash)
    reviews: HashMap<String, HashMap<String, (String, String)>>,      // step -> (output, session) still waiting for review
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

/// Where run_all starts from: results of steps already done, and the loop-back counters so far.
#[derive(Default)]
struct Seed {
    results: HashMap<String, StepResult>,
    loops: HashMap<String, i64>,
    hops: usize,
}

/// How an interrupted run picks up again (see `resume_point`).
#[derive(Default)]
struct ResumePoint {
    seed: Seed,
    todo: HashSet<String>,
    restart: HashMap<String, String>,          // step -> session of its mid-flight attempt, to continue
    reviews: HashMap<String, (String, String)>, // step -> (output, session) that was waiting for review
}

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
                    mark_interrupted(&mut run); // its process was closed mid-run: it can be resumed
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
    /// inputs: values for the workflow's inputs ({name: value}); the defaults fill in the rest.
    pub fn start(self: &Arc<Self>, wid: &str, wf: Option<Value>, inputs: &Value) -> Res<String> {
        let wf = match wf {
            Some(w) => w,
            None => find_workflow(wid).ok_or("Workflow not found.")?,
        };
        let wf = validate(&wf)?;
        let values = template::resolve_inputs(wf["inputs"].as_object().unwrap_or(&Map::new()), inputs)?;
        let rid = format!("{}{}", local_time("%Y%m%d-%H%M%S-"), hex_id(4));
        let run = json!({"id": rid, "workflowId": wf["id"], "name": wf["name"], "status": "running", "started": now(),
            "ended": null, "current": null, "cost": 0.0, "attempts": [], "stop": false, "cwd": wf["cwd"], "project": wf["cwd"],
            "inputs": values, "isolation": wf["isolation"], "plan": plan(&wf), "pid": std::process::id(), "file": s(&wf, "file"),
            "autoResume": b(&wf, "autoResume")});
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
        std::thread::spawn(move || {
            if let Some(wf) = me.prepare(wf, &id) {
                me.run_all(Arc::new(wf), &id, None, Seed::default());
            }
        });
        self.changed(&rid);
        Ok(rid)
    }

    /// Make the run's separate worktree when the workflow asks for one; steps then run there.
    /// Returns the workflow to run (its cwd moved into the worktree), or None if that failed.
    fn prepare(&self, mut wf: Value, rid: &str) -> Option<Value> {
        if s(&wf, "isolation") != "worktree" {
            return Some(wf);
        }
        self.with_run(rid, |r| r["current"] = json!("Making a separate copy of the project"));
        self.changed(rid);
        match worktree::create(&s(&wf, "cwd"), rid, &wf["worktree"]) {
            Ok(rec) => {
                wf["cwd"] = rec["folder"].clone();
                self.with_run(rid, |r| {
                    r["cwd"] = rec["folder"].clone();
                    r["worktree"] = rec;
                    r["current"] = Value::Null;
                });
                self.changed(rid);
                Some(wf)
            }
            Err(e) => {
                self.with_run(rid, |r| {
                    for (k, v) in [("status", json!("failed")), ("error", json!(e)), ("ended", json!(now())), ("current", Value::Null)] {
                        r[k] = v;
                    }
                });
                self.changed(rid);
                None
            }
        }
    }

    /// Run one step again (or from it to the end) inside the same run, with the current saved workflow.
    /// resume: (session, message) to continue that step's conversation with the user's steering instead.
    pub fn replay(self: &Arc<Self>, rid: &str, step_id: &str, only: bool, resume: Option<(String, String)>) -> Res<Value> {
        let run = self.get_run(rid).ok_or("Run not found.")?;
        let status = s(&run, "status");
        if status == "running" || status == "waiting" {
            return err("This run is still going. Wait for it to finish, or stop it, before replaying a step.");
        }
        let wf = self.saved_workflow(rid, &run, "replayed")?;
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
            // the folder moves on: undoing that rewind now would wipe out this replay's work
            run.as_object_mut().unwrap().shift_remove("rewound");
            run.as_object_mut().unwrap().shift_remove("external");
            st.own.insert(rid.into());
            st.steer.insert(rid.into(), HashMap::new());
            st.resume.insert(rid.into(), resume.iter().map(|(sess, msg)| (step_id.to_string(), (sess.clone(), msg.clone(), false))).collect());
        }
        {
            let mut g = self.graph.lock();
            let text = format!("{} “{}”{}", if resume.is_some() { "Steered" } else { "Replayed" }, s(step, "name"), if only { "" } else { " to the end" });
            g.edge("you", &format!("w:{rid}"), "prompt", Some(now()), &text, true);
            g.version += 1;
        }
        let me = self.clone();
        let id = rid.to_string();
        std::thread::spawn(move || me.run_all(Arc::new(wf), &id, Some(redo), Seed { results: seed, ..Default::default() }));
        self.changed(rid);
        Ok(json!(rid))
    }

    /// The run's workflow as saved now (or its file, for a run started from one), set to work in the run's own
    /// worktree if it has one (brought back if it was removed). what: "replayed" / "resumed", for the error.
    fn saved_workflow(&self, rid: &str, run: &Value, what: &str) -> Res<Value> {
        let mut wf = find_workflow(&s(run, "workflowId"));
        if wf.is_none() && is_file(&s(run, "file")) {
            // a workflow file run from the command line
            wf = Some(read_file(&s(run, "file"), Some(&project_of(run)).filter(|c| !c.is_empty()).map(String::as_str))?.0);
        }
        let mut wf = validate(&wf.ok_or(format!("The workflow was deleted, so its steps can't be {what}."))?)?;
        if run.get("worktree").is_some_and(Value::is_object) {
            self.ensure_worktree(rid)?;
            wf["cwd"] = run["worktree"]["folder"].clone();
        }
        Ok(wf)
    }

    /// Continue a run whose app or terminal closed mid-run, from the first step that hadn't finished. Finished
    /// steps keep their results (and their cost isn't counted again); a step that was mid-flight starts again,
    /// continuing its conversation where the tool can; a step that was waiting for review asks again.
    pub fn resume(self: &Arc<Self>, rid: &str) -> Res<Value> {
        let run = self.get_run(rid).ok_or("Run not found.")?;
        if s(&run, "status") != "interrupted" {
            return err("Only a run that was interrupted (its app or terminal closed while it ran) can be resumed.");
        }
        let wf = self.saved_workflow(rid, &run, "resumed")?;
        let point = resume_point(&run, arr(&wf, "steps"));
        {
            let mut st = self.lock();
            let run = st.runs.get_mut(rid).ok_or("Run not found.")?;
            run["resumed"] = json!(run.get("resumed").and_then(Value::as_i64).unwrap_or(0) + 1);
            for (k, v) in [("status", json!("running")), ("ended", Value::Null), ("stop", json!(false)), ("error", json!("")), ("plan", plan(&wf)), ("pid", json!(std::process::id()))] {
                run[k] = v;
            }
            run.as_object_mut().unwrap().shift_remove("external");
            st.own.insert(rid.into());
            st.steer.insert(rid.into(), HashMap::new());
            st.resume.insert(rid.into(), point.restart.iter().map(|(step, sess)| (step.clone(), (sess.clone(), String::new(), true))).collect());
            st.reviews.insert(rid.into(), point.reviews);
        }
        {
            let mut g = self.graph.lock();
            g.edge("you", &format!("w:{rid}"), "prompt", Some(now()), &format!("Resumed “{}”", s(&run, "name")), true);
            g.version += 1;
        }
        let me = self.clone();
        let id = rid.to_string();
        std::thread::spawn(move || me.run_all(Arc::new(wf), &id, Some(point.todo), point.seed));
        self.changed(rid);
        Ok(json!(rid))
    }

    /// Resume the interrupted runs of workflows with `autoResume: true` (at app start). Returns their ids.
    pub fn auto_resume(self: &Arc<Self>) -> Vec<String> {
        let ids: Vec<String> = self.lock().runs.values().filter(|r| s(r, "status") == "interrupted" && b(r, "autoResume")).map(|r| s(r, "id")).collect();
        ids.into_iter()
            .filter(|rid| match self.resume(rid) {
                Ok(_) => true,
                Err(e) => {
                    eprintln!("couldn't resume run {rid}: {e}");
                    false
                }
            })
            .collect()
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
        let mut log = if s(att, "kind") == "bash" { bash_log(att) } else { self.entries(self.graph.session_paths(session), 400) };
        if let Some(r) = guard::Redactor::from_record(att.get("redact")) {
            // Claude's own transcript isn't ours to rewrite, so mask it when it's shown
            for e in log["entries"].as_array_mut().into_iter().flatten() {
                r.redact_entry(e);
            }
        }
        Ok(log)
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
    fn run_all(self: &Arc<Self>, wf: Arc<Value>, rid: &str, todo: Option<HashSet<String>>, seed: Seed) {
        let steps = arr(&wf, "steps").to_vec();
        let by: HashMap<String, Value> = steps.iter().map(|st| (s(st, "id"), st.clone())).collect();
        let order = topo_order(&steps).unwrap_or_default();
        let mut todo: HashSet<String> = todo.unwrap_or_else(|| by.keys().cloned().collect());
        let mut results = seed.results;
        let mut status: HashMap<String, &str> = by.keys().map(|k| (k.clone(), if todo.contains(k) { "pending" } else { "done" })).collect();
        for (k, r) in &results {
            if status.get(k) == Some(&"done") && !r.ok {
                status.insert(k.clone(), "continued"); // failed, but the workflow went on past it
            }
        }
        let mut loops = seed.loops;
        let (mut loop_notes, mut stale, mut running): (HashMap<String, String>, HashSet<String>, HashSet<String>) = Default::default();
        let (tx, rx) = mpsc::channel::<(String, StepResult)>();
        let (mut hops, mut fin, mut ended_early) = (seed.hops, "succeeded", false);
        let mut error = None;

        loop {
            self.save_progress(rid, &status, &results, &loops, hops);
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
            st.reviews.remove(rid);
            if let Some(r) = st.runs.get_mut(rid) {
                r.as_object_mut().unwrap().shift_remove("resumeFrom"); // only an interrupted run needs it
                if fin == "failed" {
                    r["stop"] = json!(false);
                }
                r["status"] = json!(fin);
                r["ended"] = json!(now());
                r["current"] = Value::Null;
                if r.get("worktree").is_some_and(Value::is_object) {
                    r["finishing"] = json!(true); // delivery and tidying up follow (after_run)
                }
            }
        }
        self.changed(rid);
        self.after_run(&wf, rid);
    }

    /// Record what an interrupted run would resume from: the finished steps with their results, and the loop
    /// counters. Saved with the run file, after each step.
    fn save_progress(&self, rid: &str, status: &HashMap<String, &str>, results: &HashMap<String, StepResult>, loops: &HashMap<String, i64>, hops: usize) {
        let mut steps = Map::new();
        for (sid, r) in results {
            if ["done", "continued"].contains(&status.get(sid).copied().unwrap_or("")) {
                steps.insert(sid.clone(), json!({"ok": r.ok, "output": r.out, "session": r.session, "reason": r.reason, "notes": r.notes}));
            }
        }
        let rec = json!({"steps": steps, "loops": loops, "hops": hops});
        if self.with_run(rid, |r| (r.get("resumeFrom") != Some(&rec)).then(|| r["resumeFrom"] = rec).is_some()) {
            self.changed(rid);
        }
    }

    fn worker(self: &Arc<Self>, wf: &Value, rid: &str, sid: &str, inputs: Vec<(String, StepResult)>, extra: &str, loop_n: i64) -> StepResult {
        let step = arr(wf, "steps").iter().find(|st| st["id"] == sid).cloned().unwrap_or_default();
        let step = match self.render_step(rid, &step) {
            Ok(st) => st,
            Err(e) => return StepResult { reason: format!("couldn't fill in its template: {e}"), ..Default::default() },
        };
        let (prev_out, label) = if inputs.len() == 1 {
            (inputs[0].1.out.clone(), "")
        } else {
            let parts: Vec<String> = inputs.iter().filter(|(_, r)| !r.out.is_empty()).map(|(name, r)| format!("### From “{name}”\n{}", r.out)).collect();
            (parts.join("\n\n"), "Outputs from the steps this one depends on:")
        };
        let notes = inputs.iter().filter(|(_, r)| !r.notes.is_empty()).map(|(_, r)| r.notes.clone()).collect::<Vec<_>>().join("\n\n");
        let sessions: Vec<String> = inputs.iter().filter_map(|(_, r)| r.session.clone()).collect();
        // a step that was waiting for review when the run was interrupted: ask again rather than redo it
        let waited = self.lock().reviews.get_mut(rid).and_then(|m| m.remove(sid)).filter(|_| b(&step, "review"));
        let (mut ok, mut out, mut session, mut reason) = match waited {
            Some((out, sess)) => (true, out, Some(sess), String::new()),
            None => self.run_step(wf, rid, &step, &prev_out, &sessions, &notes, extra, 0, loop_n, label),
        };
        let mut new_notes = String::new();
        if ok && (b(&step, "review") || wants_approval(wf, &step)) && !self.stopped(rid) {
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
            let verdict = match decision.as_str() {
                "approve" => ("pass", if feedback.is_empty() { "approved by you".to_string() } else { format!("approved by you: {feedback}") }),
                "stop" => ("fail", "you stopped the workflow here".to_string()),
                _ => ("fail", format!("you asked for changes: {feedback}")),
            };
            let gate_step = s(step, "kind") == "gate";
            self.with_att(rid, &session, |rec| {
                for g in rec.get_mut("gates").and_then(Value::as_array_mut).into_iter().flatten() {
                    if g["kind"] == "approve" {
                        g["status"] = json!(verdict.0);
                        g["detail"] = json!(verdict.1);
                    }
                }
                if gate_step {
                    rec["output"] = json!(gate_summary(arr(rec, "gates")));
                }
            });
            if decision == "revise" && s(step, "kind") == "gate" {
                self.with_att(rid, &session, |rec| {
                    rec["review"] = json!({"status": "changes", "feedback": feedback, "t": now()});
                    rec["status"] = json!("failed");
                    rec["error"] = json!(format!("Sent back at the gate: {feedback}"));
                });
                self.changed(rid);
                return (false, out, Some(session), format!("at the gate “{}”, you asked for changes: {feedback}", s(step, "name")), String::new());
            }
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

    fn crash_prompt() -> String {
        "You were interrupted while working on this step: the app running you closed before you finished. \
         Everything you did earlier in this conversation, and the files you wrote, are still there: continue from them \
         rather than starting over. When you're done, give your final result for this step."
            .into()
    }

    #[allow(clippy::too_many_arguments)]
    fn run_step(self: &Arc<Self>, wf: &Value, rid: &str, step: &Value, prev_out: &str, prev_session: &[String], notes: &str, extra: &str, revision: i64, loop_n: i64, prev_label: &str) -> Outcome {
        let (mut reason, mut out, mut session): (String, String, Option<String>) = Default::default();
        let step_id = s(step, "id");
        let bash = s(step, "kind") == "bash";
        let gate = s(step, "kind") == "gate";
        // "redo with my feedback" on a finished step: continue its conversation
        let pending = self.lock().resume.get_mut(rid).and_then(|m| m.remove(&step_id));
        // after a crash: continue the conversation of the attempt that was cut off
        let (mut resume, mut steer, mut interrupted, mut crashed) = match pending.filter(|_| !bash) {
            Some((sess, msg, crash)) => (Some(sess), msg, false, crash),
            None => (None, String::new(), false, false),
        };
        let retries = step["retries"].as_i64().unwrap_or(0);
        let model = if b(step, "model") { s(step, "model") } else { s(wf, "model") };
        let guard = Guard::for_step(wf, step);
        let mut spent = 0.0; // the step's budget covers all its attempts
        let mut protected = None; // protected files as they were when this attempt (or the steered one it continues) began
        let mut attempt = 0;
        while attempt < retries + 1 {
            attempt += 1;
            if self.stopped(rid) {
                return (false, out, session, "stopped".into());
            }
            let sess = uuid::Uuid::new_v4().to_string();
            session = Some(sess.clone());
            if gate {
                // a gate step does no work: it checks what the steps before it did, and passes their output on
                let (ok, r) = self.run_gate_step(wf, rid, step, prev_out, attempt, revision, loop_n, &sess, prev_session, &reason);
                reason = r;
                if ok {
                    return (true, prev_out.to_string(), session, String::new());
                }
                if self.stopped(rid) {
                    return (false, prev_out.to_string(), session, "stopped".into());
                }
                out = prev_out.to_string();
                continue;
            }
            if bash {
                let (ok, o, r) = self.run_bash(wf, rid, step, prev_out, notes, attempt, revision, loop_n, &sess, prev_session, &reason);
                (out, reason) = (o, r);
                if ok {
                    return (true, out, session, String::new());
                }
                if self.stopped(rid) {
                    return (false, out, session, "stopped".into());
                }
                if Guard::final_failure(&reason) {
                    break;
                }
                continue;
            }
            let budget_left = guard.budget.map(|b| b - spent);
            if budget_left.is_some_and(|left| left <= 0.0) {
                reason = format!("{}: it spent ${spent:.2} of its ${:.2} budget", guard::LIMIT_HIT, guard.budget.unwrap_or(0.0));
                break;
            }
            let mut extra_steer = String::new();
            if resume.is_some() && !["claude", "ollama"].contains(&providers::split(&model).0.as_str()) {
                if !crashed {
                    extra_steer = format!("\n\n---\n{}", Self::steer_prompt(&steer, interrupted));
                }
                resume = None; // this tool can't continue a conversation: start over, with the guidance added
                crashed = false;
            }
            let prompt = if resume.is_some() && crashed {
                Self::crash_prompt()
            } else if resume.is_some() {
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
                "replay": replay, "loop": loop_n, "input": last_chars(&guard.redact(&prompt), 20000),
                "inputModel": if model.is_empty() { "default".to_string() } else { model.clone() },
                "status": "running", "started": now(), "ended": null, "cost": 0.0, "error": "", "output": ""});
            if crashed {
                rec["continues"] = json!(resume);
                rec["resumed"] = json!(true);
            } else if resume.is_some() || !extra_steer.is_empty() {
                rec["steer"] = json!(guard.redact(&steer));
                rec["continues"] = json!(resume);
            }
            if let Some(r) = guard.record() {
                rec["redact"] = r;
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
            if !interrupted || protected.is_none() {
                protected = Some(guard::fingerprint(&s(wf, "cwd"), &guard.protect));
            }

            let (ok0, o, r, mut cost) = self.invoke(wf, rid, step, &prompt, &sess, resume.as_deref(), &guard, budget_left);
            let (mut ok, mut r) = (ok0, r);
            out = o;
            spent += cost;
            let after = if before.is_some() { quality::snapshot(&s(wf, "cwd")) } else { None };
            let usage = self.lock().usage.remove(&sess).unwrap_or(Value::Null);
            self.with_att(rid, &sess, |rec| {
                rec["after"] = json!(after);
                rec["usage"] = usage;
            });
            let steered = self.lock().steer.get_mut(rid).and_then(|m| m.remove(&step_id));
            if let Some(msg) = steered.filter(|_| !self.stopped(rid)) {
                // the user steered it mid-step: continue with their message
                let output = last_chars(&guard.redact(&out), 4000);
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
            if crashed && !ok && !self.stopped(rid) {
                // the cut-off conversation couldn't be continued (e.g. its transcript is gone): start the step over
                let (error, output) = (format!("couldn't continue the interrupted conversation, so the step starts over: {}", guard.redact(&r)), last_chars(&guard.redact(&out), 4000));
                self.with_att(rid, &sess, |rec| {
                    for (k, v) in [("status", json!("failed")), ("ended", json!(now())), ("cost", json!(cost)), ("error", json!(error)), ("output", json!(output))] {
                        rec[k] = v;
                    }
                });
                self.add_cost(rid, cost);
                self.changed(rid);
                (resume, steer, interrupted, crashed) = (None, String::new(), false, false);
                attempt -= 1; // not the step's own failure, so no retry is used up
                continue;
            }
            (resume, steer, interrupted, crashed) = (None, String::new(), false, false);
            // checked even when Claude says it succeeded: the files are the evidence
            let touched = guard::violations(protected.as_ref().unwrap(), &guard::fingerprint(&s(wf, "cwd"), &guard.protect));
            if !touched.is_empty() {
                (ok, r) = (false, guard::protect_reason(&touched, before.is_some()));
                self.with_att(rid, &sess, |rec| rec["protected"] = json!(touched));
            }
            if ok && b(step, "check") && !self.stopped(rid) {
                (ok, r) = self.check(wf, step);
            }
            if ok && b(step, "judge") && !self.stopped(rid) {
                let (jok, jr, jc) = self.judge(wf, rid, step, &sess, &out);
                (ok, r) = (jok, jr);
                cost += jc;
                spent += jc;
            }
            if let Some(budget) = guard.budget.filter(|b| spent > *b + 1e-9) {
                if !Guard::final_failure(&r) {
                    (ok, r) = (false, format!("{}: it spent ${spent:.2} of its ${budget:.2} budget", guard::LIMIT_HIT));
                }
            }
            if ok && b(step, "gates") && !self.stopped(rid) {
                let (gok, gr, gc) = self.attached_gates(wf, rid, step, &sess, &out);
                (ok, r) = (gok, gr);
                cost += gc;
            }
            reason = r;
            let status = if ok { "ok" } else if self.stopped(rid) { "stopped" } else { "failed" };
            let (error, output) = (if ok { String::new() } else { guard.redact(&reason) }, last_chars(&guard.redact(&out), 4000));
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
            if Guard::final_failure(&reason) {
                break;
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
    /// guard: the step's limits and protected paths; budget_left: what remains of its budget.
    #[allow(clippy::too_many_arguments)]
    fn invoke(&self, wf: &Value, rid: &str, step: &Value, prompt: &str, session: &str, resume: Option<&str>, guard: &Guard, budget_left: Option<f64>) -> (bool, String, String, f64) {
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
            // maxBudgetUsd caps each attempt; limits.budgetUsd caps the whole step, so pass what's left of it
            let per_attempt = wf.get("maxBudgetUsd").and_then(Value::as_f64);
            let cap = match (budget_left, per_attempt) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            if let Some(cap) = cap.filter(|_| provider == "claude") {
                args.extend(["--max-budget-usd".into(), py_str(&json!(cap))]);
            }
            if let Some(n) = guard.max_turns {
                args.extend(["--max-turns".into(), n.to_string()]);
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
            let mut disallowed = str_list(wf, "disallowedTools");
            disallowed.extend(guard.disallowed_rules()); // protected paths are refused up front, and checked after
            if !disallowed.is_empty() {
                args.extend(["--disallowedTools".into(), disallowed.join(",")]);
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
        let result = collect(&mut child, Some(&prompt), guard.timeout.map(Duration::from_secs), |_| {});
        self.unregister(rid, Some(session), &proc);
        let Output { code, stdout, stderr } = match result {
            Ok(o) => o,
            Err(e) => return (false, String::new(), format!("{tool} failed: {e}"), 0.0),
        };
        if code.is_none() && !self.stopped(rid) {
            let t = guard::format_duration(guard.timeout.unwrap_or(0));
            return (false, last_chars(stdout.trim(), 4000), format!("{}: it was still running after {t}, so it was stopped", guard::LIMIT_HIT), 0.0);
        }
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
        match s(&res, "subtype").as_str() {
            "error_max_turns" => return (false, out, format!("{}: it used its {} turns", guard::LIMIT_HIT, guard.max_turns.map(|n| n.to_string()).unwrap_or("allowed".into())), cost),
            "error_max_budget_usd" => return (false, out, format!("{}: it reached its spending limit (${cost:.2} this attempt)", guard::LIMIT_HIT), cost),
            _ => {}
        }
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
        let guard = Guard::for_step(wf, step);
        let timeout = match guard.timeout {
            Some(t) => t as i64,
            None => step.get("timeout").and_then(Value::as_i64).filter(|t| *t > 0).unwrap_or(BASH_TIMEOUT),
        };
        let replay = self.with_run(rid, |r| r.get("replay").and_then(Value::as_i64).unwrap_or(0));
        let mut input = format!("$ {run_cmd}");
        if attempt > 1 {
            input += &format!("\n\n(retry {}: the previous try failed: {last_reason})", attempt - 1);
        }
        let mut rec = json!({"step": step["id"], "name": step["name"], "attempt": attempt, "revision": revision, "session": session,
            "replay": replay, "loop": loop_n, "kind": "bash", "log": log_path, "input": guard.redact(&input),
            "inputModel": "bash (no cost)", "status": "running", "started": now(), "ended": null, "cost": 0.0, "error": "", "output": ""});
        if let Some(r) = guard.record() {
            rec["redact"] = r;
        }
        self.with_run(rid, |r| {
            r["attempts"].as_array_mut().unwrap().push(rec);
            r["current"] = step["id"].clone();
        });
        self.link(rid, step, session, attempt, if revision == 0 { prev_session } else { &[] }, revision, loop_n);
        let cwd = s(wf, "cwd");
        let before = quality::snapshot(&cwd);
        self.with_att(rid, session, |rec| rec["before"] = json!(before));
        self.changed(rid);
        let protected = guard::fingerprint(&cwd, &guard.protect);
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
                .env("STEP_ATTEMPT", attempt.to_string())
                .envs(input_env(&self.get_run(rid).unwrap_or_default()));
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
                    None if guard.timeout.is_some() => reason = format!("{}: `{run_cmd}` was still running after {}, so it was stopped", guard::LIMIT_HIT, guard::format_duration(timeout as u64)),
                    None => reason = format!("`{run_cmd}` timed out after {timeout}s"),
                    Some(0) => ok = true,
                    Some(c) => reason = format!("`{run_cmd}` exited {c}:\n{}", last_chars(out.trim(), 1500)),
                }
            }
            Err(e) => reason = format!("couldn't run the command: {e}"),
        }
        if guard.redactor.is_some() {
            if let Ok(text) = read_lossy(&log_path) {
                let _ = std::fs::write(&log_path, guard.redact(&text)); // the saved log keeps no secrets
            }
        }
        let after = if before.is_some() { quality::snapshot(&cwd) } else { None };
        self.with_att(rid, session, |rec| rec["after"] = json!(after));
        let touched = guard::violations(&protected, &guard::fingerprint(&cwd, &guard.protect));
        if !touched.is_empty() && !self.stopped(rid) {
            (ok, reason) = (false, guard::protect_reason(&touched, before.is_some()));
            self.with_att(rid, session, |rec| rec["protected"] = json!(touched));
        }
        if ok && b(step, "check") && !self.stopped(rid) {
            (ok, reason) = self.check(wf, step);
        }
        if ok && b(step, "judge") && !self.stopped(rid) {
            let (jok, jr, jc) = self.judge(wf, rid, step, session, &out);
            (ok, reason) = (jok, jr);
            self.with_att(rid, session, |rec| rec["cost"] = json!(jc));
            self.add_cost(rid, jc);
        }
        if ok && b(step, "gates") && !self.stopped(rid) {
            let (gok, gr, gc) = self.attached_gates(wf, rid, step, session, &out);
            (ok, reason) = (gok, gr);
            self.with_att(rid, session, |rec| rec["cost"] = json!(f(rec, "cost") + gc));
            self.add_cost(rid, gc);
        }
        let status = if ok { "ok" } else if self.stopped(rid) { "stopped" } else { "failed" };
        let (error, output) = (if ok { String::new() } else { guard.redact(&reason) }, last_chars(&guard.redact(&out), 4000));
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
        let (base, after) = self.step_span(rid, step, session);
        let task = if b(step, "prompt") { s(step, "prompt") } else { format!("$ {}", s(step, "run")) };
        let guard = Guard::for_step(wf, step);
        let verdict = quality::judge(&s(step, "judge"), &task, out, &s(wf, "cwd"), base.as_deref(), after.as_deref(), &workflows::written_files(session), &s(wf, "judgeModel"), &compat::claude_cmd(), guard.redactor.as_ref());
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

    /// The checkpoints to judge an attempt by: from when the step began (this round of it, so retries and
    /// revisions that build on earlier attempts are included) to the end of this attempt.
    fn step_span(&self, rid: &str, step: &Value, session: &str) -> (Option<String>, Option<String>) {
        let run = self.get_run(rid).unwrap_or_default();
        let rec = arr(&run, "attempts").iter().rev().find(|a| a["session"] == session).cloned().unwrap_or_default();
        let num = |a: &Value, k: &str| a.get(k).and_then(Value::as_i64).unwrap_or(0);
        let base = arr(&run, "attempts")
            .iter()
            .find(|a| a["step"] == step["id"] && b(a, "before") && num(a, "replay") == num(&rec, "replay") && num(a, "loop") == num(&rec, "loop"))
            .map(|a| s(a, "before"))
            .or_else(|| rec.get("before").and_then(Value::as_str).map(str::to_string));
        (base, rec.get("after").and_then(Value::as_str).map(str::to_string))
    }

    /// The gates a step must pass (or a gate step checks), with any {{ … }} filled in.
    fn gate_specs(&self, wf: &Value, rid: &str, step: &Value) -> Res<Vec<Value>> {
        let mut specs = step_gate_specs(wf, step);
        if specs.iter().any(|g| template::has_template(&s(g, "run")) || template::has_template(&s(g, "judge"))) {
            let ctx = self.ctx(rid);
            for g in specs.iter_mut() {
                g["run"] = json!(template::render(&s(g, "run"), &ctx, true)?);
                g["judge"] = json!(template::render(&s(g, "judge"), &ctx, false)?);
            }
        }
        Ok(specs)
    }

    /// Gates attached to a work step, checked after it (and its own check and judge) succeeded.
    fn attached_gates(&self, wf: &Value, rid: &str, step: &Value, session: &str, out: &str) -> (bool, String, f64) {
        let specs = match self.gate_specs(wf, rid, step) {
            Ok(g) => g,
            Err(e) => return (false, format!("couldn't fill in a gate's template: {e}"), 0.0),
        };
        let (base, after) = self.step_span(rid, step, session);
        let task = if b(step, "prompt") { s(step, "prompt") } else { format!("$ {}", s(step, "run")) };
        self.eval_gates(wf, rid, session, &specs, out, &task, base.as_deref(), after.as_deref(), &workflows::written_files(session), Guard::for_step(wf, step).redactor.as_ref())
    }

    /// Check gates in order: commands first (cheap), then the AI judge, which is skipped once a command failed.
    /// Approval parts stay "pending" until the review pause. Results go to the attempt's `gates`.
    #[allow(clippy::too_many_arguments)]
    fn eval_gates(&self, wf: &Value, rid: &str, session: &str, specs: &[Value], out: &str, task: &str, base: Option<&str>, after: Option<&str>, files: &[String], redact: Option<&guard::Redactor>) -> (bool, String, f64) {
        let mut items: Vec<Value> = vec![];
        for g in specs {
            for (kind, on) in [("run", b(g, "run")), ("judge", b(g, "judge")), ("approve", b(g, "approve"))] {
                if on {
                    items.push(json!({"gate": g["name"], "kind": kind, "status": if kind == "approve" { "pending" } else { "waiting" }, "detail": ""}));
                }
            }
        }
        let set = |items: &Vec<Value>| {
            let items = items.clone();
            self.with_att(rid, session, |rec| rec["gates"] = json!(items));
            self.changed(rid);
        };
        set(&items);
        let (mut fails, mut cost) = (vec![], 0.0);
        let run_env = input_env(&self.get_run(rid).unwrap_or_default());
        let commands: Vec<(usize, &Value)> =
            specs.iter().filter_map(|g| items.iter().position(|it| it["gate"] == g["name"] && it["kind"] == "run").map(|i| (i, g))).collect();
        for (i, g) in commands {
            if self.stopped(rid) {
                break;
            }
            items[i]["status"] = json!("running");
            set(&items);
            let cmd_text = s(g, "run");
            let timeout = g["timeout"].as_i64().unwrap_or(workflows::GATE_TIMEOUT).max(1) as u64;
            let mut cmd = compat::shell_command(&cmd_text);
            cmd.current_dir(s(wf, "cwd")).env("STEP_INPUT", out).envs(run_env.clone());
            let (status, detail) = match run_capture(cmd, None, Some(Duration::from_secs(timeout))) {
                Ok(Output { code: Some(0), stdout, .. }) => ("pass", last_chars(stdout.trim(), 600)),
                Ok(Output { code: None, .. }) => ("fail", format!("`{cmd_text}` timed out after {timeout}s")),
                Ok(Output { code: Some(c), stdout, stderr }) => ("fail", format!("`{cmd_text}` exited {c}:\n{}", last_chars((stdout + &stderr).trim(), 1500))),
                Err(e) => ("fail", format!("couldn't run `{cmd_text}`: {e}")),
            };
            if status == "fail" {
                fails.push(format!("gate “{}”: {detail}", s(g, "name")));
            }
            items[i]["status"] = json!(status);
            items[i]["detail"] = json!(detail);
            set(&items);
        }
        for i in 0..items.len() {
            if items[i]["kind"] != "judge" {
                continue;
            }
            if !fails.is_empty() || self.stopped(rid) {
                items[i]["status"] = json!("skipped");
                items[i]["detail"] = json!("not checked: a command gate failed first");
                continue;
            }
            let g = specs.iter().find(|g| g["name"] == items[i]["gate"]).cloned().unwrap_or_default();
            items[i]["status"] = json!("running");
            set(&items);
            match quality::judge(&s(&g, "judge"), task, out, &s(wf, "cwd"), base, after, files, &s(wf, "judgeModel"), &compat::claude_cmd(), redact) {
                Ok(v) => {
                    cost += f(&v, "cost");
                    let pass = b(&v, "pass");
                    if !pass {
                        let unmet: Vec<String> = arr(&v, "criteria").iter().filter(|c| !b(c, "met")).map(|c| format!("- {}: {}", s(c, "criterion"), s(c, "evidence"))).collect();
                        fails.push(format!(
                            "gate “{}”: the AI judge scored it {}/100.{}\nWhat to fix: {}",
                            s(&g, "name"),
                            py_str(&v["score"]),
                            if unmet.is_empty() { String::new() } else { format!("\nNot met:\n{}", unmet.join("\n")) },
                            s(&v, "feedback")
                        ));
                    }
                    items[i]["status"] = json!(if pass { "pass" } else { "fail" });
                    items[i]["detail"] = json!(s(&v, "feedback"));
                    items[i]["verdict"] = v;
                }
                Err(e) => {
                    fails.push(format!("gate “{}”: AI judge: {e}", s(&g, "name")));
                    items[i]["status"] = json!("error");
                    items[i]["detail"] = json!(e);
                }
            }
            set(&items);
        }
        (fails.is_empty(), fails.join("\n\n"), cost)
    }

    /// A gate step: checks everything the steps before it changed, does no work of its own.
    #[allow(clippy::too_many_arguments)]
    fn run_gate_step(&self, wf: &Value, rid: &str, step: &Value, prev_out: &str, attempt: i64, revision: i64, loop_n: i64, session: &str, prev_session: &[String], last_reason: &str) -> (bool, String) {
        let replay = self.with_run(rid, |r| r.get("replay").and_then(Value::as_i64).unwrap_or(0));
        let specs = self.gate_specs(wf, rid, step);
        let names: Vec<String> = specs.as_ref().map(|v| v.iter().map(|g| s(g, "name")).collect()).unwrap_or_default();
        let mut input = format!("Gate: checks {}", names.join(", "));
        if attempt > 1 {
            input += &format!("\n\n(retry {}: the previous check failed: {last_reason})", attempt - 1);
        }
        let rec = json!({"step": step["id"], "name": step["name"], "attempt": attempt, "revision": revision, "session": session,
            "replay": replay, "loop": loop_n, "kind": "gate", "input": input, "inputModel": "gate (checks only)",
            "status": "running", "started": now(), "ended": null, "cost": 0.0, "error": "", "output": ""});
        self.with_run(rid, |r| {
            r["attempts"].as_array_mut().unwrap().push(rec);
            r["current"] = step["id"].clone();
        });
        self.link(rid, step, session, attempt, if revision == 0 { prev_session } else { &[] }, revision, loop_n);
        // what to judge: everything changed since the first of the steps before this gate started (this replay)
        let cwd = s(wf, "cwd");
        let after = quality::snapshot(&cwd);
        let upstream = upstream_steps(wf, &s(step, "id"));
        let run = self.get_run(rid).unwrap_or_default();
        let earlier: Vec<&Value> = arr(&run, "attempts")
            .iter()
            .filter(|a| upstream.contains(&s(a, "step")) && a.get("replay").and_then(Value::as_i64).unwrap_or(0) == replay)
            .collect();
        let base = earlier.iter().find(|a| b(a, "before")).map(|a| s(a, "before")).or_else(|| after.clone());
        let mut files: Vec<String> = vec![];
        for a in &earlier {
            for p in workflows::written_files(&s(a, "session")) {
                if !files.contains(&p) {
                    files.push(p);
                }
            }
        }
        self.with_att(rid, session, |rec| {
            rec["before"] = json!(base.as_ref().filter(|_| after.is_some()));
            rec["after"] = json!(after);
        });
        self.changed(rid);
        let names_done: Vec<String> = upstream_names(wf, &upstream);
        let task = format!(
            "The work done so far by the workflow “{}” ({}). This gate checks it before the workflow goes on.",
            s(wf, "name"),
            if names_done.is_empty() { "no earlier steps".to_string() } else { format!("steps: {}", names_done.join(", ")) }
        );
        let (ok, reason, cost) = match &specs {
            Ok(specs) => self.eval_gates(wf, rid, session, specs, prev_out, &task, base.as_deref(), after.as_deref(), &files, Guard::for_step(wf, step).redactor.as_ref()),
            Err(e) => (false, format!("couldn't fill in a gate's template: {e}"), 0.0),
        };
        let summary = self
            .get_run(rid)
            .and_then(|r| arr(&r, "attempts").iter().rev().find(|a| a["session"] == session).map(|a| gate_summary(arr(a, "gates"))))
            .unwrap_or_default();
        let status = if ok { "ok" } else if self.stopped(rid) { "stopped" } else { "failed" };
        let error = if ok { String::new() } else { reason.clone() };
        self.with_att(rid, session, |rec| {
            for (k, v) in [("status", json!(status)), ("ended", json!(now())), ("cost", json!(cost)), ("error", json!(error)), ("output", json!(summary))] {
                rec[k] = v;
            }
        });
        self.add_cost(rid, cost);
        self.changed(rid);
        (ok, reason)
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
        match quality::diff(&repo_of(&run), &s(&att, "before"), &s(&att, "after")) {
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
        if run.get("worktree").is_some_and(Value::is_object) {
            self.ensure_worktree(rid)?;
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
        if run.get("worktree").is_some_and(Value::is_object) {
            self.ensure_worktree(rid)?;
        }
        quality::restore(&s(&run, "cwd"), &s(&run["rewound"], "undo")).map_err(|e| format!("Couldn't undo the rewind: {e}"))?;
        self.with_run(rid, |r| r.as_object_mut().map(|o| o.shift_remove("rewound")));
        self.changed(rid);
        Ok(Value::Null)
    }

    // -- inputs, worktrees and delivery --

    /// What templates can use: the run's inputs, its steps' latest results and facts about the run.
    fn ctx(&self, rid: &str) -> Value {
        let run = self.get_run(rid).unwrap_or_default();
        let mut steps = Map::new();
        for a in arr(&run, "attempts").iter().filter(|a| s(a, "status") == "ok") {
            steps.insert(s(a, "step"), json!({"output": a["output"]}));
        }
        json!({"inputs": run.get("inputs").cloned().unwrap_or(json!({})), "steps": steps,
               "run": {"id": rid, "folder": run["cwd"], "project": project_of(&run), "workflow": run["name"]}})
    }

    /// The step with its templates filled in (commands and checks with each value shell-quoted).
    fn render_step(&self, rid: &str, step: &Value) -> Res<Value> {
        let fields = [("prompt", false), ("judge", false), ("run", true), ("check", true)];
        if !fields.iter().any(|(k, _)| template::has_template(&s(step, k))) {
            return Ok(step.clone());
        }
        let ctx = self.ctx(rid);
        let mut out = step.clone();
        for (k, shell) in fields {
            if step.get(k).is_some() {
                out[k] = json!(template::render(&s(step, k), &ctx, shell)?);
            }
        }
        Ok(out)
    }

    /// The run's files as they are now (or as it left them, if its worktree is gone).
    fn final_tree(&self, rid: &str) -> Option<String> {
        let run = self.get_run(rid)?;
        let wt = run.get("worktree").filter(|w| w.is_object())?;
        if is_dir(&s(wt, "folder")) {
            if let Some(t) = quality::snapshot(&s(wt, "folder")) {
                return Some(t);
            }
        }
        if b(wt, "finalTree") {
            return Some(s(wt, "finalTree"));
        }
        arr(&run, "attempts").iter().rev().find(|a| b(a, "after")).map(|a| s(a, "after")).or_else(|| Some(s(wt, "baseTree")))
    }

    /// Bring back a run's removed worktree with the files it left.
    fn ensure_worktree(&self, rid: &str) -> Res<()> {
        let run = self.get_run(rid).ok_or("Run not found.")?;
        let wt = run.get("worktree").filter(|w| w.is_object()).ok_or("This run didn't use a separate copy.")?;
        if is_dir(&s(wt, "folder")) {
            return Ok(());
        }
        let tree = self.final_tree(rid).ok_or("This run has no saved files to bring back.")?;
        worktree::recreate(&project_of(&run), wt, &tree)?;
        self.with_run(rid, |r| {
            r["worktree"].as_object_mut().map(|o| o.shift_remove("removed"));
            r["cwd"] = r["worktree"]["folder"].clone();
        });
        Ok(())
    }

    /// When a run ends: remember its final files, deliver them if the workflow says so, and tidy up its worktree.
    fn after_run(&self, wf: &Value, rid: &str) {
        let Some(run) = self.get_run(rid) else { return };
        let Some(wt) = run.get("worktree").filter(|w| w.is_object()).cloned() else { return };
        if let Some(t) = self.final_tree(rid) {
            self.with_run(rid, |r| r["worktree"]["finalTree"] = json!(t));
        }
        let status = s(&run, "status");
        let dl = &wf["deliver"];
        if dl.is_object() && (status == "succeeded" || (s(dl, "when") == "always" && status == "failed")) {
            self.with_run(rid, |r| r["current"] = json!("Making the branch"));
            self.changed(rid);
            if let Err(e) = self.deliver(rid, dl) {
                self.with_run(rid, |r| r["delivery"] = json!({"error": e, "t": now()}));
            }
            self.with_run(rid, |r| r["current"] = Value::Null);
        }
        let keep = s(&wt, "keep");
        if (keep == "never" || (keep == "onFailure" && status == "succeeded")) && worktree::remove(&project_of(&run), &wt).is_ok() {
            self.with_run(rid, |r| r["worktree"]["removed"] = json!(true));
        }
        self.with_run(rid, |r| r.as_object_mut().map(|o| o.shift_remove("finishing")));
        self.changed(rid);
    }

    /// Turn the run's changes into a branch (one commit per step, or one in all), and optionally push it
    /// and open a pull request. Delivering again (after a replay) moves the same branch and keeps its PR.
    fn deliver(&self, rid: &str, dl: &Value) -> Res<Value> {
        let run = self.get_run(rid).ok_or("Run not found.")?;
        let wt = run.get("worktree").filter(|w| w.is_object()).ok_or("Only runs in a separate copy (isolation: worktree) can be delivered as a branch.")?;
        let project = project_of(&run);
        let tree = self.final_tree(rid).ok_or("Couldn't read the run's files.")?;
        let ctx = self.ctx(rid);
        let pr = dl["pr"].clone();
        let title = [s(&pr, "title"), s(dl, "message")]
            .iter()
            .find(|t| !t.trim().is_empty())
            .map(|t| template::render(t, &ctx, false))
            .transpose()?
            .unwrap_or_else(|| s(&run, "name"));
        let title = first_line(title.trim());
        let footer = format!("\n\nMade by Claude Agent Graph, workflow “{}”, run {rid}.", s(&run, "name"));
        let mut trees: Vec<(String, String)> = vec![];
        if s(dl, "commit") != "squash" {
            let mut done: Vec<&Value> = arr(&run, "attempts").iter().filter(|a| s(a, "status") == "ok" && b(a, "after")).collect();
            done.sort_by(|x, y| f(x, "ended").total_cmp(&f(y, "ended")));
            trees.extend(done.iter().map(|a| (s(a, "after"), format!("{}{footer}", s(a, "name")))));
            trees.push((tree.clone(), format!("Final changes{footer}"))); // anything after the last step (e.g. a rewind)
        } else {
            trees.push((tree.clone(), format!("{title}{footer}")));
        }
        let commits = worktree::commit_chain(&project, wt, &trees)?;
        let Some(head) = commits.last() else {
            return err("The run didn't change any files, so there's no branch to make.");
        };
        let old = run.get("delivery").filter(|d| b(d, "branch")).cloned();
        let branch = match &old {
            Some(d) => worktree::branch_name(&project, &s(d, "branch"), true)?,
            None => worktree::branch_name(&project, &template::render(&s(dl, "branch"), &ctx, false)?, false)?,
        };
        worktree::set_branch(&project, &branch, head)?;
        let mut rec = json!({"branch": branch, "commit": head, "commits": commits.len(), "pushed": false, "pr": old.as_ref().map(|d| d["pr"].clone()).unwrap_or(Value::Null), "t": now(), "error": ""});
        self.with_run(rid, |r| r["delivery"] = rec.clone());
        if b(dl, "push") {
            worktree::push(&project, &branch, old.is_some_and(|d| b(&d, "pushed")))?;
            rec["pushed"] = json!(true);
            self.with_run(rid, |r| r["delivery"] = rec.clone());
        }
        if pr.is_object() && !b(&rec, "pr") {
            let body = if s(&pr, "body") == "summary" || s(&pr, "body").is_empty() { summary(&self.get_run(rid).unwrap_or_default()) } else { template::render(&s(&pr, "body"), &ctx, false)? };
            rec["pr"] = json!(worktree::open_pr(&project, &branch, &title, &body, b(&pr, "draft"), &s(&pr, "base"))?);
            self.with_run(rid, |r| r["delivery"] = rec.clone());
        }
        Ok(rec)
    }

    fn finished_run(&self, rid: &str) -> Res<Value> {
        let run = self.get_run(rid).ok_or("Run not found.")?;
        Self::external(&run, "Wait for it to finish.")?;
        if ["running", "waiting"].contains(&s(&run, "status").as_str()) {
            return err("Wait for the run to finish (or stop it) first.");
        }
        if !run.get("worktree").is_some_and(Value::is_object) {
            return err("This run worked in your folder directly, not in a separate copy.");
        }
        Ok(run)
    }

    /// "Create branch" / "Open pull request" from the app, for any finished run in a separate copy.
    pub fn deliver_now(&self, rid: &str, pr: bool) -> Res<Value> {
        let run = self.finished_run(rid)?;
        let wf = find_workflow(&s(&run, "workflowId"));
        let mut dl = wf.map(|w| w["deliver"].clone()).filter(Value::is_object).unwrap_or_else(workflows::deliver_defaults);
        if pr && !dl["pr"].is_object() {
            dl["pr"] = workflows::pr_defaults();
        }
        if pr {
            dl["push"] = json!(true);
        }
        let res = self.deliver(rid, &dl);
        if let Err(e) = &res {
            self.with_run(rid, |r| {
                if !r.get("delivery").is_some_and(Value::is_object) {
                    r["delivery"] = json!({});
                }
                r["delivery"]["error"] = json!(e);
            });
        }
        self.changed(rid);
        res
    }

    /// Copy the run's changes into the user's own folder (undoable).
    pub fn apply(&self, rid: &str) -> Res<Value> {
        let run = self.finished_run(rid)?;
        if b(&run, "applied") {
            return err("This run's changes are already in your folder.");
        }
        let tree = self.final_tree(rid).ok_or("Couldn't read the run's files.")?;
        let undo = worktree::apply(&project_of(&run), &s(&run["worktree"], "baseTree"), &tree)?;
        let rec = json!({"undo": undo, "t": now()});
        self.with_run(rid, |r| r["applied"] = rec.clone());
        self.changed(rid);
        Ok(rec)
    }

    pub fn unapply(&self, rid: &str) -> Res<Value> {
        let run = self.finished_run(rid)?;
        let undo = run.get("applied").map(|a| s(a, "undo")).filter(|u| !u.is_empty()).ok_or("There's nothing to undo.")?;
        quality::restore(&project_of(&run), &undo).map_err(|e| format!("Couldn't undo: {e}"))?;
        self.with_run(rid, |r| r.as_object_mut().map(|o| o.shift_remove("applied")));
        self.changed(rid);
        Ok(Value::Null)
    }

    /// Remove the run's worktree. Its changes stay available (apply, branch) until git cleans up old objects.
    pub fn discard(&self, rid: &str) -> Res<Value> {
        let run = self.finished_run(rid)?;
        if let Some(t) = self.final_tree(rid) {
            self.with_run(rid, |r| r["worktree"]["finalTree"] = json!(t));
        }
        worktree::remove(&project_of(&run), &run["worktree"])?;
        self.with_run(rid, |r| r["worktree"]["removed"] = json!(true));
        self.changed(rid);
        Ok(Value::Null)
    }

    /// Remove the worktrees of every finished run. Returns how many were removed.
    pub fn clean(&self) -> usize {
        let ids: Vec<String> = self.lock().runs.values()
            .filter(|r| r.get("worktree").is_some_and(|w| w.is_object() && !b(w, "removed") && is_dir(&s(w, "path"))))
            .filter(|r| !["running", "waiting"].contains(&s(r, "status").as_str()))
            .map(|r| s(r, "id"))
            .collect();
        ids.iter().filter(|rid| self.discard(rid).is_ok()).count()
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

/// A run whose process is gone mid-run: it can be resumed. Attempts that were working are marked interrupted.
fn mark_interrupted(run: &mut Value) {
    run["status"] = json!("interrupted");
    run.as_object_mut().unwrap().shift_remove("external");
    for a in run["attempts"].as_array_mut().into_iter().flatten() {
        if s(a, "status") == "running" {
            a["status"] = json!("interrupted");
            a["error"] = json!("the app or terminal running it closed");
        }
    }
}

/// Where an interrupted run continues: the steps it finished (from its `resumeFrom` record, or for runs saved
/// before that existed, each step's last successful attempt) are seeded and not run again; every other step of
/// the workflow runs. A Claude step that was mid-flight continues that session; one waiting for review asks again.
fn resume_point(run: &Value, steps: &[Value]) -> ResumePoint {
    let ids: HashSet<String> = steps.iter().map(|st| s(st, "id")).collect();
    let mut p = ResumePoint::default();
    let last = |sid: &str| arr(run, "attempts").iter().rev().find(|a| s(a, "step") == sid).cloned();
    if let Some(from) = run.get("resumeFrom").filter(|r| r.is_object()) {
        for (sid, r) in from["steps"].as_object().into_iter().flatten().filter(|(k, _)| ids.contains(*k)) {
            let session = r.get("session").and_then(Value::as_str).map(str::to_string);
            p.seed.results.insert(sid.clone(), StepResult { ok: b(r, "ok"), out: s(r, "output"), session, reason: s(r, "reason"), notes: s(r, "notes") });
        }
        p.seed.loops = from["loops"].as_object().into_iter().flatten().map(|(k, v)| (k.clone(), v.as_i64().unwrap_or(0))).collect();
        p.seed.hops = from["hops"].as_u64().unwrap_or(0) as usize;
    } else {
        for sid in &ids {
            let Some(a) = last(sid).filter(|a| s(a, "status") == "ok") else { continue };
            let rev = a.get("review").cloned().unwrap_or(json!({}));
            if b(&rev, "status") && s(&rev, "status") != "approved" {
                continue; // still to be reviewed (handled below)
            }
            let notes = if b(&rev, "status") { s(&rev, "feedback") } else { String::new() };
            p.seed.results.insert(sid.clone(), StepResult { ok: true, out: s(&a, "output"), session: Some(s(&a, "session")), reason: String::new(), notes });
        }
    }
    p.todo = ids.iter().filter(|sid| !p.seed.results.contains_key(*sid)).cloned().collect();
    for sid in &p.todo {
        let Some(a) = last(sid) else { continue };
        let status = s(&a, "status");
        if ["running", "interrupted"].contains(&status.as_str()) && s(&a, "kind") != "bash" && b(&a, "session") {
            p.restart.insert(sid.clone(), s(&a, "session"));
        } else if status == "ok" && a.get("review").map(|r| s(r, "status")) == Some("pending".into()) {
            p.reviews.insert(sid.clone(), (s(&a, "output"), s(&a, "session")));
        }
    }
    p
}

/// The user's own project folder (a run in a worktree works elsewhere).
fn project_of(run: &Value) -> String {
    if b(run, "project") { s(run, "project") } else { s(run, "cwd") }
}

/// A folder inside the run's repository that still exists, for reading its checkpoints.
fn repo_of(run: &Value) -> String {
    let cwd = s(run, "cwd");
    if is_dir(&cwd) { cwd } else { project_of(run) }
}

/// Run inputs as environment variables for shell steps: INPUT_BUG, INPUT_SEVERITY…
fn input_env(run: &Value) -> Vec<(String, String)> {
    run.get("inputs").and_then(Value::as_object).map(|o| o.iter().map(|(k, v)| (format!("INPUT_{}", k.to_uppercase()), template::as_text(v))).collect()).unwrap_or_default()
}

/// The pull request description: inputs, each step's result and judge verdict, and the cost.
fn summary(run: &Value) -> String {
    let mut out = format!("Made by Claude Agent Graph: workflow **{}**, run `{}`.\n", s(run, "name"), s(run, "id"));
    if let Some(inputs) = run.get("inputs").and_then(Value::as_object).filter(|o| !o.is_empty()) {
        out += "\n**Inputs**\n";
        for (k, v) in inputs {
            out += &format!("- {k}: {}\n", first_chars(&template::as_text(v).replace('\n', " "), 300));
        }
    }
    out += "\n| Step | Result | AI judge | Cost |\n|---|---|---|---|\n";
    let mut last: Vec<&Value> = vec![];
    for a in arr(run, "attempts") {
        last.retain(|x| x["step"] != a["step"]);
        last.push(a);
    }
    for a in last {
        let j = a.get("judge").cloned().unwrap_or(json!({}));
        let judge = if b(&j, "score") { format!("{}/100", py_str(&j["score"])) } else { "–".into() };
        let result = match s(a, "status").as_str() { "ok" => "✅ done", "failed" => "❌ failed", other => other }.to_string();
        out += &format!("| {} | {result} | {judge} | ${:.2} |\n", s(a, "name").replace('|', "/"), f(a, "cost"));
    }
    out += &format!("\nTotal cost: ${:.2}\n", f(run, "cost"));
    out
}

fn plan(wf: &Value) -> Value {
    json!(arr(wf, "steps").iter().map(|st| json!({"id": st["id"], "name": st["name"], "agents": st["agents"],
        "retries": st["retries"], "check": st["check"], "judge": s(st, "judge"), "review": st["review"],
        "dependsOn": st["dependsOn"], "loopBack": st["loopBack"], "onSuccess": st["onSuccess"], "onFailure": st["onFailure"],
        "kind": s_or(st, "kind", "claude"), "run": s(st, "run"), "gates": st.get("gates").cloned().unwrap_or(json!([])),
        "gate": st.get("gate").cloned().unwrap_or(Value::Null)})).collect::<Vec<_>>())
}

/// A step's gates: the named ones (in the order it lists them), then a gate step's own inline checks.
fn step_gate_specs(wf: &Value, step: &Value) -> Vec<Value> {
    let mut specs: Vec<Value> = str_list(step, "gates")
        .iter()
        .filter_map(|n| arr(wf, "gates").iter().find(|g| s(g, "name") == *n).cloned())
        .collect();
    if step.get("gate").is_some_and(Value::is_object) {
        let mut g = step["gate"].clone();
        g["name"] = json!(s(step, "name"));
        specs.push(g);
    }
    specs
}

/// Whether finishing this step needs a person's approval (one of its gates has approve: true).
fn wants_approval(wf: &Value, step: &Value) -> bool {
    step_gate_specs(wf, step).iter().any(|g| b(g, "approve"))
}

/// Every step this one depends on, directly or indirectly.
fn upstream_steps(wf: &Value, sid: &str) -> HashSet<String> {
    let steps = arr(wf, "steps");
    let deps = |id: &str| steps.iter().find(|st| st["id"] == id).map(|st| str_list(st, "dependsOn")).unwrap_or_default();
    let (mut seen, mut todo) = (HashSet::new(), deps(sid));
    while let Some(d) = todo.pop() {
        if seen.insert(d.clone()) {
            todo.extend(deps(&d));
        }
    }
    seen
}

fn upstream_names(wf: &Value, ids: &HashSet<String>) -> Vec<String> {
    arr(wf, "steps").iter().filter(|st| ids.contains(&s(st, "id"))).map(|st| s(st, "name")).collect()
}

/// One line per gate check, for a gate step's result.
fn gate_summary(items: &[Value]) -> String {
    items
        .iter()
        .map(|it| {
            let mark = match s(it, "status").as_str() { "pass" => "✓", "fail" | "error" => "✗", "pending" => "…", _ => "–" };
            let what = match s(it, "kind").as_str() { "run" => "command", "judge" => "AI judge", _ => "your approval" };
            let detail = first_line(&s(it, "detail"));
            format!("{mark} {} ({what}){}", s(it, "gate"), if detail.is_empty() { String::new() } else { format!(": {detail}") })
        })
        .collect::<Vec<_>>()
        .join("\n")
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn wait_for(what: &str, secs: u64, mut ok: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(secs);
        while !ok() {
            assert!(Instant::now() < end, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn dead_pid() -> u32 {
        let mut child = Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    fn step(id: &str, run: &str) -> Value {
        json!({"id": id, "name": id, "kind": "bash", "run": run})
    }

    #[test]
    fn resume_point_from_record() {
        let steps = vec![step("a", ""), step("b", ""), step("c", ""), step("d", "")];
        let run = json!({
            "attempts": [
                {"step": "a", "session": "s-a", "status": "ok", "output": "A"},
                {"step": "b", "session": "s-b", "status": "ok", "output": "B", "review": {"status": "pending"}},
                {"step": "c", "session": "s-c1", "status": "failed"},
                {"step": "c", "session": "s-c2", "status": "interrupted"},
                {"step": "d", "session": "s-d", "status": "interrupted", "kind": "bash"},
            ],
            "resumeFrom": {"steps": {"a": {"ok": true, "output": "A full", "session": "s-a", "notes": "n"}, "gone": {"ok": true}},
                           "loops": {"c": 1, "@a": 1}, "hops": 4},
        });
        let p = resume_point(&run, &steps);
        assert_eq!(p.seed.results.keys().collect::<Vec<_>>(), vec!["a"]); // a step no longer in the workflow is dropped
        assert_eq!(p.seed.results["a"].out, "A full");
        assert_eq!(p.seed.results["a"].notes, "n");
        assert_eq!(p.seed.loops["c"], 1);
        assert_eq!(p.seed.hops, 4);
        assert_eq!(p.todo, HashSet::from(["b".to_string(), "c".to_string(), "d".to_string()]));
        // the Claude step cut off mid-flight continues its own session; a command step starts over
        assert_eq!(p.restart, HashMap::from([("c".to_string(), "s-c2".to_string())]));
        // the step waiting for review asks again instead of running again
        assert_eq!(p.reviews["b"], ("B".to_string(), "s-b".to_string()));
    }

    #[test]
    fn resume_point_without_record() {
        // runs saved before resumeFrom existed: each step's last successful attempt counts as done
        let steps = vec![step("a", ""), step("b", ""), step("c", "")];
        let run = json!({"attempts": [
            {"step": "a", "session": "s-a", "status": "ok", "output": "A", "review": {"status": "approved", "feedback": "nice"}},
            {"step": "b", "session": "s-b1", "status": "ok", "output": "B"},
            {"step": "b", "session": "s-b2", "status": "running"},
        ]});
        let p = resume_point(&run, &steps);
        assert_eq!(p.seed.results.len(), 1);
        assert_eq!(p.seed.results["a"].notes, "nice");
        assert_eq!(p.todo, HashSet::from(["b".to_string(), "c".to_string()]));
        assert_eq!(p.restart["b"], "s-b2");
    }

    #[test]
    fn mark_interrupted_marks_working_attempts() {
        let mut run = json!({"status": "running", "external": true, "attempts": [{"status": "ok"}, {"status": "running"}]});
        mark_interrupted(&mut run);
        assert_eq!(run["status"], "interrupted");
        assert!(run.get("external").is_none());
        assert_eq!(run["attempts"][0]["status"], "ok");
        assert_eq!(run["attempts"][1]["status"], "interrupted");
    }

    /// The acceptance test from the spec: the process dies during step 2 of 4; resuming finishes the run
    /// without running step 1 again or counting its cost twice.
    #[test]
    fn interrupted_run_resumes_from_the_unfinished_step() {
        let dir = std::env::temp_dir().join(format!("ag-resume-test-{}", hex_id(6))).to_string_lossy().into_owned();
        std::fs::create_dir_all(&dir).unwrap();
        let file = join(&dir, "flow.yaml");
        let raw = json!({"name": "Resume test", "cwd": dir, "steps": [
            step("one", "echo ran >> one.count; echo first"),
            step("two", "echo ran >> two.count; if [ -f go ]; then echo second; else sleep 30; fi"),
            step("three", "echo \"$STEP_INPUT\" > three.in; echo third"),
            step("four", "echo ran >> four.count; echo fourth"),
        ]});
        std::fs::write(&file, serde_yaml::to_string(&raw).unwrap()).unwrap();
        let wf = read_file(&file, None).unwrap().0; // resuming reads the workflow from its file again
        let wf = validate(&wf).unwrap();

        // the first app: runs step 1, and is cut off while step 2 works
        let first = Runner::new(Arc::new(Graph::new()), Box::new(|| {}));
        let rid = first.start("", Some(wf.clone()), &json!({})).unwrap();
        wait_for("step two to start", 20, || first.get_run(&rid).is_some_and(|r| arr(&r, "attempts").iter().any(|a| a["step"] == "two" && a["status"] == "running")));
        wait_for("step two's command to start", 20, || is_file(&join(&dir, "two.count")));
        let path = join(&run_dir(), &format!("{rid}.json"));
        let mut saved = read_json(&path).unwrap();
        assert_eq!(saved["resumeFrom"]["steps"]["one"]["output"].as_str().map(str::trim), Some("first"));
        assert!(saved["resumeFrom"]["steps"].get("two").is_none());
        // what a crash leaves on disk: still "running", its process gone. Step 1 cost something.
        let crashed = format!("{rid}-crashed");
        saved["id"] = json!(crashed);
        saved["pid"] = json!(dead_pid());
        saved["cost"] = json!(0.25);
        write_json(&join(&run_dir(), &format!("{crashed}.json")), &saved).unwrap();
        first.stop(&rid).unwrap();

        // the app starts again: the run is interrupted, and resumes
        let second = Runner::new(Arc::new(Graph::new()), Box::new(|| {}));
        assert_eq!(s(&second.get_run(&crashed).unwrap(), "status"), "interrupted");
        std::fs::write(join(&dir, "go"), "").unwrap();
        second.resume(&crashed).unwrap();
        assert!(second.resume(&crashed).is_err(), "a running run can't be resumed again");
        wait_for("the resumed run to finish", 30, || second.get_run(&crashed).is_some_and(|r| !["running", "waiting"].contains(&s(&r, "status").as_str())));

        let run = second.get_run(&crashed).unwrap();
        assert_eq!(s(&run, "status"), "succeeded", "{}", s(&run, "error"));
        let count = |f: &str| std::fs::read_to_string(join(&dir, f)).unwrap_or_default().lines().count();
        assert_eq!(count("one.count"), 1, "step one ran again");
        assert_eq!(count("two.count"), 2); // the cut-off try, then the resumed one
        assert_eq!(count("four.count"), 1);
        assert_eq!(std::fs::read_to_string(join(&dir, "three.in")).unwrap().trim(), "second"); // step 3 got step 2's new output
        assert_eq!(f(&run, "cost"), 0.25, "cost counted twice");
        assert_eq!(arr(&run, "attempts").iter().filter(|a| a["step"] == "one").count(), 1);
        let two: Vec<String> = arr(&run, "attempts").iter().filter(|a| a["step"] == "two").map(|a| s(a, "status")).collect();
        assert_eq!(two, ["interrupted", "ok"]);
        assert!(run.get("resumeFrom").is_none());
        assert_eq!(run["resumed"], 1);
        // and it was saved that way
        assert_eq!(s(&read_json(&join(&run_dir(), &format!("{crashed}.json"))).unwrap(), "status"), "succeeded");
        wait_for("the first run to stop", 20, || first.get_run(&rid).is_some_and(|r| s(&r, "status") == "stopped"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_interrupted_runs_resume() {
        let r = Runner::new(Arc::new(Graph::new()), Box::new(|| {}));
        assert!(r.resume("no-such-run").is_err());
    }

    /// Runs a workflow to the end and returns the finished run (its saved files are removed when `done` runs).
    fn run(wf: Value) -> (Value, impl FnOnce()) {
        let runner = Runner::new(Arc::new(Graph::new()), Box::new(|| {}));
        let rid = runner.start("guard-test", Some(wf), &json!({})).unwrap();
        for _ in 0..600 {
            let r = runner.get_run(&rid).unwrap();
            if !["running", "waiting"].contains(&s(&r, "status").as_str()) && !b(&r, "finishing") {
                return (r, move || drop(runner.discard(&rid)));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("the run didn't finish");
    }

    /// A stand-in `claude` on PATH: it edits .env and claims success, or reports running out of turns when
    /// given --max-turns, and records its arguments.
    #[cfg(unix)]
    #[test]
    fn guardrails_end_to_end() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ag-guard-run-{}", hex_id(6))).to_string_lossy().into_owned();
        let bin = join(&dir, "bin");
        let proj = join(&dir, "proj");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(join(&proj, ".env"), "A=1\n").unwrap();
        let fake = join(&bin, "claude");
        std::fs::write(&fake, format!(r#"#!/bin/sh
cat > /dev/null
echo "$@" >> "{dir}/args.txt"
case "$*" in
  *--max-turns*) echo '{{"type":"result","subtype":"error_max_turns","is_error":true,"num_turns":3,"result":"","total_cost_usd":0.01}}' ;;
  *) echo B=2 >> .env; echo '{{"type":"result","subtype":"success","is_error":false,"result":"All done, nothing else touched","total_cost_usd":0.02}}' ;;
esac
"#)).unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::env::set_var("PATH", format!("{bin}:{}", std::env::var("PATH").unwrap_or_default()));

        // a Claude step that edits .env fails even though it claims success, and isn't retried
        let (r, done) = run(json!({"name": "guard", "cwd": proj, "protect": [".env*"], "limits": {"budgetUsd": 1},
                            "steps": [{"id": "edit", "prompt": "Fix the bug", "retries": 2}]}));
        assert_eq!(s(&r, "status"), "failed");
        let atts = arr(&r, "attempts");
        assert_eq!(atts.len(), 1);
        assert!(s(&atts[0], "error").starts_with("changed protected files: .env"), "{}", atts[0]);
        let args = std::fs::read_to_string(join(&dir, "args.txt")).unwrap();
        assert!(args.contains("Edit(.env*),Write(.env*)") && args.contains("--max-budget-usd 1"), "{args}");
        done();

        // maxTurns is passed to Claude Code, and running out of turns fails the step with no retry
        let (r, done) = run(json!({"name": "turns", "cwd": proj, "steps": [{"id": "t", "prompt": "Work", "retries": 2, "limits": {"maxTurns": 3}}]}));
        let atts = arr(&r, "attempts");
        assert_eq!(atts.len(), 1);
        assert_eq!(s(&atts[0], "error"), "went over its limit: it used its 3 turns");
        assert!(std::fs::read_to_string(join(&dir, "args.txt")).unwrap().contains("--max-turns 3"));
        done();

        // a shell step that touches a protected file fails; its saved output and log are redacted
        let key = format!("sk-ant-api03-{}", "z".repeat(40));
        let (r, done) = run(json!({"name": "bash", "cwd": proj, "redact": true, "steps": [
            {"id": "ok", "run": format!("echo using {key}")},
            {"id": "lock", "run": "touch yarn.lock", "protect": ["**/*.lock"]}]}));
        let atts = arr(&r, "attempts");
        assert_eq!(s(&atts[0], "status"), "ok");
        assert_eq!(s(&atts[0], "output").trim(), "using [REDACTED]");
        let log = std::fs::read_to_string(s(&atts[0], "log")).unwrap();
        assert!(log.contains("[REDACTED]") && !log.contains(&key), "{log}");
        assert!(s(&atts[1], "error").starts_with("changed protected files: yarn.lock"), "{}", atts[1]);
        done();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
