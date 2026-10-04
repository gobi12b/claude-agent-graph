//! Workflow and defaults storage, all as YAML files you can edit by hand or from the app.
//!
//! Two stores:
//!   - App store: defaults/agents.yaml (the subagent library), defaults/steps.yaml (step templates) and
//!     defaults/samples.yaml. Your additions/overrides go in ~/.config/claude-agent-graph/{agents,steps}.yaml.
//!   - Project store: each workflow lives in its project folder, <project>/.claude/workflows/<id>.yaml,
//!     so it is versioned with the code. Projects are found from the folders you saved workflows to and
//!     from your Claude Code sessions' working folders.
//!
//! The app re-reads files whenever they change on disk. Saves from the builder merge into the existing
//! file, so your comments and layout survive.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{LazyLock, Mutex, OnceLock, RwLock};

use regex::Regex;
use serde_json::{json, Map, Value};

use crate::util::*;
use crate::yamldoc;

pub const HEADER: &str = "\
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
  inputs: values asked for when the workflow runs, used as {{ inputs.<name> }} in prompts, commands,
          checks and judge criteria. Per input: description, type (string | text | number | boolean |
          choice), required, default, options (for choice). Shorthand: `bug: What's going wrong?`
          Templates can also use {{ steps.<id>.output }}, {{ run.id }}, {{ run.folder }} and the filters
          default(…), slug, trim and json. In commands and checks each value is safely quoted.
  isolation: worktree   run in a separate git worktree, so your folder and other runs are untouched
    worktree: {base: HEAD, keep: always | onFailure | never, setup: shell command, copy: [.env]}
  deliver: turn the run's changes into a branch, one commit per step (needs isolation: worktree)
    {branch: agent-graph/{{ run.id }}, commit: perStep | squash, when: success | always,
     push: true, pr: {draft: true, title: …, body: summary, base: main}}
";

pub fn id_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new("^[a-z0-9][a-z0-9-]{0,60}$").unwrap())
}

fn agent_name_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new("^[a-z][a-z0-9-]{0,39}$").unwrap())
}

pub fn project_subdir() -> String {
    join(".claude", "workflows")
}
pub fn user_agents() -> String {
    join(&config_dir(), "agents.yaml")
}
pub fn user_steps() -> String {
    join(&config_dir(), "steps.yaml")
}
pub fn settings_path() -> String {
    join(&config_dir(), "settings.json")
}
fn legacy_dir() -> String {
    std::env::var("AGENT_GRAPH_WORKFLOW_DIR").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| join(&config_dir(), "workflows"))
}
fn trash_dir() -> String {
    join(&config_dir(), "deleted")
}

// ---- the built-in defaults (shipped inside the binary) ----------------------------------------

const BUILTIN: [(&str, &str); 3] = [
    ("agents.yaml", include_str!("../defaults/agents.yaml")),
    ("steps.yaml", include_str!("../defaults/steps.yaml")),
    ("samples.yaml", include_str!("../defaults/samples.yaml")),
];

/// The folder with the built-in agents.yaml / steps.yaml / samples.yaml. They are real files so they can be
/// opened in your IDE: $AGENT_GRAPH_DEFAULTS, a `defaults` folder next to the program, the source checkout it
/// was built from, or else a copy written to ~/.config/claude-agent-graph/builtin (refreshed on upgrade).
pub fn defaults_dir() -> String {
    static DIR: OnceLock<String> = OnceLock::new();
    DIR.get_or_init(|| {
        let has_all = |d: &str| BUILTIN.iter().all(|(n, _)| is_file(&join(d, n)));
        if let Ok(d) = std::env::var("AGENT_GRAPH_DEFAULTS") {
            if has_all(&d) {
                return d;
            }
        }
        if let Some(exe_dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.to_string_lossy().into_owned())) {
            for d in [join(&exe_dir, "defaults"), join(&exe_dir, "../share/claude-agent-graph/defaults")] {
                if has_all(&d) {
                    return normpath(&d);
                }
            }
        }
        let source = concat!(env!("CARGO_MANIFEST_DIR"), "/defaults");
        if has_all(source) {
            return source.to_string();
        }
        let dir = join(&config_dir(), "builtin");
        let stamp = join(&dir, ".version");
        let fresh = std::fs::read_to_string(&stamp).map(|v| v.trim() == env!("CARGO_PKG_VERSION")).unwrap_or(false);
        let _ = std::fs::create_dir_all(&dir);
        for (name, text) in BUILTIN {
            let path = join(&dir, name);
            if !fresh || !is_file(&path) {
                let _ = std::fs::write(&path, text);
            }
        }
        let _ = std::fs::write(&stamp, env!("CARGO_PKG_VERSION"));
        dir
    })
    .clone()
}

// ---- workflow dict <-> YAML document ------------------------------------------------------------

fn step_defaults() -> [(&'static str, Value); 9] {
    [
        ("model", json!("")),
        ("retries", json!(0)),
        ("check", json!("")),
        ("judge", json!("")),
        ("review", json!(false)),
        ("agents", json!([])),
        ("onSuccess", json!("next")),
        ("onFailure", json!("stop")),
        ("loopBack", Value::Null),
    ]
}

/// The tidy shape written to disk: defaults left out, subagents keyed by name.
pub fn to_doc(wf: &Value) -> Value {
    let mut d = Map::new();
    d.insert("name".into(), wf["name"].clone());
    d.insert("cwd".into(), json!(tilde(&py_str(&wf["cwd"]))));
    if b(wf, "model") {
        d.insert("model".into(), wf["model"].clone());
    }
    d.insert("permissionMode".into(), wf["permissionMode"].clone());
    if !wf.get("passOutput").map(truthy).unwrap_or(true) {
        d.insert("passOutput".into(), json!(false));
    }
    for k in ["allowedTools", "disallowedTools"] {
        if b(wf, k) {
            d.insert(k.into(), wf[k].clone());
        }
    }
    if wf.get("maxBudgetUsd").is_some_and(|v| !v.is_null()) {
        d.insert("maxBudgetUsd".into(), wf["maxBudgetUsd"].clone());
    }
    if b(wf, "judgeModel") {
        d.insert("judgeModel".into(), wf["judgeModel"].clone());
    }
    if wf.get("inputs").is_some_and(|i| i.as_object().is_some_and(|o| !o.is_empty())) {
        let mut inputs = Map::new();
        for (name, spec) in wf["inputs"].as_object().unwrap() {
            let mut out = strip_defaults(spec, &json!({"description": "", "type": "string", "required": false, "default": null, "options": []}));
            // `name: description` for a required text input with nothing else set
            let only_desc = out.len() == 2 && out.get("required") == Some(&json!(true)) && out.contains_key("description");
            inputs.insert(name.clone(), if only_desc { out.remove("description").unwrap() } else { Value::Object(out) });
        }
        d.insert("inputs".into(), Value::Object(inputs));
    }
    if s(wf, "isolation") == "worktree" {
        d.insert("isolation".into(), json!("worktree"));
        let wt = strip_defaults(&wf["worktree"], &crate::workflows::worktree_defaults());
        if !wt.is_empty() {
            d.insert("worktree".into(), Value::Object(wt));
        }
    }
    if wf.get("deliver").is_some_and(Value::is_object) {
        let mut dl = strip_defaults(&wf["deliver"], &crate::workflows::deliver_defaults());
        if wf["deliver"]["pr"].is_object() {
            dl.remove("push"); // implied by pr
            let pr = strip_defaults(&wf["deliver"]["pr"], &crate::workflows::pr_defaults());
            dl.insert("pr".into(), if pr.is_empty() { json!(true) } else { Value::Object(pr) });
        }
        d.insert("deliver".into(), if dl.is_empty() { json!(true) } else { Value::Object(dl) });
    }
    if b(wf, "agents") {
        let mut agents = Map::new();
        for a in arr(wf, "agents") {
            let mut spec = Map::new();
            for k in ["description", "model", "tools", "prompt"] {
                if b(a, k) {
                    spec.insert(k.into(), a[k].clone());
                }
            }
            agents.insert(py_str(&a["name"]), Value::Object(spec));
        }
        d.insert("agents".into(), Value::Object(agents));
    }
    let mut steps = vec![];
    let mut prev: Option<String> = None;
    for s in arr(wf, "steps") {
        let mut out = Map::new();
        out.insert("id".into(), s["id"].clone());
        out.insert("name".into(), s["name"].clone());
        let default_deps: Vec<Value> = prev.iter().map(|p| json!(p)).collect();
        if let Some(deps) = s.get("dependsOn") {
            if deps.as_array().map(|a| a != &default_deps).unwrap_or(true) {
                out.insert("dependsOn".into(), deps.clone());
            }
        }
        let bash = s.get("kind").and_then(Value::as_str) == Some("bash");
        if bash {
            // shell step: just the command (no Claude, no cost)
            out.insert("run".into(), s["run"].clone());
            if s.get("timeout").is_some_and(|t| truthy(t) && t.as_i64() != Some(1800)) {
                out.insert("timeout".into(), s["timeout"].clone());
            }
        } else {
            out.insert("prompt".into(), s["prompt"].clone());
        }
        for (k, default) in step_defaults() {
            let v = s.get(k).cloned().unwrap_or_else(|| default.clone());
            if v != default && !(bash && (k == "model" || k == "agents")) {
                out.insert(k.into(), v);
            }
        }
        steps.push(Value::Object(out));
        prev = Some(py_str(&s["id"]));
    }
    d.insert("steps".into(), Value::Array(steps));
    Value::Object(d)
}

/// The keys of a mapping whose values differ from the defaults.
fn strip_defaults(v: &Value, defaults: &Value) -> Map<String, Value> {
    v.as_object()
        .map(|o| o.iter().filter(|(k, x)| defaults.get(k.as_str()) != Some(*x)).map(|(k, x)| (k.clone(), x.clone())).collect())
        .unwrap_or_default()
}

/// Accepts the tidy shape (and the older JSON shape) and returns the app's workflow dict.
pub fn from_doc(doc: Value, wid: &str) -> Res<Value> {
    let Value::Object(mut wf) = doc else {
        return err("The file should be a mapping with name, cwd and steps.");
    };
    let mut agents: Vec<Value> = match wf.get("agents") {
        Some(Value::Object(m)) => m
            .iter()
            .map(|(name, spec)| {
                let mut a = Map::new();
                a.insert("name".into(), json!(name));
                if let Value::Object(spec) = spec {
                    a.extend(spec.clone());
                }
                Value::Object(a)
            })
            .collect(),
        Some(Value::Array(a)) => a.clone(),
        _ => vec![],
    };
    for a in agents.iter_mut() {
        if let Some(Value::String(t)) = a.get("tools").cloned() {
            a["tools"] = json!(t.split(',').map(str::trim).collect::<Vec<_>>());
        }
    }
    wf.insert("agents".into(), Value::Array(agents));
    let Some(Value::Array(steps)) = wf.get_mut("steps") else {
        return err("“steps” should be a list.");
    };
    for (i, s) in steps.iter_mut().enumerate() {
        let Value::Object(s) = s else {
            return Err(format!("Step {} should be a mapping (id, name, prompt…).", i + 1));
        };
        if let Some(Value::String(a)) = s.get("agents").cloned() {
            s.insert("agents".into(), json!([a]));
        }
        if !opt_truthy(s.get("id")) {
            let base = s.get("name").filter(|n| truthy(n)).map(py_str).unwrap_or_else(|| format!("step-{}", i + 1));
            s.insert("id".into(), json!(slug(&base, "step", 40)));
        }
    }
    wf.insert("id".into(), json!(wid));
    Ok(Value::Object(wf))
}

// ---- settings and projects --------------------------------------------------------------------------

type Hint = Box<dyn Fn() -> HashSet<String> + Send + Sync>;
static PROJECT_HINT: RwLock<Option<Hint>> = RwLock::new(None);

/// Set by the app: folders of Claude Code sessions, where workflows may live.
pub fn set_project_hint(f: Hint) {
    *PROJECT_HINT.write().unwrap() = Some(f);
}

pub fn read_settings() -> Value {
    read_json(&settings_path()).filter(Value::is_object).unwrap_or_else(|| json!({}))
}

pub fn write_settings(data: &Value) -> Res<()> {
    std::fs::create_dir_all(config_dir()).map_err(|e| e.to_string())?;
    let text = serde_json::to_string_pretty(data).map_err(|e| e.to_string())?;
    write_atomic(&settings_path(), &text)
}

pub fn register_project(folder: &str) -> Res<()> {
    let folder = realpath(&expanduser(folder));
    let mut s = read_settings();
    if !s.get("projects").is_some_and(Value::is_array) {
        s["projects"] = json!([]);
    }
    let projects = s["projects"].as_array_mut().unwrap();
    if !projects.iter().any(|p| p.as_str() == Some(&folder)) {
        projects.push(json!(folder));
        write_settings(&s)?;
    }
    Ok(())
}

/// Folders that hold (or may hold) .claude/workflows: registered ones plus session folders that have one.
pub fn project_dirs() -> Vec<String> {
    let mut all: BTreeSet<String> = str_list(&read_settings(), "projects").into_iter().filter(|p| is_dir(p)).collect();
    if let Some(hint) = PROJECT_HINT.read().unwrap().as_ref() {
        for p in hint() {
            // a run's separate copy has the project's workflows too: they aren't another project
            if is_dir(&join(&p, &project_subdir())) && !crate::worktree::is_ours(&realpath(&p)) {
                all.insert(realpath(&p));
            }
        }
    }
    all.into_iter().collect()
}

fn workflow_files() -> Vec<String> {
    let mut files = vec![];
    for proj in project_dirs() {
        let d = join(&proj, &project_subdir());
        files.extend(list_dir(&d).into_iter().filter(|n| n.ends_with(".yaml")).map(|n| join(&d, &n)));
    }
    let legacy = legacy_dir();
    if is_dir(&legacy) {
        // files not yet moved into a project (e.g. their folder is missing)
        files.extend(list_dir(&legacy).into_iter().filter(|n| n.ends_with(".yaml")).map(|n| join(&legacy, &n)));
    }
    files
}

static INDEX: LazyLock<Mutex<(Option<Vec<String>>, HashMap<String, String>, Vec<String>)>> =
    LazyLock::new(|| Mutex::new((None, HashMap::new(), vec![])));

fn invalidate_index() {
    INDEX.lock().unwrap().0 = None;
}

/// id -> path, in file order. The id is the file name; a clash across projects gets a short suffix.
pub fn index() -> Vec<(String, String)> {
    let files = workflow_files();
    let mut cache = INDEX.lock().unwrap();
    if cache.0.as_ref() != Some(&files) {
        let mut map = HashMap::new();
        let mut order = vec![];
        for path in &files {
            let stem = basename(path).trim_end_matches(".yaml").to_string();
            let mut wid = stem.clone();
            if map.contains_key(&wid) {
                let hash = path.bytes().fold(0xcbf29ce484222325u64, |h, c| (h ^ c as u64).wrapping_mul(0x100000001b3));
                wid = format!("{stem}-{:04}", hash % 10000);
            }
            map.insert(wid.clone(), path.clone());
            order.push(wid);
        }
        *cache = (Some(files), map, order);
    }
    cache.2.iter().map(|w| (w.clone(), cache.1[w].clone())).collect()
}

pub fn index_get(wid: &str) -> Option<String> {
    index().into_iter().find(|(w, _)| w == wid).map(|(_, p)| p)
}

/// The file a workflow id lives in (ids are file names, made unique across projects).
pub fn path_for(wid: &str) -> Res<String> {
    if !id_re().is_match(wid) {
        return err("Bad workflow id.");
    }
    index_get(wid).ok_or_else(|| "Workflow not found.".into())
}

// ---- workflow files ----------------------------------------------------------------------------------

static LOCK: Mutex<()> = Mutex::new(());
type CacheEntry = (Option<f64>, u64, Option<Value>, Option<String>, String);
static CACHE: LazyLock<Mutex<HashMap<String, CacheEntry>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn load(path: &str, wid: &str) -> std::io::Result<(Option<Value>, Option<String>)> {
    let meta = std::fs::metadata(path)?;
    let mt = meta.modified().ok().map(to_secs);
    let size = meta.len();
    if let Some(hit) = CACHE.lock().unwrap().get(path) {
        if hit.0 == mt && hit.1 == size && hit.4 == wid {
            return Ok((hit.2.clone(), hit.3.clone()));
        }
    }
    let result = (|| -> Res<Value> {
        if !id_re().is_match(wid) {
            return err("Rename the file to lowercase letters, digits and dashes (e.g. my-flow.yaml).");
        }
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        let mut wf = from_doc(yamldoc::parse(&text)?, wid)?;
        wf["updated"] = json!(mt.unwrap_or(0.0));
        wf["file"] = json!(path);
        Ok(wf)
    })();
    let (wf, e) = match result {
        Ok(wf) => (Some(wf), None),
        Err(e) => (None, Some(e)),
    };
    CACHE.lock().unwrap().insert(path.into(), (mt, size, wf.clone(), e.clone(), wid.into()));
    Ok((wf, e))
}

/// Returns (raw workflows, errors). Raw: parsed but not yet validated.
pub fn load_all() -> (Vec<Value>, Vec<Value>) {
    migrate();
    let (mut out, mut errors) = (vec![], vec![]);
    let _guard = LOCK.lock().unwrap();
    for (wid, path) in index() {
        match load(&path, &wid) {
            Ok((Some(wf), _)) => out.push(wf),
            Ok((None, e)) => errors.push(json!({"file": path, "name": tilde(&path), "id": wid, "error": e.unwrap_or_default()})),
            Err(_) => continue,
        }
    }
    (out, errors)
}

fn target_dir(cwd: &str) -> String {
    join(&realpath(&expanduser(cwd)), &project_subdir())
}

/// Save a validated workflow into <its folder>/.claude/workflows, merging into the existing file.
/// base_updated is the file time the editor started from: if the file changed on disk since then,
/// refuse rather than overwrite those edits. Changing the folder moves the file.
pub fn write(wf: &Value, base_updated: Option<f64>) -> Res<String> {
    let wid = py_str(&wf["id"]);
    let old_path = index_get(&wid);
    let tdir = target_dir(&py_str(&wf["cwd"]));
    let path;
    {
        let _guard = LOCK.lock().unwrap();
        let mut old_text = None;
        if let Some(op) = old_path.as_deref().filter(|p| exists(p)) {
            if let (Some(base), Some(mt)) = (base_updated.filter(|b| *b != 0.0), mtime(op)) {
                if mt > base + 0.001 {
                    return err("This workflow's file was changed in your editor since you opened it here. \
                                Reload to get those changes, then make your edits again.");
                }
            }
            old_text = std::fs::read_to_string(op).ok();
        }
        path = match &old_path {
            Some(op) if dirname(op) == tdir => op.clone(),
            _ => {
                // new, or moved to another project
                let stem = old_path.as_ref().map(|p| basename(p).trim_end_matches(".yaml").to_string()).unwrap_or(wid.clone());
                let mut p = join(&tdir, &format!("{stem}.yaml"));
                let mut n = 2;
                while exists(&p) {
                    p = join(&tdir, &format!("{stem}-{n}.yaml"));
                    n += 1;
                }
                p
            }
        };
        let text = yamldoc::dump(old_text.as_deref(), &to_doc(wf), HEADER);
        std::fs::create_dir_all(&tdir).map_err(|e| e.to_string())?;
        write_atomic(&path, &text)?;
        if let Some(op) = old_path.as_deref().filter(|op| *op != path && exists(op)) {
            trash(op)?;
        }
    }
    register_project(&dirname(&dirname(&tdir)))?;
    invalidate_index();
    Ok(path)
}

fn trash(path: &str) -> Res<()> {
    std::fs::create_dir_all(trash_dir()).map_err(|e| e.to_string())?;
    let stem = basename(path).trim_end_matches(".yaml").to_string();
    let dest = join(&trash_dir(), &format!("{stem}.{}.yaml", local_time("%Y%m%d-%H%M%S")));
    move_file(path, &dest)
}

fn move_file(src: &str, dest: &str) -> Res<()> {
    if std::fs::rename(src, dest).is_ok() {
        return Ok(());
    }
    std::fs::copy(src, dest).map_err(|e| e.to_string())?;
    std::fs::remove_file(src).map_err(|e| e.to_string())
}

/// Deleting keeps a copy in ~/.config/claude-agent-graph/deleted, in case it was a mistake.
pub fn remove(wid: &str) -> Res<()> {
    let path = path_for(wid)?;
    if exists(&path) {
        trash(&path)?;
    }
    invalidate_index();
    Ok(())
}

pub fn new_id(name: &str) -> String {
    let base = slug(name, "step", 40);
    let taken: HashSet<String> = index().into_iter().map(|(w, _)| w).collect();
    let (mut wid, mut n) = (base.clone(), 2);
    while taken.contains(&wid) {
        wid = format!("{base}-{n}");
        n += 1;
    }
    wid
}

/// One-time moves: old <id>.json files → YAML, and YAML in ~/.config → its project's .claude/workflows.
pub fn migrate() {
    let legacy = legacy_dir();
    if !is_dir(&legacy) {
        return;
    }
    let backup = join(&config_dir(), "workflows-backup");
    for name in list_dir(&legacy) {
        let src = join(&legacy, &name);
        let result = (|| -> Res<()> {
            let yaml = name.ends_with(".yaml");
            let wf = if name.ends_with(".json") {
                let mut wf = read_json(&src).ok_or("bad JSON")?;
                if !b(&wf, "id") {
                    wf["id"] = json!(name.trim_end_matches(".json"));
                }
                wf
            } else if yaml {
                from_doc(yamldoc::parse(&std::fs::read_to_string(&src).map_err(|e| e.to_string())?)?, name.trim_end_matches(".yaml"))?
            } else {
                return Ok(());
            };
            let cwd = expanduser(&s(&wf, "cwd"));
            if cwd.is_empty() || !is_dir(&cwd) {
                return Ok(()); // stays in the old folder (still listed) until its folder exists
            }
            let dest = join(&target_dir(&cwd), &format!("{}.yaml", py_str(&wf["id"])));
            if !exists(&dest) {
                std::fs::create_dir_all(dirname(&dest)).map_err(|e| e.to_string())?;
                if yaml {
                    std::fs::copy(&src, &dest).map_err(|e| e.to_string())?; // keeps comments exactly
                } else {
                    let mut wf = wf.clone();
                    wf["cwd"] = json!(cwd);
                    invalidate_index();
                    std::fs::write(&dest, yamldoc::dump(None, &to_doc(&wf), HEADER)).map_err(|e| e.to_string())?;
                }
            }
            register_project(&cwd)?;
            std::fs::create_dir_all(&backup).map_err(|e| e.to_string())?;
            move_file(&src, &join(&backup, &name))?;
            invalidate_index();
            Ok(())
        })();
        if let Err(e) = result {
            eprintln!("workflow migration skipped {name} {e}");
        }
    }
}

// ---- defaults: agent library and step templates -------------------------------------------------------

static DEFAULTS_CACHE: LazyLock<Mutex<Option<(Vec<(String, Option<f64>)>, Value)>>> = LazyLock::new(|| Mutex::new(None));

fn read_yaml(path: &str) -> Res<Value> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(yamldoc::parse(&text)?).map(|v| if v.is_null() { json!({}) } else { v }),
        Err(_) => Ok(json!({})),
    }
}

fn read_text(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

pub fn default_files() -> Value {
    let dd = defaults_dir();
    json!({"agents": join(&dd, "agents.yaml"), "steps": join(&dd, "steps.yaml"),
           "userAgents": user_agents(), "userSteps": user_steps()})
}

/// The agent library and step templates: app defaults, then your files on top.
pub fn load_defaults() -> Value {
    let dd = defaults_dir();
    let (agents_file, steps_file, samples_file) = (join(&dd, "agents.yaml"), join(&dd, "steps.yaml"), join(&dd, "samples.yaml"));
    let key: Vec<(String, Option<f64>)> = [&agents_file, &steps_file, &user_agents(), &user_steps(), &samples_file]
        .into_iter()
        .filter(|f| exists(f))
        .map(|f| (f.clone(), mtime(f)))
        .collect();
    if let Some((k, v)) = DEFAULTS_CACHE.lock().unwrap().as_ref() {
        if *k == key {
            return v.clone();
        }
    }
    let mut errors = vec![];
    let mut categories: Vec<Value> = vec![];
    let mut steps = Map::new();
    let (mut builtin_agents, mut builtin_steps) = (HashSet::new(), HashSet::new());
    for path in [&agents_file, &user_agents()] {
        let mine = *path == user_agents();
        let doc = match read_yaml(path) {
            Ok(d) => d,
            Err(e) => {
                errors.push(json!({"name": tilde(path), "file": path, "error": e}));
                continue;
            }
        };
        for cat in arr(&doc, "categories") {
            let mut agents = vec![];
            if let Some(Value::Object(m)) = cat.get("agents") {
                for (n, a) in m {
                    let mut agent = Map::new();
                    agent.insert("name".into(), json!(n));
                    if let Value::Object(a) = a {
                        agent.extend(a.clone());
                    }
                    agent.insert("source".into(), json!(if mine { "user" } else { "builtin" }));
                    agent.insert("overrides".into(), json!(mine && builtin_agents.contains(n)));
                    agents.push(Value::Object(agent));
                }
            }
            if !mine {
                builtin_agents.extend(agents.iter().map(|a| py_str(&a["name"])));
            }
            for a in &agents {
                // a role with the same name replaces the earlier one
                for c in categories.iter_mut() {
                    if let Some(list) = c["agents"].as_array_mut() {
                        list.retain(|x| x["name"] != a["name"]);
                    }
                }
            }
            let cname = cat.get("name").cloned().unwrap_or(Value::Null);
            if let Some(same) = categories.iter_mut().find(|c| c["name"] == cname) {
                same["agents"].as_array_mut().unwrap().extend(agents);
            } else {
                categories.push(json!({"name": s_or(cat, "name", "My agents"), "icon": s_or(cat, "icon", "⑂"),
                                       "agents": agents, "user": mine}));
            }
        }
    }
    for path in [&steps_file, &user_steps()] {
        let mine = *path == user_steps();
        let doc = match read_yaml(path) {
            Ok(d) => d,
            Err(e) => {
                errors.push(json!({"name": tilde(path), "file": path, "error": e}));
                continue;
            }
        };
        if let Some(Value::Object(m)) = doc.get("steps") {
            for (k, v) in m {
                let mut st = v.as_object().cloned().unwrap_or_default();
                st.insert("source".into(), json!(if mine { "user" } else { "builtin" }));
                st.insert("overrides".into(), json!(mine && builtin_steps.contains(k)));
                steps.insert(k.clone(), Value::Object(st));
                if !mine {
                    builtin_steps.insert(k.clone());
                }
            }
        }
    }
    let mut samples = vec![]; // ready-made workflows for "Start from a sample"
    match read_yaml(&samples_file) {
        Ok(doc) => {
            if let Some(Value::Object(m)) = doc.get("samples") {
                for (k, v) in m {
                    let mut sm = Map::new();
                    sm.insert("id".into(), json!(k));
                    if let Value::Object(v) = v {
                        sm.extend(v.clone());
                    }
                    samples.push(Value::Object(sm));
                }
            }
        }
        Err(e) => errors.push(json!({"name": tilde(&samples_file), "file": samples_file, "error": e})),
    }
    categories.retain(|c| !arr(c, "agents").is_empty());
    let result = json!({"categories": categories, "steps": steps, "errors": errors, "samples": samples, "files": default_files()});
    *DEFAULTS_CACHE.lock().unwrap() = Some((key, result.clone()));
    result
}

fn dump_user(path: &str, old_text: Option<&str>, doc: &Value, header: &str) -> Res<()> {
    *DEFAULTS_CACHE.lock().unwrap() = None; // file times can be too coarse to notice a quick second save
    std::fs::create_dir_all(config_dir()).map_err(|e| e.to_string())?;
    std::fs::write(path, yamldoc::dump(old_text, doc, header)).map_err(|e| e.to_string())
}

/// Your file, as text and parsed (an empty mapping if it's missing, empty or not a mapping).
fn user_doc(path: &str) -> (Option<String>, Value) {
    let text = read_text(path);
    let doc = text.as_deref().and_then(|t| yamldoc::parse(t).ok()).filter(Value::is_object).unwrap_or_else(|| json!({}));
    let text = if doc.as_object().is_some_and(|o| !o.is_empty()) { text } else { None };
    (text, doc)
}

/// Add or replace a role in your own library file (~/.config/claude-agent-graph/agents.yaml).
pub fn save_user_agent(agent: &Value, category: &str, old_name: Option<&str>) -> Res<Value> {
    let name = s(agent, "name");
    if !agent_name_re().is_match(&name) {
        return err("Helper names use lowercase letters, digits and dashes, and start with a letter (e.g. test-writer).");
    }
    if s(agent, "description").trim().is_empty() || s(agent, "prompt").trim().is_empty() {
        return err("Give the helper a short description and its instructions.");
    }
    if let Some(old) = old_name.filter(|o| !o.is_empty() && *o != name) {
        delete_user_agent(old, true)?;
    }
    let (text, mut doc) = user_doc(&user_agents());
    if !doc.get("categories").is_some_and(Value::is_array) {
        doc["categories"] = json!([]);
    }
    let cats = doc["categories"].as_array_mut().unwrap();
    let mut spec = Map::new();
    for k in ["description", "model", "tools", "prompt"] {
        if b(agent, k) {
            spec.insert(k.into(), agent[k].clone());
        }
    }
    if let Some(Value::String(t)) = spec.get("tools").cloned() {
        spec.insert("tools".into(), json!(t.split(',').map(str::trim).filter(|x| !x.is_empty()).collect::<Vec<_>>()));
    }
    for c in cats.iter_mut() {
        // one place per name
        if c.get("name").and_then(Value::as_str) != Some(category) {
            if let Some(Value::Object(a)) = c.get_mut("agents") {
                a.shift_remove(&name);
            }
        }
    }
    let pos = match cats.iter().position(|c| c.get("name").and_then(Value::as_str) == Some(category)) {
        Some(p) => p,
        None => {
            cats.push(json!({"name": category, "icon": "⭐", "agents": {}}));
            cats.len() - 1
        }
    };
    if !cats[pos].get("agents").is_some_and(Value::is_object) {
        cats[pos]["agents"] = json!({});
    }
    cats[pos]["agents"].as_object_mut().unwrap().insert(name, Value::Object(spec));
    dump_user(
        &user_agents(),
        text.as_deref(),
        &doc,
        "Your own subagent roles. Same shape as defaults/agents.yaml in the app;\na role with the same name as a default one replaces it.",
    )?;
    Ok(load_defaults())
}

/// Remove one of your helpers. If it replaced a built-in one, the built-in one comes back.
pub fn delete_user_agent(name: &str, missing_ok: bool) -> Res<Value> {
    let (text, mut doc) = user_doc(&user_agents());
    if let Some(cats) = doc.get_mut("categories").and_then(Value::as_array_mut) {
        if let Some(c) = cats.iter_mut().find(|c| c.get("agents").and_then(Value::as_object).is_some_and(|a| a.contains_key(name))) {
            c["agents"].as_object_mut().unwrap().shift_remove(name);
            cats.retain(|x| opt_truthy(x.get("agents")));
            dump_user(&user_agents(), text.as_deref(), &doc, "")?;
            return Ok(load_defaults());
        }
    }
    if missing_ok {
        return Ok(load_defaults());
    }
    err("Only helpers you made can be deleted; built-in ones stay.")
}

/// Add or replace a step template in ~/.config/claude-agent-graph/steps.yaml.
pub fn save_user_step(key: Option<&str>, step: &Value, old_key: Option<&str>) -> Res<Value> {
    let key = slug(key.filter(|k| !k.is_empty()).map(str::to_string).unwrap_or_else(|| s(step, "name")).as_str(), "step", 40);
    if key.is_empty() || s(step, "name").trim().is_empty() {
        return err("Give the template a name.");
    }
    if s(step, "prompt").trim().is_empty() && s(step, "run").trim().is_empty() {
        return err("Say what the step should do (or the command it runs).");
    }
    if let Some(old) = old_key.filter(|o| !o.is_empty() && *o != key) {
        delete_user_step(old, true)?;
    }
    let (text, mut doc) = user_doc(&user_steps());
    if !doc.get("steps").is_some_and(Value::is_object) {
        doc["steps"] = json!({});
    }
    let mut spec = Map::new();
    for k in ["icon", "name", "prompt", "run", "check", "judge", "model", "retries", "review", "agents"] {
        if let Some(v) = step.get(k).filter(|v| truthy(v)) {
            spec.insert(k.into(), v.clone());
        }
    }
    doc["steps"].as_object_mut().unwrap().insert(key, Value::Object(spec));
    dump_user(
        &user_steps(),
        text.as_deref(),
        &doc,
        "Your own step templates. Same shape as defaults/steps.yaml in the app;\na template with the same key as a default one replaces it.",
    )?;
    Ok(load_defaults())
}

pub fn delete_user_step(key: &str, missing_ok: bool) -> Res<Value> {
    let (text, mut doc) = user_doc(&user_steps());
    if let Some(steps) = doc.get_mut("steps").and_then(Value::as_object_mut) {
        if steps.shift_remove(key).is_some() {
            dump_user(&user_steps(), text.as_deref(), &doc, "")?;
            return Ok(load_defaults());
        }
    }
    if missing_ok {
        return Ok(load_defaults());
    }
    err("Only templates you made can be deleted; built-in ones stay.")
}

static VERSION: LazyLock<Mutex<(u64, Option<Vec<(String, Option<f64>, u64)>>)>> = LazyLock::new(|| Mutex::new((0, None)));

/// A counter that goes up whenever a workflow or defaults file is added, changed or removed.
pub fn version() -> u64 {
    let dd = defaults_dir();
    let mut sig = vec![];
    for path in workflow_files().into_iter().chain([join(&dd, "agents.yaml"), join(&dd, "steps.yaml"), user_agents(), user_steps()]) {
        if let Ok(m) = std::fs::metadata(&path) {
            sig.push((path, m.modified().ok().map(to_secs), m.len()));
        }
    }
    let mut v = VERSION.lock().unwrap();
    if v.1.as_ref() != Some(&sig) {
        v.0 += 1;
        v.1 = Some(sig);
    }
    v.0
}
