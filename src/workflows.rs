//! Workflow validation and loading, the IDE integration, the permissions editor backend and prompt rewriting.
//!
//! A workflow is a list of steps run as headless `claude -p` sessions (or shell commands). Each step has a
//! prompt, a retry count, an optional shell check and AI judge, and directions for where to go on success /
//! failure. The runner lives in runner.rs.

use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;
use serde_json::{json, Map, Value};

use crate::util::*;
use crate::watcher::{file_tool, parse_ts};
use crate::{compat, providers, quality, template, wfstore, worktree, yamldoc};

pub const PERMISSION_MODES: [&str; 6] = ["acceptEdits", "auto", "dontAsk", "plan", "manual", "bypassPermissions"];
pub const BASH_TIMEOUT: i64 = 1800; // default limit for shell steps (seconds)

fn claude_settings() -> String {
    join(&claude_dir(), "settings.json")
}

fn agent_name_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new("^[a-z][a-z0-9-]{0,39}$").unwrap())
}

// ---- permissions (~/.claude/settings.json) ------------------------------------------------------

pub fn read_permissions() -> Res<Value> {
    let path = claude_settings();
    let settings: Value = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| e.to_string())?,
        Err(_) => json!({}),
    };
    let perms = settings.get("permissions").filter(|p| truthy(p)).cloned().unwrap_or(json!({}));
    let get = |k: &str, d: Value| perms.get(k).cloned().unwrap_or(d);
    Ok(json!({"path": path, "modes": PERMISSION_MODES, "defaultMode": get("defaultMode", json!("")),
              "allow": get("allow", json!([])), "ask": get("ask", json!([])), "deny": get("deny", json!([])),
              "additionalDirectories": get("additionalDirectories", json!([]))}))
}

pub fn write_permissions(p: &Value) -> Res<Value> {
    let path = claude_settings();
    // re-read so edits made elsewhere aren't lost
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{e}: '{path}'"))?;
    let mut settings: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    std::fs::copy(&path, format!("{path}.agent-graph.bak")).map_err(|e| e.to_string())?;
    let mut perms = settings.get("permissions").and_then(Value::as_object).cloned().unwrap_or_default();
    for key in ["allow", "ask", "deny", "additionalDirectories"] {
        let mut rules: Vec<String> = vec![];
        for r in arr(p, key) {
            let r = py_str(r).trim().to_string();
            if !r.is_empty() && !rules.contains(&r) {
                rules.push(r);
            }
        }
        if rules.is_empty() {
            perms.shift_remove(key);
        } else {
            perms.insert(key.into(), json!(rules));
        }
    }
    let mode = s(p, "defaultMode");
    if mode.is_empty() {
        perms.shift_remove("defaultMode");
    } else {
        if !PERMISSION_MODES.contains(&mode.as_str()) {
            return Err(format!("unknown mode {mode}"));
        }
        perms.insert("defaultMode".into(), json!(mode));
    }
    let obj = settings.as_object_mut().ok_or("settings.json isn't an object")?;
    if perms.is_empty() {
        obj.shift_remove("permissions");
    } else {
        obj.insert("permissions".into(), Value::Object(perms));
    }
    write_json(&path, &settings)?;
    read_permissions()
}

// ---- workflow storage -------------------------------------------------------------------------------

pub fn list_workflows(errors: Option<&mut Vec<Value>>) -> Vec<Value> {
    let (raw, mut errs) = wfstore::load_all();
    let mut out = vec![];
    for wf in raw {
        match validate(&wf) {
            Ok(v) => out.push(v),
            Err(e) => errs.push(json!({"file": wf["file"], "name": basename(&s(&wf, "file")), "error": e})),
        }
    }
    if let Some(errors) = errors {
        errors.extend(errs);
    }
    out
}

pub fn find_workflow(wid: &str) -> Option<Value> {
    list_workflows(None).into_iter().find(|w| w["id"] == wid)
}

// ---- workflow files given on the command line --------------------------------------------------------

const WF_KEYS: [&str; 18] = ["id", "name", "cwd", "model", "permissionMode", "allowedTools", "disallowedTools", "passOutput",
    "maxBudgetUsd", "judgeModel", "agents", "steps", "updated", "file", "inputs", "isolation", "worktree", "deliver"];
const INPUT_KEYS: [&str; 5] = ["description", "type", "required", "default", "options"];
const WORKTREE_KEYS: [&str; 4] = ["base", "keep", "setup", "copy"];
const DELIVER_KEYS: [&str; 6] = ["branch", "commit", "message", "push", "pr", "when"];
const PR_KEYS: [&str; 4] = ["draft", "title", "body", "base"];
const STEP_KEYS: [&str; 16] = ["id", "name", "kind", "run", "timeout", "prompt", "retries", "check", "model", "dependsOn",
    "onSuccess", "onFailure", "loopBack", "agents", "review", "judge"];
const AGENT_KEYS: [&str; 5] = ["name", "description", "prompt", "tools", "model"];

/// A workflow from any YAML file, not only the ones the app knows. Returns (raw dict, path).
/// cwd: `folder` if given; else the file's cwd, relative to the file; else its project (…/.claude/workflows/x.yaml)
/// or the file's own folder.
pub fn read_file(path: &str, folder: Option<&str>) -> Res<(Value, String)> {
    let path = abspath(&expanduser(path));
    if !is_file(&path) {
        return Err(format!("No such file: {path}"));
    }
    let stem = std::path::Path::new(&path).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let wid = if wfstore::id_re().is_match(&stem) { stem.clone() } else { slug(&stem, "step", 40) };
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    let text = String::from_utf8(bytes).map_err(|_| "The file isn't text (expected a YAML workflow).".to_string())?;
    let mut wf = wfstore::from_doc(yamldoc::parse(&text)?, &wid)?;
    let here = dirname(&path);
    if let Some(folder) = folder.filter(|f| !f.is_empty()) {
        wf["cwd"] = json!(abspath(&expanduser(folder)));
    } else if b(&wf, "cwd") {
        let cwd = expanduser(&s(&wf, "cwd"));
        wf["cwd"] = json!(if std::path::Path::new(&cwd).is_absolute() { cwd } else { normpath(&join(&here, &cwd)) });
    } else {
        let p = std::path::Path::new(&here);
        let in_project = p.file_name().is_some_and(|n| n == "workflows") && p.parent().and_then(|q| q.file_name()).is_some_and(|n| n == ".claude");
        wf["cwd"] = json!(if in_project { dirname(&dirname(&here)) } else { here.clone() });
    }
    wf["file"] = json!(path);
    wf["updated"] = json!(mtime(&path).unwrap_or(0.0));
    Ok((wf, path))
}

/// A validated, ready-to-run workflow from a YAML file.
pub fn load_file(path: &str, folder: Option<&str>) -> Res<Value> {
    validate(&read_file(path, folder)?.0)
}

/// difflib.SequenceMatcher(None, a, b).ratio()
fn ratio(a: &str, b: &str) -> f64 {
    fn matches(a: &[char], b: &[char]) -> usize {
        if a.is_empty() || b.is_empty() {
            return 0;
        }
        let (mut best, mut bi, mut bj) = (0, 0, 0);
        let mut prev = vec![0usize; b.len() + 1];
        for i in 0..a.len() {
            let mut cur = vec![0usize; b.len() + 1];
            for j in 0..b.len() {
                if a[i] == b[j] {
                    cur[j + 1] = prev[j] + 1;
                    if cur[j + 1] > best {
                        best = cur[j + 1];
                        bi = i + 1 - best;
                        bj = j + 1 - best;
                    }
                }
            }
            prev = cur;
        }
        if best == 0 {
            return 0;
        }
        best + matches(&a[..bi], &b[..bj]) + matches(&a[bi + best..], &b[bj + best..])
    }
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.len() + b.len() == 0 {
        return 1.0;
    }
    2.0 * matches(&a, &b) as f64 / (a.len() + b.len()) as f64
}

fn close_match(word: &str, options: &[&str]) -> Option<String> {
    let mut scored: Vec<(f64, &str)> = options.iter().map(|o| (ratio(word, o), *o)).filter(|(r, _)| *r >= 0.6).collect();
    scored.sort_by(|x, y| y.0.total_cmp(&x.0).then(y.1.cmp(x.1)));
    scored.first().map(|(_, o)| o.to_string())
}

/// Things that don't stop a workflow from loading but are probably mistakes. raw: as written; wf: validated.
pub fn lint(raw: &Value, wf: &Value) -> Vec<String> {
    let mut warn = vec![];
    let unknown = |keys: Vec<&String>, allowed: &[&str], place: &str, warn: &mut Vec<String>| {
        for k in keys {
            if !allowed.contains(&k.as_str()) {
                warn.push(match close_match(k, allowed) {
                    Some(g) => format!("{place}: unknown setting “{k}” (did you mean “{g}”?)"),
                    None => format!("{place}: unknown setting “{k}” (it's ignored)"),
                });
            }
        }
    };
    if let Some(o) = raw.as_object() {
        unknown(o.keys().collect(), &WF_KEYS, "workflow", &mut warn);
    }
    if let Some(o) = raw.get("inputs").and_then(Value::as_object) {
        for (name, spec) in o {
            if let Some(spec) = spec.as_object() {
                unknown(spec.keys().collect(), &INPUT_KEYS, &format!("input “{name}”"), &mut warn);
            }
        }
    }
    if let Some(o) = raw.get("worktree").and_then(Value::as_object) {
        unknown(o.keys().collect(), &WORKTREE_KEYS, "worktree", &mut warn);
    }
    if let Some(o) = raw.get("deliver").and_then(Value::as_object) {
        unknown(o.keys().collect(), &DELIVER_KEYS, "deliver", &mut warn);
        if let Some(pr) = o.get("pr").and_then(Value::as_object) {
            unknown(pr.keys().collect(), &PR_KEYS, "deliver.pr", &mut warn);
        }
    }
    if b(wf, "worktree") && s(wf, "isolation") != "worktree" && raw.get("worktree").is_some() {
        warn.push("workflow: “worktree” settings are ignored unless isolation is worktree".into());
    }
    for a in arr(raw, "agents") {
        if let Some(o) = a.as_object() {
            unknown(o.keys().collect(), &AGENT_KEYS, &format!("helper “{}”", o.get("name").map(py_str).unwrap_or("?".into())), &mut warn);
        }
    }
    for (i, st) in arr(raw, "steps").iter().enumerate() {
        if let Some(o) = st.as_object() {
            unknown(o.keys().collect(), &STEP_KEYS, &format!("step {} (“{}”)", i + 1, o.get("name").map(py_str).unwrap_or("?".into())), &mut warn);
        }
    }
    let mut models: Vec<(String, String)> = vec![("workflow".into(), s(wf, "model"))];
    for st in arr(wf, "steps") {
        if b(st, "model") {
            models.push((format!("step “{}”", s(st, "name")), s(st, "model")));
        }
    }
    models.sort();
    models.dedup();
    for (place, model) in models {
        let (prov, name) = providers::split(&model);
        if prov == "claude" {
            if !name.is_empty() && !providers::CLAUDE_MODELS.contains(&name.as_str()) && !name.starts_with("claude-") {
                warn.push(format!("{place}: “{name}” isn't a Claude model name I know (haiku, sonnet, opus, fable, or a full claude-… id)"));
            }
            continue;
        }
        if let Err(e) = providers::check_ready(&prov, &name) {
            warn.push(format!("{place}: model “{model}” can't run on this computer yet: {e}"));
        }
    }
    static HOLE: OnceLock<Regex> = OnceLock::new();
    let hole = HOLE.get_or_init(|| Regex::new(r"<describe [^<>\n]*>|\{task\}").unwrap());
    for st in arr(wf, "steps") {
        if let Some(m) = hole.find(&s(st, "prompt")) {
            warn.push(format!("step “{}”: still contains “{}”, a part meant to be filled in", s(st, "name"), m.as_str()));
        }
    }
    let mut texts: Vec<String> = arr(wf, "steps").iter().flat_map(|st| ["prompt", "run", "check", "judge"].map(|k| s(st, k))).collect();
    texts.push(wf["deliver"].to_string());
    for name in wf.get("inputs").and_then(Value::as_object).map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default() {
        let re = Regex::new(&format!(r"inputs\s*\.\s*{}\b", regex::escape(&name))).unwrap();
        let env = format!("INPUT_{}", name.to_uppercase());
        if !texts.iter().any(|t| re.is_match(t) || t.contains(&env)) {
            warn.push(format!("input “{name}” is defined but nothing uses it (write {{{{ inputs.{name} }}}} in a prompt, or ${env} in a command)"));
        }
    }
    let used: HashSet<String> = arr(wf, "steps").iter().flat_map(|st| str_list(st, "agents")).collect();
    for a in arr(wf, "agents") {
        if !used.contains(&s(a, "name")) {
            warn.push(format!("helper “{}” is defined but no step uses it", s(a, "name")));
        }
    }
    warn
}

// ---- IDEs ---------------------------------------------------------------------------------------------

// (command, name, style): "vscode" takes `folder -g file`, the rest take `folder file` or just a path
const IDES: [(&str, &str, &str); 13] = [
    ("code", "VS Code", "vscode"),
    ("antigravity-ide-snap.antigravity-ide", "Antigravity", "vscode"),
    ("antigravity", "Antigravity", "vscode"),
    ("cursor", "Cursor", "vscode"),
    ("windsurf", "Windsurf", "vscode"),
    ("codium", "VSCodium", "vscode"),
    ("rider", "Rider", "jetbrains"),
    ("pycharm", "PyCharm", "jetbrains"),
    ("idea", "IntelliJ IDEA", "jetbrains"),
    ("webstorm", "WebStorm", "jetbrains"),
    ("android-studio", "Android Studio", "jetbrains"),
    ("zed", "Zed", "plain"),
    ("subl", "Sublime Text", "plain"),
];

pub fn list_ides() -> Value {
    let mut seen = HashSet::new();
    let mut out = vec![];
    for (cmd, name, _) in IDES {
        if !seen.contains(name) && which(cmd).is_some() {
            seen.insert(name);
            out.push(json!({"id": cmd, "name": name}));
        }
    }
    let chosen = wfstore::read_settings().get("ide").cloned().unwrap_or(Value::Null);
    let default = if out.iter().any(|i| i["id"] == chosen) { chosen } else { out.first().map(|i| i["id"].clone()).unwrap_or(Value::Null) };
    json!({"ides": out, "default": default})
}

pub fn set_ide(ide: &Value) -> Res<Value> {
    if !arr(&list_ides(), "ides").iter().any(|i| &i["id"] == ide) {
        return err("That IDE isn't installed.");
    }
    let mut st = wfstore::read_settings();
    st["ide"] = ide.clone();
    write_json(&wfstore::settings_path(), &st)?;
    Ok(list_ides())
}

/// Open a file (inside its project folder when given) or a folder in the chosen IDE. Returns the IDE's name.
pub fn open_in_ide(path: Option<&str>, project: Option<&str>) -> Res<String> {
    let info = list_ides();
    let Some(cmd) = info["default"].as_str().map(str::to_string) else {
        return err("No IDE found. Install VS Code, a JetBrains IDE, Zed or Sublime Text.");
    };
    let style = IDES.iter().find(|(c, _, _)| *c == cmd).map(|x| x.2).unwrap_or("plain");
    let project = project.filter(|p| is_dir(p)).map(str::to_string);
    if path.is_some_and(|p| !exists(p)) {
        return err("File no longer exists.");
    }
    let args: Vec<String> = match path {
        None => vec![project.clone().unwrap_or_else(home)],
        Some(p) if style == "vscode" => project.iter().cloned().chain(["-g".into(), p.into()]).collect(),
        Some(p) if style == "jetbrains" => match &project {
            Some(proj) if p.starts_with(&format!("{proj}{}", sep())) => vec![proj.clone(), p.into()],
            _ => vec![p.into()],
        },
        Some(p) => project.iter().cloned().chain([p.to_string()]).collect(),
    };
    let mut c = Command::new(which(&cmd).ok_or("That IDE isn't installed.")?);
    c.args(args);
    compat::detached(&mut c);
    compat::spawn_bg(&mut c).map_err(|e| e.to_string())?;
    Ok(arr(&info, "ides").iter().find(|i| i["id"] == cmd.as_str()).map(|i| s(i, "name")).unwrap_or(cmd))
}

pub fn workflows_state() -> Value {
    let mut errors = vec![];
    let wfs = list_workflows(Some(&mut errors));
    json!({"workflows": wfs, "errors": errors, "projects": wfstore::project_dirs().iter().map(|p| tilde(p)).collect::<Vec<_>>()})
}

/// Open a workflow's YAML (or a defaults file) in the IDE, with its project folder as the workspace.
pub fn open_workflow_file(wid: &Value, what: &Value) -> Res<Value> {
    let what = what.as_str().unwrap_or("");
    let (target, project);
    if ["agents", "steps", "userAgents", "userSteps"].contains(&what) {
        target = s(&wfstore::default_files(), what);
        if !exists(&target) {
            // first time: start the user's file from a commented template
            std::fs::create_dir_all(dirname(&target)).map_err(|e| e.to_string())?;
            let kind = what[4..].to_lowercase();
            let top = if what == "userAgents" { "categories" } else { "steps" };
            std::fs::write(
                &target,
                format!("# Your additions to the app's defaults (same shape as defaults/{kind}.yaml in the app).\n# Entries with the same name replace the defaults.\n{top}:\n"),
            )
            .map_err(|e| e.to_string())?;
        }
        project = dirname(&target);
    } else {
        target = wfstore::index_get(&py_str(wid)).filter(|p| exists(p)).ok_or("That workflow file doesn't exist.")?;
        project = dirname(&dirname(&dirname(&target)));
    }
    if !list_ides()["default"].is_null() {
        return open_in_ide(Some(&target), Some(&project)).map(Value::from);
    }
    compat::open_path(&target)?;
    Ok(json!(target))
}

// ---- validation --------------------------------------------------------------------------------------------

fn str_items(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => a.iter().map(py_str).map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect(),
        Some(Value::String(t)) => t.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect(),
        _ => vec![],
    }
}

pub fn validate(wf: &Value) -> Res<Value> {
    let name = wf.get("name").map(py_str).unwrap_or_default().trim().to_string();
    if name.is_empty() {
        return err("Workflow needs a name.");
    }
    let cwd = expanduser(&s_or(wf, "cwd", "~"));
    if !is_dir(&cwd) {
        return Err(format!("Folder doesn't exist: {cwd}"));
    }
    let mode = s_or(wf, "permissionMode", "acceptEdits");
    if !PERMISSION_MODES.contains(&mode.as_str()) {
        return Err(format!("Unknown permission mode: {mode}"));
    }
    let mut agents: Vec<Value> = vec![];
    for a in arr(wf, "agents") {
        let aname = a.get("name").map(py_str).unwrap_or_default().trim().to_lowercase();
        if !agent_name_re().is_match(&aname) {
            return Err(format!("Subagent name “{aname}” must be lowercase letters, digits and dashes."));
        }
        if agents.iter().any(|x| x["name"] == aname.as_str()) {
            return Err(format!("Two subagents are called “{aname}”."));
        }
        if s(a, "description").trim().is_empty() || s(a, "prompt").trim().is_empty() {
            return Err(format!("Subagent “{aname}” needs a role description and instructions."));
        }
        agents.push(json!({"name": aname, "description": s(a, "description").trim(), "prompt": s(a, "prompt"),
                           "tools": str_items(a.get("tools")), "model": s(a, "model").trim()}));
    }
    let agent_names: HashSet<String> = agents.iter().map(|a| s(a, "name")).collect();
    let steps = arr(wf, "steps");
    if steps.is_empty() {
        return err("Add at least one step.");
    }
    if let Some(i) = steps.iter().position(|s| !s.is_object()) {
        return Err(format!("Step {} should be a mapping (id, name, prompt…).", i + 1));
    }
    // steps added in the builder get a readable id from their name ("new-…" is a placeholder)
    let mut renames: HashMap<String, String> = HashMap::new();
    let mut new_ids = vec![];
    let mut taken: HashSet<String> = steps.iter().filter(|st| b(st, "id") && !s(st, "id").starts_with("new-")).map(|st| s(st, "id")).collect();
    for (i, st) in steps.iter().enumerate() {
        let old = s(st, "id");
        let mut sid = old.clone();
        if old.is_empty() || old.starts_with("new-") {
            let base = slug(&s_or(st, "name", &format!("step-{}", i + 1)), "step", 40);
            sid = base.clone();
            let mut n = 2;
            while taken.contains(&sid) {
                sid = format!("{base}-{n}");
                n += 1;
            }
            taken.insert(sid.clone());
            if !old.is_empty() {
                renames.insert(old, sid.clone());
            }
        }
        new_ids.push(sid);
    }
    let rn = |x: String| renames.get(&x).cloned().unwrap_or(x);
    static STEP_ID: OnceLock<Regex> = OnceLock::new();
    let step_id = STEP_ID.get_or_init(|| Regex::new("^[A-Za-z0-9_-]{1,60}$").unwrap());
    let mut ids: HashSet<String> = HashSet::new();
    let mut clean: Vec<Value> = vec![];
    for (i, st) in steps.iter().enumerate() {
        let sid = new_ids[i].clone();
        if !step_id.is_match(&sid) {
            return Err(format!("Step id “{sid}” should be letters, digits and dashes."));
        }
        if !ids.insert(sid.clone()) {
            return Err(format!("Duplicate step id {sid}"));
        }
        let label = s_or(st, "name", &(i + 1).to_string());
        let kind_field = st.get("kind").and_then(Value::as_str);
        let kind = if kind_field == Some("bash") || (b(st, "run") && kind_field != Some("claude")) { "bash" } else { "claude" };
        if kind == "bash" && s(st, "run").trim().is_empty() {
            return Err(format!("Step “{label}” is a shell step but has no command (run)."));
        }
        if kind == "claude" && s(st, "prompt").trim().is_empty() {
            return Err(format!("Step {} has no prompt.", i + 1));
        }
        let step_agents: Vec<String> = match st.get("agents") {
            Some(Value::Array(a)) => a.iter().map(py_str).collect(),
            Some(v) if truthy(v) => vec![py_str(v)],
            _ => vec![],
        };
        if let Some(u) = step_agents.iter().find(|n| !agent_names.contains(*n)) {
            return Err(format!("Step “{label}” uses subagent “{u}”, which isn't defined under agents."));
        }
        let depends: Vec<String> = match st.get("dependsOn") {
            Some(Value::Array(a)) => a.iter().map(|d| rn(py_str(d))).collect(),
            Some(v) if truthy(v) => vec![rn(py_str(v))],
            Some(_) => vec![],
            None => clean.last().map(|c| vec![s(c, "id")]).unwrap_or_default(),
        };
        let int_or = |k: &str, d: i64| -> Res<i64> { st.get(k).filter(|v| truthy(v)).map(py_int).unwrap_or(Ok(d)) };
        let on_success = renames.get(&st.get("onSuccess").map(py_str).unwrap_or("None".into())).cloned().unwrap_or_else(|| s_or(st, "onSuccess", "next"));
        let on_failure = renames.get(&st.get("onFailure").map(py_str).unwrap_or("None".into())).cloned().unwrap_or_else(|| s_or(st, "onFailure", "stop"));
        clean.push(json!({
            "id": sid,
            "name": s_or(st, "name", &format!("Step {}", i + 1)).trim(),
            "kind": kind,
            "run": if kind == "bash" { s(st, "run") } else { String::new() },
            "timeout": int_or("timeout", BASH_TIMEOUT)?.clamp(10, 86400),
            "prompt": if kind == "claude" { s(st, "prompt") } else { String::new() },
            "retries": int_or("retries", 0)?.clamp(0, 10),
            "check": s(st, "check").trim(),
            "judge": s(st, "judge").trim(),
            "model": s(st, "model").trim(),
            "dependsOn": depends,
            "onSuccess": on_success,
            "onFailure": on_failure,
            "loopBack": clean_loop(st.get("loopBack"), &renames)?,
            "agents": step_agents,
            "review": b(st, "review"),
        }));
    }
    for st in clean.iter_mut() {
        let name = s(st, "name");
        for d in str_list(st, "dependsOn") {
            if !ids.contains(&d) {
                return Err(format!("Step “{name}” depends on a missing step ({d})."));
            }
            if d == s(st, "id") {
                return Err(format!("Step “{name}” can't depend on itself."));
            }
        }
        // older files: onFailure/onSuccess pointing at a step id were jumps; express them as loop-backs
        if st["onFailure"] == "next" {
            st["onFailure"] = json!("continue");
        }
        let of = s(st, "onFailure");
        if of != "stop" && of != "continue" {
            if !ids.contains(&of) {
                return Err(format!("Step “{name}” points to a missing step ({of})."));
            }
            if st["loopBack"].is_null() {
                st["loopBack"] = json!({"to": of, "when": "failure", "max": 3});
            }
            st["onFailure"] = json!("stop");
        }
        let os = s(st, "onSuccess");
        if os != "next" && os != "end" {
            if !ids.contains(&os) {
                return Err(format!("Step “{name}” points to a missing step ({os})."));
            }
            if st["loopBack"].is_null() {
                st["loopBack"] = json!({"to": os, "when": "success", "max": 3});
            }
            st["onSuccess"] = json!("next");
        }
    }
    topo_order(&clean)?; // fails on cycles
    let by_id: HashMap<String, &Value> = clean.iter().map(|st| (s(st, "id"), st)).collect();
    for st in &clean {
        if let Some(lb) = st.get("loopBack").filter(|l| l.is_object()) {
            let to = s(lb, "to");
            if !ids.contains(&to) {
                return Err(format!("Step “{}” loops back to a missing step ({to}).", s(st, "name")));
            }
            if to != s(st, "id") && !ancestors(&by_id, &s(st, "id")).contains(&to) {
                return Err(format!(
                    "Step “{}” can only loop back to itself or a step it depends on (directly or indirectly), not “{}”.",
                    s(st, "name"),
                    s(by_id[&to], "name")
                ));
            }
        }
    }
    let inputs = template::clean_inputs(wf.get("inputs"))?;
    let step_ids: Vec<String> = clean.iter().map(|st| s(st, "id")).collect();
    for st in &clean {
        for k in ["prompt", "run", "check", "judge"] {
            template::check(&s(st, k), &inputs, &step_ids).map_err(|e| format!("Step “{}”, {k}: {e}", s(st, "name")))?;
        }
    }
    let isolation = s_or(wf, "isolation", "none");
    match isolation.as_str() {
        "none" | "worktree" => {}
        "container" => return err("isolation: container isn't supported yet. Use worktree."),
        other => return Err(format!("Unknown isolation “{other}” (use none or worktree).")),
    }
    let worktree = clean_worktree(wf.get("worktree"))?;
    if isolation == "worktree" && !quality::is_repo(&cwd) {
        return Err(format!("isolation: worktree needs a git repository, and {} isn't one (run `git init` and make a first commit).", tilde(&cwd)));
    }
    let deliver = clean_deliver(wf.get("deliver"))?;
    if !deliver.is_null() {
        if isolation != "worktree" {
            return err("deliver needs isolation: worktree, so the branch holds exactly what the run changed.");
        }
        for (k, t) in [("branch", s(&deliver, "branch")), ("message", s(&deliver, "message")), ("pr.title", deliver["pr"].get("title").map(py_str).unwrap_or_default()), ("pr.body", deliver["pr"].get("body").map(py_str).unwrap_or_default())] {
            template::check(&t, &inputs, &step_ids).map_err(|e| format!("deliver.{k}: {e}"))?;
        }
    }
    let budget = match wf.get("maxBudgetUsd") {
        None | Some(Value::Null) => Value::Null,
        Some(Value::String(t)) if t.is_empty() => Value::Null,
        Some(v) => json!(py_float(v)?),
    };
    let id = if b(wf, "id") { wf["id"].clone() } else { json!(wfstore::new_id(&name)) };
    Ok(json!({
        "id": id,
        "name": name,
        "cwd": cwd,
        "model": s(wf, "model").trim(),
        "permissionMode": mode,
        "allowedTools": str_items(wf.get("allowedTools")),
        "disallowedTools": str_items(wf.get("disallowedTools")),
        "passOutput": wf.get("passOutput").map(truthy).unwrap_or(true),
        "maxBudgetUsd": budget,
        "judgeModel": s(wf, "judgeModel").trim(),
        "inputs": inputs,
        "isolation": isolation,
        "worktree": worktree,
        "deliver": deliver,
        "agents": agents,
        "steps": clean,
        "updated": if b(wf, "updated") { wf["updated"].clone() } else { json!(now()) },
        "file": s(wf, "file"),
    }))
}

pub fn worktree_defaults() -> Value {
    json!({"base": "HEAD", "keep": "onFailure", "setup": "", "copy": []})
}

fn clean_worktree(v: Option<&Value>) -> Res<Value> {
    let mut out = worktree_defaults();
    let Some(v) = v.filter(|v| truthy(v)) else { return Ok(out) };
    if !v.is_object() {
        return err("“worktree” should be a mapping (base, keep, setup, copy).");
    }
    for k in ["base", "keep", "setup"] {
        if b(v, k) {
            out[k] = json!(s(v, k).trim());
        }
    }
    if !worktree::KEEP.contains(&s(&out, "keep").as_str()) {
        return Err(format!("worktree keep must be {}.", worktree::KEEP.join(", ")));
    }
    let copy = str_items(v.get("copy"));
    if let Some(bad) = copy.iter().find(|c| std::path::Path::new(c).is_absolute() || c.split(['/', '\\']).any(|p| p == "..")) {
        return Err(format!("worktree copy: “{bad}” should be a path inside the project folder."));
    }
    out["copy"] = json!(copy);
    Ok(out)
}

pub fn pr_defaults() -> Value {
    json!({"draft": true, "title": "", "body": "summary", "base": ""})
}

pub fn deliver_defaults() -> Value {
    json!({"branch": "agent-graph/{{ run.id }}", "commit": "perStep", "message": "", "push": false, "pr": false, "when": "success"})
}

/// deliver: false/absent = nothing; true = a local branch with the defaults; or a mapping.
fn clean_deliver(v: Option<&Value>) -> Res<Value> {
    let Some(v) = v.filter(|v| truthy(v)) else { return Ok(Value::Null) };
    let mut out = deliver_defaults();
    if v == &json!(true) {
        return Ok(out);
    }
    if !v.is_object() {
        return err("“deliver” should be true or a mapping (branch, commit, push, pr, when).");
    }
    for k in ["branch", "message"] {
        if b(v, k) {
            out[k] = json!(s(v, k).trim());
        }
    }
    for (k, allowed) in [("commit", &["perStep", "squash"][..]), ("when", &["success", "always"][..])] {
        if b(v, k) {
            let x = s(v, k);
            if !allowed.contains(&x.as_str()) {
                return Err(format!("deliver {k} must be {}.", allowed.join(" or ")));
            }
            out[k] = json!(x);
        }
    }
    let pr = match v.get("pr") {
        None | Some(Value::Null) | Some(Value::Bool(false)) => Value::Bool(false),
        Some(Value::Bool(true)) => pr_defaults(),
        Some(p) if p.is_object() => {
            let mut pr = pr_defaults();
            for k in ["title", "body", "base"] {
                if b(p, k) {
                    pr[k] = json!(s(p, k).trim());
                }
            }
            if let Some(d) = p.get("draft") {
                pr["draft"] = json!(truthy(d));
            }
            pr
        }
        Some(_) => return err("deliver pr should be true, false or a mapping (draft, title, body, base)."),
    };
    // a pull request needs the branch on the remote
    out["push"] = json!(v.get("push").map(truthy).unwrap_or(false) || pr.is_object());
    out["pr"] = pr;
    Ok(out)
}

fn clean_loop(lb: Option<&Value>, renames: &HashMap<String, String>) -> Res<Value> {
    let Some(lb) = lb.filter(|l| truthy(l)) else { return Ok(Value::Null) };
    let lb = if let Value::String(t) = lb { json!({"to": t}) } else { lb.clone() };
    if !lb.is_object() || !b(&lb, "to") {
        return err("loopBack needs “to: <step id>”.");
    }
    let when = s_or(&lb, "when", "failure");
    if when != "failure" && when != "success" {
        return err("loopBack “when” must be failure or success.");
    }
    let to = s(&lb, "to");
    let max = lb.get("max").filter(|v| truthy(v)).map(py_int).unwrap_or(Ok(3))?;
    Ok(json!({"to": renames.get(&to).cloned().unwrap_or(to), "when": when, "max": max.clamp(1, 20)}))
}

/// Every step that depends on sid, directly or indirectly.
pub fn descendants(steps: &[Value], sid: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut changed = true;
    while changed {
        changed = false;
        for st in steps {
            let id = s(st, "id");
            if !out.contains(&id) && str_list(st, "dependsOn").iter().any(|d| d == sid || out.contains(d)) {
                out.insert(id);
                changed = true;
            }
        }
    }
    out
}

fn ancestors(by_id: &HashMap<String, &Value>, sid: &str) -> HashSet<String> {
    let mut seen = HashSet::new();
    let mut todo = str_list(by_id[sid], "dependsOn");
    while let Some(d) = todo.pop() {
        if seen.insert(d.clone()) {
            todo.extend(str_list(by_id[&d], "dependsOn"));
        }
    }
    seen
}

/// Steps in an order where dependencies come first; fails if the dependencies form a cycle.
pub fn topo_order(steps: &[Value]) -> Res<Vec<String>> {
    let by_id: HashMap<String, &Value> = steps.iter().map(|st| (s(st, "id"), st)).collect();
    let mut state: HashMap<String, bool> = HashMap::new(); // false = visiting, true = done
    let mut order = vec![];
    fn visit(sid: &str, path: &mut Vec<String>, by_id: &HashMap<String, &Value>, state: &mut HashMap<String, bool>, order: &mut Vec<String>) -> Res<()> {
        match state.get(sid) {
            Some(true) => return Ok(()),
            Some(false) => {
                let start = path.iter().position(|x| x == sid).unwrap_or(0);
                let names: Vec<String> = path[start..].iter().chain([&sid.to_string()]).map(|x| s(by_id[x], "name")).collect();
                return Err(format!("The step dependencies form a cycle: {}. Use loopBack to repeat steps.", names.join(" → ")));
            }
            None => {}
        }
        state.insert(sid.into(), false);
        path.push(sid.into());
        for d in str_list(by_id[sid], "dependsOn") {
            visit(&d, path, by_id, state, order)?;
        }
        path.pop();
        state.insert(sid.into(), true);
        order.push(sid.into());
        Ok(())
    }
    for st in steps {
        visit(&s(st, "id"), &mut vec![], &by_id, &mut state, &mut order)?;
    }
    Ok(order)
}

pub fn save_workflow(wf: &Value) -> Res<Value> {
    let base = if b(wf, "id") { wf.get("updated").and_then(Value::as_f64) } else { None };
    let mut wf = wf.clone();
    if !b(&wf, "id") {
        wf["id"] = json!(wfstore::new_id(&s_or(&wf, "name", "workflow")));
    }
    let wf = validate(&wf)?;
    wfstore::write(&wf, base)?;
    find_workflow(&s(&wf, "id")).ok_or_else(|| "The workflow was saved but couldn't be read back.".into())
}

pub fn delete_workflow(wid: &str) -> Res<()> {
    wfstore::remove(wid)
}

// ---- prompt rewriting ------------------------------------------------------------------------------------

const REWRITE_SYSTEM: &str = "You improve text used to instruct AI coding agents in a workflow. Rewrite the user's text so it is clear, \
specific and actionable: state the goal, the concrete deliverable (files, format), and constraints. Keep the \
author's intent, language, tone, and every path, command, name and number exactly. Do not invent requirements \
or facts. Keep it about as long as needed, no longer. NEVER ask questions or ask for details: always return a \
rewrite. Where something essential is missing, insert a short <placeholder in angle brackets> for the author to \
fill in. Output ONLY the rewritten text: no preamble, no explanation, no quotes, no markdown fences.";

fn rewrite_kind(kind: &str) -> &'static str {
    match kind {
        "role" => "This is a subagent's role: ONE short sentence saying what it does and when to use it.",
        "agent" => "These are a subagent's standing instructions (its procedure). Numbered steps work well.",
        _ => "This is the task prompt for one step of a workflow.",
    }
}

/// Polish a prompt with the cheapest model; nothing is saved as a session.
pub fn rewrite_text(text: &Value, kind: &str, context: &str) -> Res<Value> {
    let text = if truthy(text) { py_str(text).trim().to_string() } else { String::new() };
    if text.is_empty() {
        return err("Write something first, then improve it.");
    }
    let ctx = if context.is_empty() { String::new() } else { format!("Context: {context}") };
    let prompt = format!(
        "{}\n{ctx}\n\nRewrite the text between the markers. Reply with the rewritten text only.\n<<<\n{text}\n>>>",
        rewrite_kind(kind)
    );
    let mut cmd = Command::new(compat::claude_cmd());
    cmd.args(["-p", "--model", "haiku", "--output-format", "json", "--no-session-persistence", "--tools", "", "--system-prompt", REWRITE_SYSTEM, "--max-budget-usd", "0.10"])
        .current_dir(home());
    let res = run_capture(cmd, Some(&prompt), Some(Duration::from_secs(120)))
        .ok()
        .filter(|o| o.code.is_some())
        .and_then(|o| last_json_line(&o.stdout))
        .ok_or("The rewrite didn't come back. Try again.")?;
    if b(&res, "is_error") || !b(&res, "result") {
        let why = if b(&res, "result") { s(&res, "result") } else { py_str(res.get("subtype").unwrap_or(&Value::Null)) };
        return Err(format!("Rewrite failed: {}", first_chars(&why, 200)));
    }
    Ok(json!({"text": s(&res, "result").trim(), "cost": res.get("total_cost_usd").cloned().unwrap_or(Value::Null)}))
}

// ---- transcripts as logs ---------------------------------------------------------------------------------

pub fn clip(text: &str, n: usize) -> String {
    let text = text.trim();
    let len = char_len(text);
    if len <= n {
        text.to_string()
    } else {
        format!("{}\n… ({} more characters)", first_chars(text, n), len - n)
    }
}

/// Turn one transcript record into log entries: text, tool calls, tool results.
pub fn log_lines(d: &Value) -> Vec<Value> {
    let typ = d.get("type").and_then(Value::as_str).unwrap_or("");
    if typ != "user" && typ != "assistant" {
        return vec![];
    }
    let t = parse_ts(d.get("timestamp")).unwrap_or(0.0);
    let content = match d.get("message").and_then(|m| m.get("content")) {
        Some(Value::String(text)) => vec![json!({"type": "text", "text": text})],
        Some(Value::Array(a)) => a.clone(),
        _ => vec![],
    };
    let mut out = vec![];
    let text_of = |v: Option<&Value>| match v {
        Some(Value::String(x)) => x.clone(),
        Some(Value::Array(a)) => a.iter().filter(|x| x.is_object()).map(|x| s(x, "text")).collect::<Vec<_>>().join("\n"),
        Some(Value::Null) | None => String::new(),
        Some(other) => py_str(other),
    };
    for blk in content.iter().filter(|x| x.is_object()) {
        let kind = blk.get("type").and_then(Value::as_str).unwrap_or("");
        let body = s(blk, "text");
        if typ == "assistant" && kind == "text" && !body.trim().is_empty() {
            out.push(json!({"t": t, "kind": "text", "title": first_line(&clip(&body, 160)), "body": clip(&body, 4000)}));
        } else if kind == "tool_use" {
            let inp = blk.get("input").cloned().unwrap_or(json!({}));
            let name = s(blk, "name");
            let key = ["command", "file_path", "notebook_path", "pattern", "query", "url", "description", "prompt"].into_iter().find(|k| inp.get(*k).is_some());
            let title = match key {
                Some(k) => format!("{name}: {}", first_line(&clip(&text_of(inp.get(k)), 140))),
                None => name,
            };
            let body = ["content", "new_string", "command", "prompt"]
                .into_iter()
                .find_map(|k| inp.get(k).filter(|v| truthy(v)).map(|v| text_of(Some(v))))
                .unwrap_or_else(|| serde_json::to_string_pretty(&inp).unwrap_or_default());
            out.push(json!({"t": t, "kind": "tool", "title": title, "body": clip(&body, 3000)}));
        } else if kind == "tool_result" {
            let c = text_of(blk.get("content"));
            let title = first_line(&clip(&c, 140));
            out.push(json!({"t": t, "kind": if b(blk, "is_error") { "error" } else { "result" },
                            "title": if title.is_empty() { "(no output)".to_string() } else { title }, "body": clip(&c, 3000)}));
        } else if typ == "user" && kind == "text" && !body.trim().is_empty() && !b(d, "isMeta") {
            out.push(json!({"t": t, "kind": "input", "title": first_line(&clip(&body, 160)), "body": clip(&body, 3000)}));
        }
    }
    out
}

/// Token use of one claude -p call, and how much of its input came from the prompt cache.
pub fn usage(res: &Value) -> Value {
    let u = res.get("usage").cloned().unwrap_or(json!({}));
    let n = |k: &str| u.get(k).filter(|v| truthy(v)).and_then(|v| py_int(v).ok()).unwrap_or(0);
    let (fresh, read, write) = (n("input_tokens"), n("cache_read_input_tokens"), n("cache_creation_input_tokens"));
    let total = fresh + read + write;
    json!({"input": total, "output": n("output_tokens"), "cacheRead": read, "cacheWrite": write,
           "cacheHit": if total > 0 { json!(round3(read as f64 / total as f64)) } else { Value::Null },
           "turns": res.get("num_turns").cloned().unwrap_or(Value::Null), "apiMs": res.get("duration_api_ms").cloned().unwrap_or(Value::Null)})
}

/// A step session's transcript files: the session's own and its subagents'.
fn session_files(session: &str, subagents_only: bool) -> Vec<String> {
    let root = crate::watcher::projects_dir();
    let mut out = vec![];
    for proj in list_dir(&root) {
        let p = join(&root, &proj);
        let own = join(&p, &format!("{session}.jsonl"));
        if !subagents_only && is_file(&own) {
            out.push(own);
        }
        let sub = join(&join(&p, session), "subagents");
        out.extend(list_dir(&sub).into_iter().map(|n| join(&sub, &n)));
    }
    out
}

/// Files a step session (and its subagents) wrote or edited, read straight from its transcripts.
pub fn written_files(session: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for path in session_files(session, false).into_iter().filter(|p| p.ends_with(".jsonl")) {
        let Ok(text) = read_lossy(&path) else { continue };
        for line in text.lines().filter(|l| l.contains("\"tool_use\"")) {
            let Ok(d) = serde_json::from_str::<Value>(line) else { continue };
            for blk in d.get("message").map(|m| arr(m, "content")).unwrap_or(&[]) {
                if blk.get("type").and_then(Value::as_str) == Some("tool_use") && file_tool(&s(blk, "name")).is_some() {
                    let inp = blk.get("input").cloned().unwrap_or(json!({}));
                    let p = if b(&inp, "file_path") { s(&inp, "file_path") } else { s(&inp, "notebook_path") };
                    if !p.is_empty() && !out.contains(&p) {
                        out.push(p);
                    }
                }
            }
        }
    }
    out
}

/// Subagent types a step's session actually ran, read from its transcript folder.
pub fn agents_used(session: &str) -> HashSet<String> {
    session_files(session, true)
        .into_iter()
        .filter(|p| p.ends_with(".meta.json"))
        .filter_map(|p| read_json(&p))
        .map(|m| s(&m, "agentType"))
        .collect()
}

/// The JSON object with these keys, in this order (missing ones as null).
pub fn pick(o: &Value, keys: &[&str]) -> Map<String, Value> {
    keys.iter().map(|k| (k.to_string(), o.get(*k).cloned().unwrap_or(Value::Null))).collect()
}
