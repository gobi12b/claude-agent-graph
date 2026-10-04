//! The local HTTP server: the page, its API and the live event stream.
//!
//! It listens only on 127.0.0.1, answers only requests addressed to localhost, and every API call needs a
//! random token issued at startup, so other websites can't reach it.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::runner::Runner;
use crate::util::*;
use crate::watcher::Graph;
use crate::{providers, wfstore, workflows};

const INDEX_HTML: &str = include_str!("../agent_graph/index.html");
const ICON_SVG: &[u8] = include_bytes!("../agent_graph/assets/icon.svg");
pub const ICON_PNG: &[u8] = include_bytes!("../agent_graph/assets/icon.png");
const POLL: Duration = Duration::from_millis(500);
const HEARTBEAT: Duration = Duration::from_secs(3);

pub struct App {
    pub graph: Arc<Graph>,
    pub runner: Arc<Runner>,
    token: String,
    changed: Arc<(Mutex<u64>, Condvar)>,
    wf_version: AtomicU64, // bumps when a workflow YAML changes on disk
}

fn notifier(changed: &Arc<(Mutex<u64>, Condvar)>) -> impl Fn() + Send + Sync + 'static {
    let changed = changed.clone();
    move || {
        *changed.0.lock().unwrap() += 1;
        changed.1.notify_all();
    }
}

impl App {
    pub fn new() -> Arc<App> {
        let graph = Arc::new(Graph::new());
        let changed = Arc::new((Mutex::new(0u64), Condvar::new()));
        let g = graph.clone();
        wfstore::set_project_hint(Box::new(move || g.project_dirs())); // projects your sessions ran in may hold workflows
        let runner = Runner::new(graph.clone(), Box::new(notifier(&changed)));
        let token = uuid::Uuid::new_v4().simple().to_string() + &uuid::Uuid::new_v4().simple().to_string()[..8];
        Arc::new(App { graph, runner, token, changed, wf_version: AtomicU64::new(0) })
    }

    fn notify(&self) {
        notifier(&self.changed)();
    }

    pub fn poll_forever(self: Arc<Self>) {
        loop {
            let before = self.wf_version.load(Ordering::SeqCst);
            let now_v = wfstore::version();
            self.wf_version.store(now_v, Ordering::SeqCst);
            // + runs started in a terminal
            if self.graph.scan(true) | (now_v != before) | self.runner.sync_disk() {
                self.notify();
            }
            std::thread::sleep(POLL);
        }
    }

    /// Serve on 127.0.0.1:port (0 = any free port). Returns the port.
    pub fn serve(self: &Arc<Self>, port: u16) -> Res<u16> {
        let server = Server::http(("127.0.0.1", port)).map_err(|e| e.to_string())?;
        let port = server.server_addr().to_ip().map(|a| a.port()).unwrap_or(port);
        let app = self.clone();
        std::thread::spawn(move || {
            for req in server.incoming_requests() {
                let app = app.clone();
                std::thread::spawn(move || app.handle(req));
            }
        });
        Ok(port)
    }

    fn handle(&self, mut req: Request) {
        let header = |name: &str| req.headers().iter().find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name)).map(|h| h.value.as_str().to_string());
        let host = header("Host").unwrap_or_default();
        let host = host.rsplit_once(':').map(|(h, _)| h.to_string()).unwrap_or(host);
        if host != "127.0.0.1" && host != "localhost" {
            let _ = req.respond(Response::empty(403));
            return;
        }
        let token_ok = header("X-Token").as_deref() == Some(self.token.as_str());
        let path = req.url().split('?').next().unwrap_or("").to_string();
        match (req.method().clone(), path.as_str()) {
            (Method::Get, "/" | "/index.html") => {
                let home = serde_json::to_string(&home()).unwrap_or_default();
                let body = INDEX_HTML.replace("__TOKEN__", &self.token).replace("__HOME__", &home[1..home.len() - 1]);
                let _ = req.respond(Response::from_string(body).with_header(hdr("Content-Type", "text/html; charset=utf-8")));
            }
            (Method::Get, "/icon.svg") => {
                let _ = req.respond(Response::from_data(ICON_SVG).with_header(hdr("Content-Type", "image/svg+xml")).with_header(hdr("Cache-Control", "max-age=86400")));
            }
            (Method::Get, "/icon.png") => {
                let _ = req.respond(Response::from_data(ICON_PNG).with_header(hdr("Content-Type", "image/png")).with_header(hdr("Cache-Control", "max-age=86400")));
            }
            (Method::Get, "/events") => self.stream(req),
            (Method::Get, p) => match self.api_get(p) {
                None => {
                    let _ = req.respond(Response::empty(404));
                }
                Some(_) if !token_ok => {
                    let _ = req.respond(Response::empty(403));
                }
                Some(f) => respond_json(req, &result(f())),
            },
            (Method::Post, p) => {
                if !API_POST.contains(&p) {
                    let _ = req.respond(Response::empty(404));
                    return;
                }
                let mut raw = String::new();
                let _ = req.as_reader().read_to_string(&mut raw);
                let body: Value = if raw.trim().is_empty() {
                    json!({})
                } else {
                    match serde_json::from_str(&raw) {
                        Ok(v) => v,
                        Err(_) => return respond_json(req, &json!({"ok": false, "message": "Bad JSON."})),
                    }
                };
                if !token_ok {
                    let _ = req.respond(Response::empty(403));
                    return;
                }
                let out = if p == "/kill" { self.kill(&body) } else { result(self.api_post(p, &body)) };
                respond_json(req, &out);
            }
            _ => {
                let _ = req.respond(Response::empty(405));
            }
        }
    }

    fn api_get(&self, path: &str) -> Option<Box<dyn Fn() -> Res<Value> + '_>> {
        Some(match path {
            "/api/permissions" => Box::new(workflows::read_permissions),
            "/api/workflows" => Box::new(|| Ok(workflows::workflows_state())),
            "/api/ides" => Box::new(|| Ok(workflows::list_ides())),
            "/api/folders" => Box::new(|| Ok(self.recent_folders(12))),
            "/api/models" => Box::new(|| Ok(providers::status())),
            "/api/defaults" => Box::new(|| Ok(wfstore::load_defaults())),
            _ => return None,
        })
    }

    fn api_post(&self, path: &str, b: &Value) -> Res<Value> {
        let r = &self.runner;
        let opt = |k: &str| b.get(k).filter(|v| truthy(v)).map(py_str);
        let id = || req_str(b, "id");
        match path {
            "/api/permissions" => workflows::write_permissions(b),
            "/api/workflows/save" => workflows::save_workflow(b),
            "/api/workflows/delete" => workflows::delete_workflow(&id()?).map(|_| Value::Null),
            "/api/workflows/open" => workflows::open_workflow_file(b.get("id").unwrap_or(&Value::Null), b.get("what").unwrap_or(&Value::Null)),
            "/api/agents/save" => wfstore::save_user_agent(req(b, "agent")?, &opt("category").unwrap_or("My helpers".into()), opt("oldName").as_deref()),
            "/api/agents/delete" => wfstore::delete_user_agent(&req_str(b, "name")?, false),
            "/api/steps/save" => wfstore::save_user_step(opt("key").as_deref(), req(b, "step")?, opt("oldKey").as_deref()),
            "/api/steps/delete" => wfstore::delete_user_step(&req_str(b, "key")?, false),
            "/api/ides/set" => workflows::set_ide(b.get("ide").unwrap_or(&Value::Null)),
            "/api/runs/ide" => r.open_ide(&id()?, opt("path").as_deref()),
            "/api/runs/start" => r.start(&id()?, None).map(Value::from),
            "/api/runs/stop" => r.stop(&id()?),
            "/api/runs/replay" => r.replay(&id()?, &req_str(b, "step")?, opt_truthy(b.get("only")), None),
            "/api/folders/check" => check_folder(b),
            "/api/models/extra" => providers::set_extra_models(&s(b, "provider"), b.get("models").unwrap_or(&Value::Null)),
            "/api/folders/pick" => pick_folder(b),
            "/api/runs/steer" => r.steer(&id()?, &req_str(b, "step")?, &s(b, "message"), b.get("only") != Some(&json!(false))),
            "/api/runs/detail" => r.detail(&id()?),
            "/api/runs/folder" => r.open_folder(&id()?),
            "/api/runs/log" => r.log(&id()?, &req_str(b, "session")?),
            "/api/sessions/log" => r.session_log(&req_str(b, "session")?),
            "/api/runs/review" => r.review(&id()?, &s(b, "decision"), &s(b, "feedback")),
            "/api/runs/save" => r.write_artefact(&id()?, &req_str(b, "path")?, &req_str(b, "text")?),
            "/api/rewrite" => workflows::rewrite_text(b.get("text").unwrap_or(&Value::Null), &s_or(b, "kind", "step"), &s(b, "context")),
            "/api/runs/file" => r.read_artefact(&id()?, &req_str(b, "path")?),
            "/api/runs/changes" => r.changes(&id()?, &req_str(b, "session")?),
            "/api/runs/rewind" => r.rewind(&id()?, &req_str(b, "session")?),
            "/api/runs/unrewind" => r.undo_rewind(&id()?),
            "/api/runs/open" => r.open_artefact(&id()?, &req_str(b, "path")?, opt_truthy(b.get("folder"))),
            _ => err("Not found."),
        }
    }

    fn kill(&self, body: &Value) -> Value {
        let Ok(id) = req_str(body, "id") else { return json!({"ok": false, "message": "'id'"}) };
        let (ok, message) = self.graph.kill_session(&id);
        if ok {
            self.notify();
        }
        json!({"ok": ok, "message": message})
    }

    /// Folders your Claude sessions ran in, plus ones that already hold workflows, most recent first.
    fn recent_folders(&self, limit: usize) -> Value {
        let mut seen: indexmap::IndexMap<String, f64> = indexmap::IndexMap::new();
        {
            let g = self.graph.lock();
            for n in g.nodes.values() {
                let cwd = n.get("cwd").filter(|c| truthy(c)).map(py_str);
                if let (Some(cwd), Some("session")) = (cwd, n.get("kind").and_then(Value::as_str)) {
                    let t = n.get("lastActive").and_then(Value::as_f64).unwrap_or(0.0);
                    let e = seen.entry(cwd).or_insert(0.0);
                    *e = e.max(t);
                }
            }
        }
        for p in wfstore::project_dirs() {
            seen.entry(p).or_insert(0.0);
        }
        let home = realpath(&home());
        // skip temporary and hidden folders (scratch space, caches): nobody picks those as a project
        let useful = |p: &str| {
            let real = realpath(p);
            is_dir(&real) && real != home && !real.starts_with("/tmp") && !real.starts_with("/var/tmp") && !real.split(sep()).any(|part| part.starts_with('.'))
        };
        let mut dirs: Vec<(String, f64)> = seen.into_iter().collect();
        dirs.sort_by(|a, b| b.1.total_cmp(&a.1));
        let recent: Vec<String> = dirs.into_iter().map(|(p, _)| p).filter(|p| useful(p)).take(limit).map(|p| tilde(&p)).collect();
        json!({"recent": recent, "home": "~"})
    }

    fn stream(&self, req: Request) {
        let mut w = req.into_writer();
        if w.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n").is_err() {
            return;
        }
        let mut seq = 0;
        loop {
            let seen = *self.changed.0.lock().unwrap();
            let mut snap = self.graph.snapshot(seq);
            snap["runs"] = json!(self.runner.summary());
            snap["wfVersion"] = json!(self.wf_version.load(Ordering::SeqCst));
            seq = snap["seq"].as_u64().unwrap_or(seq);
            let msg = format!("data: {snap}\n\n");
            if w.write_all(msg.as_bytes()).and_then(|_| w.flush()).is_err() {
                return;
            }
            let guard = self.changed.0.lock().unwrap();
            if *guard == seen {
                let _ = self.changed.1.wait_timeout(guard, HEARTBEAT);
            }
        }
    }

    pub fn initial_scan(&self) {
        self.graph.scan(false); // history: shown, but not animated
    }
}

const API_POST: [&str; 30] = [
    "/kill", "/api/permissions", "/api/workflows/save", "/api/workflows/delete", "/api/workflows/open", "/api/agents/save",
    "/api/agents/delete", "/api/steps/save", "/api/steps/delete", "/api/ides/set", "/api/runs/ide", "/api/runs/start",
    "/api/runs/stop", "/api/runs/replay", "/api/folders/check", "/api/models/extra", "/api/folders/pick", "/api/runs/steer",
    "/api/runs/detail", "/api/runs/folder", "/api/runs/log", "/api/sessions/log", "/api/runs/review", "/api/runs/save",
    "/api/rewrite", "/api/runs/file", "/api/runs/changes", "/api/runs/rewind", "/api/runs/unrewind", "/api/runs/open",
];

fn hdr(k: &str, v: &str) -> Header {
    Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap()
}

fn result(r: Res<Value>) -> Value {
    match r {
        Ok(v) => json!({"ok": true, "data": v}),
        Err(e) => json!({"ok": false, "message": e}),
    }
}

fn respond_json(req: Request, v: &Value) {
    let _ = req.respond(Response::from_string(v.to_string()).with_header(hdr("Content-Type", "application/json")));
}

fn check_folder(body: &Value) -> Res<Value> {
    let raw = s(body, "path").trim().to_string();
    if raw.is_empty() {
        return err("Choose a folder first.");
    }
    let path = realpath(&expanduser(&raw));
    if b(body, "create") && !exists(&path) {
        std::fs::create_dir_all(&path).map_err(|e| e.to_string())?;
    }
    let is = is_dir(&path);
    Ok(json!({"path": tilde(&path), "exists": is, "isFile": is_file(&path),
              "git": is && is_dir(&join(&path, ".git")),
              "entries": if is { std::fs::read_dir(&path).map(|d| d.count()).unwrap_or(0) } else { 0 }}))
}

/// Open the desktop's own folder chooser (zenity or kdialog). Returns null if cancelled.
fn pick_folder(body: &Value) -> Res<Value> {
    let mut start = realpath(&expanduser(&s_or(body, "start", "~")));
    if !is_dir(&start) {
        start = home();
    }
    let mut cmd;
    if which("zenity").is_some() {
        cmd = std::process::Command::new("zenity");
        cmd.args(["--file-selection", "--directory", "--title=Choose the project folder", &format!("--filename={start}{}", sep())]);
    } else if which("kdialog").is_some() {
        cmd = std::process::Command::new("kdialog");
        cmd.args(["--getexistingdirectory", &start, "--title", "Choose the project folder"]);
    } else {
        return err("No folder chooser is available here. Type the folder's path instead.");
    }
    let out = cmd.output().map_err(|e| e.to_string())?;
    let chosen = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok(if out.status.success() && !chosen.is_empty() { json!(tilde(&chosen)) } else { Value::Null })
}
