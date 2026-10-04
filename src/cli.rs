//! Run workflows from the terminal: `agent-graph list`, `agent-graph run <workflow or file.yaml>`,
//! `agent-graph validate [workflows or files]`, `agent-graph runs`.
//!
//! Runs use the same runner as the app and are saved in the same place, so they also show up on the app's
//! Runs page. A step that pauses for review asks you here (approve, request changes, or stop).

use std::collections::HashMap;
use std::io::{BufRead, IsTerminal};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use serde_json::{json, Value};

use crate::runner::Runner;
use crate::template;
use crate::util::*;
use crate::watcher::Graph;
use crate::workflows::{self, pick};
use crate::wfstore;

const NEEDS_REVIEW: i32 = 3;

#[derive(Parser)]
#[command(name = "agent-graph", about = "Run Claude Agent Graph workflows from the terminal.")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// list your workflows
    List {
        /// print JSON
        #[arg(long)]
        json: bool,
    },
    /// run a workflow (by name, or a workflow .yaml file) and follow it here
    Run {
        /// a workflow file (path/to/flow.yaml), or the id or name (or its start) of a workflow the app knows
        workflow: String,
        /// run in this project folder instead of the one in the workflow
        #[arg(short = 'C', long)]
        folder: Option<String>,
        /// approve review points automatically
        #[arg(short, long)]
        yes: bool,
        /// don't print the final result
        #[arg(short, long)]
        quiet: bool,
        /// print a JSON summary at the end instead of progress
        #[arg(long)]
        json: bool,
        /// a value for one of the workflow's inputs: --input bug="Login fails" (repeat for more)
        #[arg(short = 'i', long = "input", value_name = "NAME=VALUE")]
        input: Vec<String>,
        /// input values from a JSON file ({"bug": "…"}); --input wins over it
        #[arg(long, value_name = "FILE")]
        inputs: Option<String>,
        /// run in a separate git worktree (worktree), or in the folder itself (none)
        #[arg(long, value_parser = ["none", "worktree"])]
        isolation: Option<String>,
    },
    /// remove the separate copies (worktrees) of finished runs
    Clean,
    /// check workflows for mistakes without running them
    Validate {
        /// workflow files or names (default: every workflow the app knows)
        targets: Vec<String>,
        /// check as if run in this folder (matters for a file without cwd)
        #[arg(short = 'C', long)]
        folder: Option<String>,
        /// treat warnings as errors (exit code 1)
        #[arg(long)]
        strict: bool,
        /// print JSON
        #[arg(long)]
        json: bool,
    },
    /// list recent runs
    Runs {
        /// how many (default 15)
        #[arg(short = 'n', long, default_value_t = 15)]
        limit: usize,
        /// print JSON
        #[arg(long)]
        json: bool,
    },
}

fn use_color() -> bool {
    static C: OnceLock<bool> = OnceLock::new();
    *C.get_or_init(|| std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none())
}

fn c(code: &str, text: &str) -> String {
    if use_color() { format!("\x1b[{code}m{text}\x1b[0m") } else { text.to_string() }
}

fn dur(sec: f64) -> String {
    let sec = sec.max(0.0) as i64;
    if sec < 60 {
        format!("{sec}s")
    } else if sec < 3600 {
        format!("{}m {}s", sec / 60, sec % 60)
    } else {
        format!("{}h {}m", sec / 3600, sec % 3600 / 60)
    }
}

/// Print a message and exit with code 1 (like Python's sys.exit("message")).
fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("{msg}");
    std::process::exit(1)
}

fn print_json(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

fn is_file_arg(arg: &str) -> bool {
    arg.ends_with(".yaml") || arg.ends_with(".yml") || arg.contains('/') || arg.contains(std::path::MAIN_SEPARATOR) || is_file(arg)
}

/// A workflow file path, or the id/name of a workflow the app knows. Exits with a message if not found.
fn target(arg: &str, folder: Option<&str>) -> Value {
    if is_file_arg(arg) {
        return workflows::load_file(arg, folder).unwrap_or_else(|e| die(format!("{arg}: {e}")));
    }
    let mut wf = find(arg);
    if let Some(folder) = folder {
        wf["cwd"] = json!(abspath(&expanduser(folder)));
        wf = workflows::validate(&wf).unwrap_or_else(|e| die(e));
    }
    wf
}

/// A workflow by id, exact name, or the start of its name (case doesn't matter).
fn find(query: &str) -> Value {
    let mut errors = vec![];
    let wfs = workflows::list_workflows(Some(&mut errors));
    let q = query.trim().to_lowercase();
    let tests: [&dyn Fn(&Value) -> bool; 3] = [
        &|w| s(w, "id") == query,
        &|w| s(w, "name").to_lowercase() == q,
        &|w| s(w, "name").to_lowercase().starts_with(&q) || s(w, "id").starts_with(&q),
    ];
    for test in tests {
        let hits: Vec<&Value> = wfs.iter().filter(|w| test(w)).collect();
        if hits.len() == 1 {
            return hits[0].clone();
        }
        if hits.len() > 1 {
            let names: Vec<String> = hits.iter().map(|w| format!("  {:<28} {}", s(w, "id"), s(w, "name"))).collect();
            die(format!("“{query}” matches several workflows; use the id:\n{}", names.join("\n")));
        }
    }
    if let Some(bad) = errors.iter().find(|e| s(e, "name").to_lowercase().contains(&q)) {
        die(format!("{} has a mistake, so it can't run: {}", s(bad, "name"), s(bad, "error")));
    }
    die(format!("No workflow called “{query}”. See them with: agent-graph list"))
}

fn cmd_list(as_json: bool) -> i32 {
    let mut errors = vec![];
    let wfs = workflows::list_workflows(Some(&mut errors));
    if as_json {
        let out: Vec<Value> = wfs
            .iter()
            .map(|w| {
                let mut o = pick(w, &["id", "name", "cwd", "file"]);
                o.insert("steps".into(), json!(arr(w, "steps").iter().map(|st| st["name"].clone()).collect::<Vec<_>>()));
                Value::Object(o)
            })
            .collect();
        print_json(&json!(out));
        return 0;
    }
    if wfs.is_empty() {
        println!("No workflows yet. Create one in the app (agent-graph), on the Workflows page.");
    }
    for w in &wfs {
        println!("{}  {}", c("1", &s(w, "name")), c("2", &format!("({})", s(w, "id"))));
        println!("  {}", arr(w, "steps").iter().map(|st| s(st, "name")).collect::<Vec<_>>().join(" → "));
        println!("  {}", c("2", &format!("📁 {}", tilde(&s(w, "cwd")))));
    }
    for e in &errors {
        eprintln!("{}", c("31", &format!("⚠ {} isn't loaded: {}", s(e, "name"), s(e, "error"))));
    }
    0
}

fn cmd_runs(limit: usize, as_json: bool) -> i32 {
    let runner = Runner::new(Arc::new(Graph::new()), Box::new(|| {}));
    let mut runs = runner.summary();
    runs.truncate(limit);
    if as_json {
        let out: Vec<Value> = runs.iter().map(|r| Value::Object(pick(r, &["id", "name", "status", "started", "ended", "cost", "error"]))).collect();
        print_json(&json!(out));
        return 0;
    }
    if runs.is_empty() {
        println!("No runs yet.");
    }
    for r in &runs {
        let started = f(r, "started");
        let when = format_local(started, "%b %d %H:%M");
        let ended = r.get("ended").and_then(Value::as_f64).filter(|e| *e != 0.0).unwrap_or_else(now);
        println!("{}  {when}  {:<9}  {:>7}  ${:.2}  {}", s(r, "id"), s(r, "status"), dur(ended - started), f(r, "cost"), s(r, "name"));
    }
    0
}

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Ctrl+C (SIGINT) and SIGTERM set INTERRUPTED; pressed twice, give up waiting and exit.
#[cfg(unix)]
fn on_interrupt() {
    extern "C" fn handler(_: libc::c_int) {
        if INTERRUPTED.swap(true, Ordering::SeqCst) {
            // SAFETY: _exit is async-signal-safe.
            unsafe { libc::_exit(130) };
        }
    }
    // SAFETY: the handler only touches an atomic and calls _exit.
    unsafe {
        libc::signal(libc::SIGINT, handler as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, handler as *const () as libc::sighandler_t);
    }
}

#[cfg(windows)]
fn on_interrupt() {
    let _ = ctrlc::set_handler(|| {
        if INTERRUPTED.swap(true, Ordering::SeqCst) {
            std::process::exit(130);
        }
    });
}

/// Lines typed in the terminal, read on their own thread so Ctrl+C can still stop the run mid-question.
fn stdin_lines() -> &'static std::sync::Mutex<Receiver<String>> {
    static RX: OnceLock<std::sync::Mutex<Receiver<String>>> = OnceLock::new();
    RX.get_or_init(|| {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                if tx.send(line.unwrap_or_default()).is_err() {
                    break;
                }
            }
        });
        std::sync::Mutex::new(rx)
    })
}

/// Ask a question; None when Ctrl+C was pressed (or input ended).
fn input(prompt: &str) -> Option<String> {
    use std::io::Write;
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let rx = stdin_lines().lock().unwrap();
    loop {
        if INTERRUPTED.load(Ordering::SeqCst) {
            return None;
        }
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => return Some(line),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return None,
        }
    }
}

/// The run paused for review: show the result and ask what to do. None: Ctrl+C.
fn ask_review(runner: &Runner, rid: &str, att: &Value) -> Option<()> {
    println!();
    println!("{}", c("35;1", &format!("👤 “{}” is done and waiting for your review.", s(att, "name"))));
    let out = s(att, "output").trim().to_string();
    if !out.is_empty() {
        let lines: Vec<&str> = out.lines().collect();
        println!("{}", c("2", &format!("── its result {}──", if lines.len() > 25 { "(last 25 lines) " } else { "" })));
        println!("{}", lines[lines.len().saturating_sub(25)..].join("\n"));
        println!("{}", c("2", "──"));
    }
    loop {
        let choice: String = input("Approve and continue [a], request changes [c], or stop [s]? ")?.trim().to_lowercase().chars().take(1).collect();
        match choice.as_str() {
            "a" => {
                let note = input("Notes for the next step (optional, Enter to skip): ")?;
                let _ = runner.review(rid, "approve", note.trim());
                return Some(());
            }
            "s" => {
                let _ = runner.review(rid, "stop", "");
                return Some(());
            }
            "c" => {
                let fb = input("What should change? ")?.trim().to_string();
                if !fb.is_empty() {
                    let _ = runner.review(rid, "revise", &fb);
                    return Some(());
                }
                println!("Write what should change, or choose another option.");
            }
            _ => {}
        }
    }
}

fn live(run: &Value) -> bool {
    matches!(s(run, "status").as_str(), "running" | "waiting") || b(run, "finishing")
}

fn wait_end(runner: &Runner, rid: &str, timeout: Duration) {
    let end = Instant::now() + timeout;
    while runner.get_run(rid).is_some_and(|r| live(&r)) && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Input values from --inputs FILE and --input NAME=VALUE; asks for missing required ones in a terminal.
fn gather_inputs(wf: &Value, pairs: &[String], file: Option<&str>, ask: bool) -> Value {
    let mut given = match file {
        Some(f) => {
            let text = std::fs::read_to_string(expanduser(f)).unwrap_or_else(|e| die(format!("{f}: {e}")));
            serde_json::from_str::<Value>(&text).ok().filter(Value::is_object).unwrap_or_else(|| die(format!("{f}: expected a JSON object of input values")))
        }
        None => json!({}),
    };
    for p in pairs {
        let Some((k, v)) = p.split_once('=') else { die(format!("--input {p}: write it as NAME=VALUE")) };
        given[k.trim()] = json!(v);
    }
    let defs = wf["inputs"].as_object().cloned().unwrap_or_default();
    if ask {
        for (name, d) in &defs {
            if b(d, "required") && d["default"].is_null() && given.get(name).is_none_or(|v| template::as_text(v).trim().is_empty()) {
                let mut q = if b(d, "description") { s(d, "description") } else { name.clone() };
                if s(d, "type") == "choice" {
                    q += &format!(" [{}]", str_list(d, "options").join("/"));
                }
                match input(&format!("{} {q}: ", c("36", "?"))) {
                    Some(v) => given[name] = json!(v.trim()),
                    None => die("No input given."),
                }
            }
        }
    }
    given
}

fn cmd_clean() -> i32 {
    let runner = Runner::new(Arc::new(Graph::new()), Box::new(|| {}));
    let n = runner.clean();
    println!("Removed {n} separate cop{} of finished runs.", if n == 1 { "y" } else { "ies" });
    0
}

#[allow(clippy::too_many_arguments)]
fn cmd_run(workflow: &str, folder: Option<&str>, yes: bool, quiet: bool, as_json: bool, pairs: &[String], inputs_file: Option<&str>, isolation: Option<&str>) -> i32 {
    // Ctrl+C here, or Stop in the app (which sends the same signal), stops the run cleanly.
    on_interrupt();
    let mut wf = target(workflow, folder);
    if let Some(iso) = isolation {
        wf["isolation"] = json!(iso);
        if iso == "none" {
            wf["deliver"] = Value::Null; // delivery needs a worktree
        }
        wf = workflows::validate(&wf).unwrap_or_else(|e| die(e));
    }
    let given = gather_inputs(&wf, pairs, inputs_file, std::io::stdin().is_terminal() && !as_json);
    let runner = Runner::new(Arc::new(Graph::new()), Box::new(|| {}));
    let interactive = std::io::stdin().is_terminal() && !yes;
    let rid = runner.start(&s(&wf, "id"), Some(wf.clone()), &given).unwrap_or_else(|e| die(e));
    if !as_json {
        println!("{}{}", c("1", &format!("▶ {}", s(&wf, "name"))), c("2", &format!("  ({} steps · run {rid})", arr(&wf, "steps").len())));
        println!("{}", c("2", &format!("  in {}   ·   Ctrl+C stops it", tilde(&s(&wf, "cwd")))));
    }
    let mut seen: HashMap<String, String> = HashMap::new();
    let mut stopping = false;
    loop {
        if INTERRUPTED.load(Ordering::SeqCst) {
            stopping = true;
            break;
        }
        let run = runner.get_run(&rid).unwrap_or_default();
        for att in arr(&run, "attempts") {
            let (key, st) = (s(att, "session"), s(att, "status"));
            if seen.get(&key) == Some(&st) {
                continue;
            }
            seen.insert(key, st.clone());
            if as_json {
                continue;
            }
            let attempt = att["attempt"].as_i64().unwrap_or(1);
            let mut label = s(att, "name");
            if attempt > 1 {
                label += &format!(" (retry {})", attempt - 1);
            }
            if b(att, "steer") {
                label += " (steered)";
            }
            match st.as_str() {
                "running" => println!("  {} {label}…", c("33", "●")),
                "ok" => {
                    let took = format!("{} · ${:.2}", dur(f(att, "ended") - f(att, "started")), f(att, "cost"));
                    let j = att.get("judge").cloned().unwrap_or(json!({}));
                    let verdict = if s(&j, "status") == "pass" { format!("  {}", c("36", &format!("⚖ judge {}/100", py_str(&j["score"])))) } else { String::new() };
                    println!("  {} {label}  {}{verdict}", c("32", "✓"), c("2", &took));
                }
                "failed" => println!("  {} {label}: {}", c("31", "✗"), first_chars(&s(att, "error"), 300)),
                "stopped" => println!("  {} {label} stopped", c("2", "■")),
                _ => {}
            }
        }
        if s(&run, "status") == "waiting" {
            let pending = arr(&run, "attempts").iter().rev().find(|a| a.get("review").map(|r| s(r, "status")) == Some("pending".into())).cloned();
            if let Some(att) = pending {
                if yes {
                    let _ = runner.review(&rid, "approve", "");
                } else if interactive {
                    if ask_review(&runner, &rid, &att).is_none() {
                        stopping = true;
                        break;
                    }
                } else {
                    let _ = runner.review(&rid, "stop", "");
                    eprintln!(
                        "{}",
                        c("35", &format!("👤 “{}” needs your review. Run it in a terminal you can type in, add --yes to approve reviews automatically, or use the app.", s(&att, "name")))
                    );
                    wait_end(&runner, &rid, Duration::from_secs(60));
                    return NEEDS_REVIEW;
                }
            }
        }
        if !live(&runner.get_run(&rid).unwrap_or_default()) {
            break;
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    if stopping {
        println!("{}", c("31", "\n■ Stopping…"));
        let _ = runner.stop(&rid);
        wait_end(&runner, &rid, Duration::from_secs(60));
    }
    let run = runner.get_run(&rid).unwrap_or_default();
    let status = s(&run, "status");
    if as_json {
        let mut out = pick(&run, &["id", "name", "status", "started", "ended", "cost", "error", "inputs", "delivery", "worktree"]);
        let steps: Vec<Value> = arr(&run, "attempts")
            .iter()
            .map(|a| Value::Object(pick(a, &["name", "status", "attempt", "cost", "error", "output", "judge", "usage"])))
            .collect();
        out.insert("steps".into(), json!(steps));
        print_json(&Value::Object(out));
    } else {
        let mark = match status.as_str() {
            "succeeded" => c("32;1", "✓ Finished"),
            "failed" => c("31;1", "✗ Failed"),
            "stopped" => c("2;1", "■ Stopped"),
            other => other.to_string(),
        };
        let ended = run.get("ended").and_then(Value::as_f64).unwrap_or_else(now);
        let took = format!("{} · ${:.2}", dur(ended - f(&run, "started")), f(&run, "cost"));
        let error = if b(&run, "error") { format!("\n  {}", s(&run, "error")) } else { String::new() };
        println!("{mark}  {}{error}", c("2", &took));
        if let Some(d) = run.get("delivery").filter(|d| d.is_object()) {
            if b(d, "branch") {
                let pr = if b(d, "pr") { format!("  ·  PR {}", s(d, "pr")) } else if b(d, "pushed") { "  ·  pushed".into() } else { String::new() };
                println!("  {} branch {}  ({} commit{}){pr}", c("36", "⎇"), s(d, "branch"), py_str(&d["commits"]), if d["commits"] == 1 { "" } else { "s" });
            }
            if b(d, "error") {
                println!("  {} couldn't deliver: {}", c("31", "✗"), s(d, "error"));
            }
        }
        if let Some(wt) = run.get("worktree").filter(|w| w.is_object() && !b(w, "removed")) {
            println!("{}", c("2", &format!("  The run's separate copy is in {} (agent-graph clean removes it).", tilde(&s(wt, "folder")))));
        }
        let last = arr(&run, "attempts").iter().rev().find(|a| s(a, "status") == "ok" && b(a, "output"));
        if let (Some(last), "succeeded", false) = (last, status.as_str(), quiet) {
            println!("{}", c("2", &format!("── result of “{}” ──", s(last, "name"))));
            println!("{}", last_chars(s(last, "output").trim(), 3000));
        }
        println!("{}", c("2", "Details: open the app (agent-graph) → Runs."));
    }
    if run.get("delivery").is_some_and(|d| b(d, "error")) && status == "succeeded" {
        return 1;
    }
    match status.as_str() {
        "succeeded" => 0,
        "stopped" => 2,
        _ => 1,
    }
}

/// Check workflows without running them: errors stop a workflow from loading; warnings are likely mistakes.
fn cmd_validate(targets: &[String], folder: Option<&str>, strict: bool, as_json: bool) -> i32 {
    let mut items: Vec<(String, Option<Value>, Option<String>)> = vec![]; // (label, raw, error)
    if targets.is_empty() {
        // everything the app knows, including files it couldn't load
        let (raw_all, load_errors) = wfstore::load_all();
        for raw in raw_all {
            items.push((s(&raw, "file"), Some(raw), None));
        }
        for e in load_errors {
            items.push((if b(&e, "file") { s(&e, "file") } else { s(&e, "name") }, None, Some(s(&e, "error"))));
        }
        if items.is_empty() {
            println!("No workflows found. Pass a file: agent-graph validate path/to/workflow.yaml");
            return 0;
        }
    }
    for t in targets {
        if is_file_arg(t) {
            match workflows::read_file(t, folder) {
                Ok((raw, path)) => items.push((path, Some(raw), None)),
                Err(e) => items.push((abspath(t), None, Some(e))),
            }
        } else {
            let wf = find(t);
            match workflows::read_file(&s(&wf, "file"), folder) {
                Ok((raw, path)) => items.push((path, Some(raw), None)),
                Err(e) => items.push((s(&wf, "file"), None, Some(e))),
            }
        }
    }
    let mut report = vec![];
    let mut bad = 0;
    for (label, raw, mut error) in items {
        let mut entry = json!({"file": label, "ok": false, "errors": [], "warnings": []});
        if let (None, Some(raw)) = (&error, &raw) {
            match workflows::validate(raw) {
                Ok(wf) => {
                    entry["name"] = wf["name"].clone();
                    entry["steps"] = json!(arr(&wf, "steps").len());
                    entry["cwd"] = wf["cwd"].clone();
                    entry["warnings"] = json!(workflows::lint(raw, &wf));
                    entry["ok"] = json!(true);
                }
                Err(e) => error = Some(e),
            }
        }
        if let Some(e) = error {
            entry["errors"].as_array_mut().unwrap().push(json!(e));
            entry["name"] = raw.as_ref().and_then(|r| r.get("name").cloned()).unwrap_or(Value::Null);
        }
        if strict && !arr(&entry, "warnings").is_empty() {
            entry["ok"] = json!(false);
        }
        if !b(&entry, "ok") {
            bad += 1;
        }
        report.push(entry);
    }
    if as_json {
        print_json(&json!(report));
        return if bad > 0 { 1 } else { 0 };
    }
    for e in &report {
        let place = tilde(&s(e, "file"));
        if !arr(e, "errors").is_empty() {
            println!("{} {place}", c("31;1", "✗"));
            for m in arr(e, "errors") {
                println!("    {} {}", c("31", "error:"), py_str(m));
            }
        } else {
            let mark = if arr(e, "warnings").is_empty() { c("32;1", "✓") } else { c("33;1", "⚠") };
            let n = e["steps"].as_u64().unwrap_or(0);
            let detail = format!("{place} · {n} step{} · runs in {}", if n == 1 { "" } else { "s" }, tilde(&s(e, "cwd")));
            println!("{mark} {}  {}", s(e, "name"), c("2", &detail));
        }
        for m in arr(e, "warnings") {
            println!("    {} {}", c("33", "warning:"), py_str(m));
        }
    }
    let ok = report.iter().filter(|e| b(e, "ok")).count();
    let warnings: usize = report.iter().map(|e| arr(e, "warnings").len()).sum();
    let mut summary = format!("{ok} of {} valid", report.len());
    if warnings > 0 {
        summary += &format!(" ({warnings} warning(s))");
    }
    let hint = if !strict && warnings > 0 { c("2", "  · --strict treats warnings as errors") } else { String::new() };
    println!("{}{hint}", c("1", &summary));
    if bad > 0 { 1 } else { 0 }
}

pub fn main(args: Vec<String>) -> i32 {
    let cli = Cli::parse_from(std::iter::once("agent-graph".to_string()).chain(args));
    match cli.cmd {
        Cmd::List { json } => cmd_list(json),
        Cmd::Run { workflow, folder, yes, quiet, json, input, inputs, isolation } => {
            cmd_run(&workflow, folder.as_deref(), yes, quiet, json, &input, inputs.as_deref(), isolation.as_deref())
        }
        Cmd::Clean => cmd_clean(),
        Cmd::Validate { targets, folder, strict, json } => cmd_validate(&targets, folder.as_deref(), strict, json),
        Cmd::Runs { limit, json } => cmd_runs(limit, json),
    }
}
