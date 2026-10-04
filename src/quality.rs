//! Checkpoints and the AI judge: what each step changed, a way back, and a second opinion on its work.
//!
//! Checkpoints: before and after every step the project folder is snapshotted as a git tree, using a private
//! index file, so your branch, index, HEAD and stash are never touched. Untracked files are included and
//! .gitignore is respected. Two snapshots give the step's exact diff; rewinding puts the folder back to a
//! snapshot (and snapshots the current state first, so the rewind itself can be undone). Folders that aren't
//! git repositories simply get no checkpoints.
//!
//! Judge: an independent model (fresh context, no tools) grades a step against plain-language criteria,
//! looking at the real diff rather than the step's own account of what it did. A failing verdict fails the
//! step, and its feedback becomes the reason given to the retry or loop-back.

use std::collections::HashMap;
use std::process::Command;
use std::time::Duration;

use serde_json::{json, Value};

use crate::util::*;

const GIT_TIMEOUT: Duration = Duration::from_secs(60);
const DIFF_LIMIT: usize = 200_000; // characters of patch sent to the UI
const JUDGE_DIFF_LIMIT: usize = 60_000; // characters of patch / file content the judge reads

const JUDGE_SYSTEM: &str = "You are a strict, independent reviewer of work done by an AI coding agent. You did not do the work and have \
no stake in it. Grade it ONLY against the acceptance criteria you are given.\n\
Rules:\n\
- Split the criteria into separate checks and judge each one on evidence from the DIFF / FILES. The agent's own \
summary is a claim, not evidence: agents often say they did things they didn't. If the evidence for a criterion \
is missing, it is not met.\n\
- pass is true only if every criterion is met. Don't fail work for things the criteria don't ask for.\n\
- score: 0-100 for how fully the criteria are met.\n\
- feedback: if it fails, tell the agent exactly what to fix, concretely (files, functions, what's missing), \
in a few sentences addressed to it. If it passes, one short sentence.";

fn judge_schema() -> Value {
    json!({
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
    })
}

// ---- checkpoints --------------------------------------------------------------------------------

pub fn git(cwd: &str, args: &[&str], env: &[(&str, &str)], input: Option<&str>) -> Res<String> {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(cwd).env("GIT_OPTIONAL_LOCKS", "0");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = run_capture(cmd, input.or(Some("")), Some(GIT_TIMEOUT)).map_err(|e| e.to_string())?;
    match out.code {
        Some(0) => Ok(out.stdout),
        None => Err(format!("git {} timed out", args[0])),
        Some(_) => {
            let msg = if out.stderr.trim().is_empty() { out.stdout } else { out.stderr };
            let msg = first_chars(msg.trim(), 300);
            Err(if msg.is_empty() { format!("git {} failed", args[0]) } else { msg })
        }
    }
}

pub fn is_repo(cwd: &str) -> bool {
    which("git").is_some() && is_dir(cwd) && git(cwd, &["rev-parse", "--is-inside-work-tree"], &[], None).is_ok_and(|o| o.trim() == "true")
}

pub fn toplevel(cwd: &str) -> Res<String> {
    Ok(git(cwd, &["rev-parse", "--show-toplevel"], &[], None)?.trim().to_string())
}

/// Run f(env) with a throwaway copy of the repo's index (copied so git can reuse its file-stat cache).
pub fn with_index<T>(cwd: &str, f: impl FnOnce(&[(&str, &str)]) -> Res<T>) -> Res<T> {
    let tmp = std::env::temp_dir().join(format!("agent-graph-index-{}", hex_id(12))).to_string_lossy().into_owned();
    let result = (|| {
        let real = join(cwd, git(cwd, &["rev-parse", "--git-path", "index"], &[], None)?.trim());
        if exists(&real) {
            std::fs::copy(&real, &tmp).map_err(|e| e.to_string())?;
            // keep the index's own time: git compares it with file times to catch edits made in the same
            // second the index was written (same size, same mtime); a fresh time would hide those edits
            if let Ok(t) = std::fs::metadata(&real).and_then(|m| m.modified()) {
                let _ = std::fs::File::options().write(true).open(&tmp).and_then(|f| f.set_modified(t));
            }
        } // else a new repo: git starts the index from scratch
        f(&[("GIT_INDEX_FILE", tmp.as_str())])
    })();
    let _ = std::fs::remove_file(&tmp);
    result
}

/// The folder's current contents as a git tree id (tracked + untracked, minus ignored), or None.
pub fn snapshot(cwd: &str) -> Option<String> {
    if !is_repo(cwd) {
        return None;
    }
    let top = toplevel(cwd).ok()?;
    with_index(&top, |env| {
        git(&top, &["add", "-A"], env, None)?;
        Ok(git(&top, &["write-tree"], env, None)?.trim().to_string())
    })
    .ok()
}

/// What changed between two snapshots: per-file line counts and the unified patch.
pub fn diff(cwd: &str, before: &str, after: &str) -> Res<Value> {
    let top = toplevel(cwd)?;
    let mut files = vec![];
    for line in git(&top, &["diff", "--no-renames", "--numstat", before, after], &[], None)?.lines() {
        let mut parts = line.splitn(3, '\t');
        let (add, rem, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        let num = |x: &str| if x == "-" { Value::Null } else { json!(x.parse::<i64>().unwrap_or(0)) };
        files.push(json!({"path": path, "added": num(add), "removed": num(rem)}));
    }
    let status: HashMap<String, String> = git(&top, &["diff", "--no-renames", "--name-status", before, after], &[], None)?
        .lines()
        .filter_map(|l| l.split_once('\t').map(|(st, p)| (p.to_string(), st.to_string())))
        .collect();
    for f in files.iter_mut() {
        let st = status.get(&s(f, "path")).map(String::as_str).unwrap_or("M");
        f["status"] = json!(match st.chars().next() {
            Some('A') => "added",
            Some('D') => "deleted",
            _ => "modified",
        });
    }
    let patch = git(&top, &["diff", "--no-color", "--no-renames", "-U3", before, after], &[], None)?;
    let total = |k: &str| files.iter().map(|f| f[k].as_i64().unwrap_or(0)).sum::<i64>();
    let (added, removed) = (total("added"), total("removed"));
    Ok(json!({"files": files, "patch": first_chars(&patch, DIFF_LIMIT), "truncated": char_len(&patch) > DIFF_LIMIT,
              "added": added, "removed": removed}))
}

/// Put the folder's files back to a snapshot. Only paths that differ are written or deleted; HEAD, the index
/// and ignored files are left alone. Returns a snapshot of the state before, so this can be undone.
pub fn restore(cwd: &str, tree: &str) -> Res<String> {
    let top = toplevel(cwd)?;
    git(&top, &["cat-file", "-e", &format!("{tree}^{{tree}}")], &[], None)?; // fails if git has pruned it
    let current = snapshot(&top).ok_or("couldn't snapshot the folder before rewinding")?;
    let listing = git(&top, &["diff", "--no-renames", "--name-status", tree, &current], &[], None)?;
    let changes: Vec<(String, String)> = listing.lines().filter_map(|l| l.split_once('\t').map(|(a, b)| (a.into(), b.into()))).collect();
    for (st, path) in &changes {
        if st.starts_with('A') {
            // created since the snapshot: remove it
            let full = join(&top, path);
            if std::fs::symlink_metadata(&full).is_ok() {
                std::fs::remove_file(&full).map_err(|e| e.to_string())?;
            }
            prune_dirs(&top, &dirname(&full));
        }
    }
    let back: Vec<&str> = changes.iter().filter(|(st, _)| !st.starts_with('A')).map(|(_, p)| p.as_str()).collect();
    if !back.is_empty() {
        let input = back.join("\n") + "\n";
        with_index(&top, |env| {
            git(&top, &["read-tree", tree], env, None)?;
            git(&top, &["checkout-index", "-f", "--stdin"], env, Some(&input))
        })?;
    }
    Ok(current)
}

fn prune_dirs(top: &str, d: &str) {
    let mut d = d.to_string();
    let prefix = format!("{top}{}", sep());
    while d.starts_with(&prefix) && is_dir(&d) && list_dir_all(&d).is_empty() {
        if std::fs::remove_dir(&d).is_err() {
            break;
        }
        d = dirname(&d);
    }
}

pub fn list_dir_all(d: &str) -> Vec<String> {
    std::fs::read_dir(d).map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default()
}

// ---- judge ------------------------------------------------------------------------------------------

/// The judge's view of the work: the git diff when there are checkpoints, else the files the step wrote.
fn evidence(cwd: &str, before: Option<&str>, after: Option<&str>, files: &[String]) -> String {
    if let (Some(before), Some(after)) = (before, after) {
        if let Ok(d) = diff(cwd, before, after) {
            let files = arr(&d, "files");
            if files.is_empty() {
                return "DIFF: the step changed no files.".into();
            }
            let head: Vec<String> = files
                .iter()
                .map(|f| format!("{:>8}  {}  (+{} -{})", s(f, "status"), s(f, "path"), f["added"].as_i64().unwrap_or(0), f["removed"].as_i64().unwrap_or(0)))
                .collect();
            return format!(
                "DIFF of everything changed in the project since the step began (all its attempts so far):\n{}\n\n{}",
                head.join("\n"),
                first_chars(&s(&d, "patch"), JUDGE_DIFF_LIMIT)
            );
        }
    }
    let (mut parts, mut budget) = (vec![], JUDGE_DIFF_LIMIT as i64);
    for path in files {
        let Ok(text) = read_lossy(path) else { continue };
        let text = first_chars(&text, budget.max(0) as usize);
        budget -= char_len(&text) as i64;
        parts.push(format!("=== {path}\n{text}"));
        if budget <= 0 {
            break;
        }
    }
    if parts.is_empty() {
        "FILES: the step wrote no files.".into()
    } else {
        format!("FILES the step wrote or edited (current content):\n\n{}", parts.join("\n\n"))
    }
}

/// Grade a step. Returns {pass, score, criteria, feedback, cost, model}.
#[allow(clippy::too_many_arguments)]
pub fn judge(criteria: &str, task: &str, output: &str, cwd: &str, before: Option<&str>, after: Option<&str>, files: &[String], model: &str, claude_cmd: &str) -> Res<Value> {
    let model = if model.is_empty() { "haiku" } else { model };
    let answer = if output.trim().is_empty() { "(none)".to_string() } else { last_chars(output.trim(), 6000) };
    let prompt = format!(
        "ACCEPTANCE CRITERIA:\n{}\n\nTHE TASK the agent was given:\n{}\n\nTHE AGENT'S FINAL ANSWER (a claim, check it against the evidence):\n{}\n\n{}",
        criteria.trim(),
        first_chars(task.trim(), 6000),
        answer,
        evidence(cwd, before, after, files)
    );
    let mut cmd = Command::new(claude_cmd);
    cmd.args(["-p", "--model", model, "--output-format", "json", "--no-session-persistence", "--tools", "", "--system-prompt", JUDGE_SYSTEM, "--json-schema"])
        .arg(judge_schema().to_string())
        .args(["--max-budget-usd", "1"])
        .current_dir(home());
    let res = match run_capture(cmd, Some(&prompt), Some(Duration::from_secs(300))) {
        Ok(Output { code: None, .. }) => return err("the judge didn't answer (TimeoutExpired)"),
        Ok(out) => last_json_line(&out.stdout).ok_or("the judge didn't answer (ValueError)")?,
        Err(e) => return Err(format!("the judge didn't answer ({e})")),
    };
    let verdict = res.get("structured_output").filter(|v| v.is_object());
    let Some(verdict) = verdict.filter(|_| !b(&res, "is_error")) else {
        let why = if b(&res, "result") { s(&res, "result") } else { py_str(res.get("subtype").unwrap_or(&Value::Null)) };
        return Err(format!("the judge failed: {}", first_chars(&why, 200)));
    };
    Ok(json!({"pass": b(verdict, "pass"), "score": py_int(verdict.get("score").unwrap_or(&json!(0))).unwrap_or(0),
              "criteria": verdict.get("criteria").filter(|c| truthy(c)).cloned().unwrap_or(json!([])),
              "feedback": s(verdict, "feedback").trim(), "cost": f(&res, "total_cost_usd"), "model": model}))
}
