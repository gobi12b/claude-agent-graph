//! Isolated runs and delivery: a run can work in its own git worktree (a separate checkout of the project
//! that shares its history), so runs never collide with each other or with the user's own edits. When the
//! run is done its changes can be applied to the user's folder, turned into a branch (one commit per step),
//! pushed, and opened as a pull request.
//!
//! Worktrees live in ~/.config/claude-agent-graph/worktrees/<run id>, outside the project, so they never
//! show up as untracked files in it. A run's changes are kept as git trees (base and final snapshot), so
//! applying or delivering still works after its worktree has been removed.

use std::process::Command;
use std::time::Duration;

use serde_json::{json, Value};

use crate::quality::{self, git, list_dir_all, toplevel, with_index};
use crate::util::*;
use crate::compat;

pub const KEEP: [&str; 3] = ["always", "onFailure", "never"];
const SETUP_TIMEOUT: u64 = 1800;
const BOT: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "Claude Agent Graph"),
    ("GIT_AUTHOR_EMAIL", "agent-graph@localhost"),
    ("GIT_COMMITTER_NAME", "Claude Agent Graph"),
    ("GIT_COMMITTER_EMAIL", "agent-graph@localhost"),
];

pub fn root() -> String {
    join(&config_dir(), "worktrees")
}

/// Is this folder inside one of our run worktrees? (Its .claude/workflows mustn't count as a project.)
pub fn is_ours(path: &str) -> bool {
    let r = root();
    path == r || path.starts_with(&format!("{r}{}", sep()))
}

/// Where the project folder sits inside its repository ("" when it is the top).
fn rel_in_repo(project: &str) -> Res<(String, String)> {
    let top = toplevel(project)?;
    let real = realpath(project);
    let rel = real.strip_prefix(&realpath(&top)).unwrap_or("").trim_start_matches(['/', '\\']).to_string();
    Ok((top, rel))
}

fn copy_path(src: &str, dest: &str) -> std::io::Result<()> {
    let meta = std::fs::metadata(src)?;
    if meta.is_dir() {
        std::fs::create_dir_all(dest)?;
        for name in list_dir_all(src) {
            copy_path(&join(src, &name), &join(dest, &name))?;
        }
    } else {
        if let Some(parent) = std::path::Path::new(dest).parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dest)?;
    }
    Ok(())
}

/// Make the run's worktree. spec: the workflow's cleaned `worktree:` settings.
/// Returns the run's worktree record: path, folder (where steps run), base commit and base tree.
pub fn create(project: &str, rid: &str, spec: &Value) -> Res<Value> {
    let (top, rel) = rel_in_repo(project)?;
    let base = s_or(spec, "base", "HEAD");
    let commit = git(&top, &["rev-parse", "--verify", "--quiet", &format!("{base}^{{commit}}")], &[], None)
        .map(|c| c.trim().to_string())
        .map_err(|_| format!("Can't make a separate copy from “{base}”: no such commit or branch (a new repository needs a first commit)."))?;
    let path = join(&root(), rid);
    std::fs::create_dir_all(root()).map_err(|e| e.to_string())?;
    git(&top, &["worktree", "add", "--detach", &path, &commit], &[], None).map_err(|e| format!("Couldn't make a separate copy of the project: {e}"))?;
    let folder = if rel.is_empty() { path.clone() } else { join(&path, &rel) };
    for item in str_list(spec, "copy") {
        let src = join(project, &item);
        if exists(&src) {
            copy_path(&src, &join(&folder, &item)).map_err(|e| format!("Couldn't copy {item} into the separate copy: {e}"))?;
        }
    }
    let mut rec = json!({"path": path, "folder": folder, "baseRef": base, "baseCommit": commit, "keep": s_or(spec, "keep", "onFailure")});
    let setup = s(spec, "setup");
    if !setup.trim().is_empty() {
        let mut cmd = compat::shell_command(&setup);
        cmd.current_dir(&folder);
        let out = run_capture(cmd, None, Some(Duration::from_secs(SETUP_TIMEOUT))).map_err(|e| format!("couldn't run the setup command: {e}"))?;
        match out.code {
            Some(0) => {}
            None => return Err(format!("The setup command `{setup}` timed out after {SETUP_TIMEOUT}s.")),
            Some(c) => return Err(format!("The setup command `{setup}` exited {c}:\n{}", last_chars((out.stdout + &out.stderr).trim(), 1500))),
        }
    }
    // taken after copying and setup, so neither shows up as a change of the run
    rec["baseTree"] = json!(quality::snapshot(&folder).ok_or("Couldn't snapshot the separate copy.")?);
    Ok(rec)
}

/// Delete a run's worktree (its changes stay available as git trees).
pub fn remove(project: &str, wt: &Value) -> Res<()> {
    let path = s(wt, "path");
    if !is_ours(&path) {
        return err("That isn't a run's separate copy.");
    }
    let top = toplevel(project).unwrap_or_else(|_| project.to_string());
    if git(&top, &["worktree", "remove", "--force", &path], &[], None).is_err() && is_dir(&path) {
        std::fs::remove_dir_all(&path).map_err(|e| e.to_string())?;
    }
    let _ = git(&top, &["worktree", "prune"], &[], None);
    Ok(())
}

/// Bring back a removed worktree, with the files as the run left them (tree: its latest snapshot).
pub fn recreate(project: &str, wt: &Value, tree: &str) -> Res<()> {
    let path = s(wt, "path");
    if is_dir(&s(wt, "folder")) {
        return Ok(());
    }
    let top = toplevel(project)?;
    let _ = git(&top, &["worktree", "prune"], &[], None);
    git(&top, &["worktree", "add", "--detach", &path, &s(wt, "baseCommit")], &[], None).map_err(|e| format!("Couldn't bring back the separate copy: {e}"))?;
    quality::restore(&s(wt, "folder"), tree).map_err(|e| format!("Couldn't bring back the run's files: {e}"))?;
    Ok(())
}

/// The run's changes, as a patch from its base to `tree` (paths relative to the repository top).
fn patch(top: &str, base_tree: &str, tree: &str) -> Res<String> {
    git(top, &["diff", "--binary", "--no-color", "--no-renames", base_tree, tree], &[], None)
}

/// Apply the run's net changes to the user's own folder. Atomic: if any part doesn't apply, nothing changes.
/// Returns a snapshot of the folder from before, to undo it.
pub fn apply(project: &str, base_tree: &str, tree: &str) -> Res<String> {
    let top = toplevel(project)?;
    let p = patch(&top, base_tree, tree)?;
    if p.trim().is_empty() {
        return err("The run didn't change any files, so there's nothing to apply.");
    }
    let undo = quality::snapshot(&top).ok_or("Couldn't snapshot your folder first.")?;
    git(&top, &["apply", "--binary", "--whitespace=nowarn", "-"], &[], Some(&p)).map_err(|e| {
        format!("Your folder has changed since the run started, so its changes don't apply cleanly. Nothing was changed. ({e})")
    })?;
    Ok(undo)
}

fn identity_env(top: &str) -> Vec<(&'static str, &'static str)> {
    let has = |k: &str| git(top, &["config", k], &[], None).is_ok_and(|v| !v.trim().is_empty());
    if has("user.name") && has("user.email") {
        vec![]
    } else {
        BOT.to_vec()
    }
}

/// Commits on top of the run's base commit: one per (tree, message), each holding the run's changes up to
/// that tree. Trees are full snapshots of the worktree, so only their difference from the base tree is used:
/// files copied in or made by setup (and anything else that was already there) are never committed.
pub fn commit_chain(project: &str, wt: &Value, trees: &[(String, String)]) -> Res<Vec<String>> {
    let top = toplevel(project)?;
    let (base_tree, base_commit) = (s(wt, "baseTree"), s(wt, "baseCommit"));
    let id_env = identity_env(&top);
    let mut parent = base_commit.clone();
    let mut last_tree = git(&top, &["rev-parse", &format!("{base_commit}^{{tree}}")], &[], None)?.trim().to_string();
    let mut commits = vec![];
    for (tree, message) in trees {
        let p = patch(&top, &base_tree, tree)?;
        let new_tree = with_index(&top, |env| {
            git(&top, &["read-tree", &base_commit], env, None)?;
            if !p.trim().is_empty() {
                git(&top, &["apply", "--cached", "--binary", "--whitespace=nowarn", "-"], env, Some(&p))?;
            }
            Ok(git(&top, &["write-tree"], env, None)?.trim().to_string())
        })?;
        if new_tree == last_tree {
            continue; // this step changed nothing
        }
        let env: Vec<(&str, &str)> = id_env.clone();
        let c = git(&top, &["commit-tree", &new_tree, "-p", &parent, "-F", "-"], &env, Some(message))?.trim().to_string();
        commits.push(c.clone());
        parent = c;
        last_tree = new_tree;
    }
    Ok(commits)
}

/// A free branch name: `name`, or `name-2`, `name-3`… if taken (unless `reuse` says it is ours already).
pub fn branch_name(project: &str, name: &str, reuse: bool) -> Res<String> {
    let top = toplevel(project)?;
    git(&top, &["check-ref-format", "--branch", name], &[], None).map_err(|_| format!("“{name}” isn't a valid branch name."))?;
    if reuse {
        return Ok(name.to_string());
    }
    let taken = |n: &str| git(&top, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{n}")], &[], None).is_ok();
    let mut out = name.to_string();
    let mut n = 2;
    while taken(&out) {
        out = format!("{name}-{n}");
        n += 1;
    }
    Ok(out)
}

pub fn set_branch(project: &str, name: &str, commit: &str) -> Res<()> {
    git(&toplevel(project)?, &["branch", "-f", name, commit], &[], None).map(|_| ())
}

pub fn push(project: &str, name: &str, force: bool) -> Res<()> {
    let top = toplevel(project)?;
    let mut args = vec!["push", "-u", "origin", name];
    if force {
        args.insert(1, "--force-with-lease");
    }
    let mut cmd = Command::new("git");
    cmd.args(&args).current_dir(&top).env("GIT_TERMINAL_PROMPT", "0");
    let out = run_capture(cmd, Some(""), Some(Duration::from_secs(300))).map_err(|e| e.to_string())?;
    match out.code {
        Some(0) => Ok(()),
        None => err("git push timed out"),
        Some(_) => Err(format!("git push failed: {}", last_chars(out.stderr.trim(), 400))),
    }
}

/// Open a pull request for a pushed branch with the GitHub CLI. Returns its URL.
pub fn open_pr(project: &str, branch: &str, title: &str, body: &str, draft: bool, base: &str) -> Res<String> {
    let gh = which("gh").ok_or("Opening a pull request needs the GitHub CLI (gh): install it and run `gh auth login`.")?;
    let top = toplevel(project)?;
    let mut cmd = Command::new(gh);
    cmd.args(["pr", "create", "--head", branch, "--title", title, "--body-file", "-"]).current_dir(&top);
    if draft {
        cmd.arg("--draft");
    }
    if !base.is_empty() {
        cmd.args(["--base", base]);
    }
    let out = run_capture(cmd, Some(body), Some(Duration::from_secs(120))).map_err(|e| e.to_string())?;
    match out.code {
        Some(0) => Ok(out.stdout.lines().rev().find(|l| l.starts_with("http")).unwrap_or(out.stdout.trim()).trim().to_string()),
        None => err("gh pr create timed out"),
        Some(_) => Err(format!("gh pr create failed: {}", last_chars((out.stderr + &out.stdout).trim(), 400))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &str, c: &str) {
        let st = Command::new("bash").args(["-c", c]).current_dir(dir).output().unwrap();
        assert!(st.status.success(), "{c}: {}", String::from_utf8_lossy(&st.stderr));
    }

    #[test]
    fn worktree_apply_and_commits() {
        let dir = std::env::temp_dir().join(format!("ag-wt-test-{}", hex_id(6))).to_string_lossy().into_owned();
        std::fs::create_dir_all(&dir).unwrap();
        sh(&dir, "git init -q && git config user.name t && git config user.email t@t && echo one > a.txt && git add . && git commit -qm init && echo SECRET=1 > .env");
        let rid = format!("test-{}", hex_id(6));
        let wt = create(&dir, &rid, &json!({"copy": [".env"], "setup": "echo built > setup.out"})).unwrap();
        let folder = s(&wt, "folder");
        assert!(is_file(&join(&folder, ".env")));
        // the user's folder is untouched while the run works
        sh(&folder, "echo two > a.txt && echo new > b.txt");
        let t1 = quality::snapshot(&folder).unwrap();
        sh(&folder, "echo three > c.txt");
        let t2 = quality::snapshot(&folder).unwrap();
        assert_eq!(std::fs::read_to_string(join(&dir, "a.txt")).unwrap().trim(), "one");
        // commits hold only the run's own changes: not .env or setup.out
        let commits = commit_chain(&dir, &wt, &[(t1.clone(), "Step 1".into()), (t1.clone(), "No change".into()), (t2.clone(), "Step 2".into())]).unwrap();
        assert_eq!(commits.len(), 2);
        let files = git(&dir, &["show", "--name-only", "--format=", &commits[1]], &[], None).unwrap();
        assert_eq!(files.trim(), "c.txt");
        let all = git(&dir, &["ls-tree", "-r", "--name-only", &commits[1]], &[], None).unwrap();
        assert!(!all.contains(".env") && !all.contains("setup.out"), "{all}");
        let name = branch_name(&dir, "agent/test", false).unwrap();
        set_branch(&dir, &name, &commits[1]).unwrap();
        assert_eq!(branch_name(&dir, "agent/test", false).unwrap(), "agent/test-2");
        // applying brings the changes to the user's folder, and can be undone
        let undo = apply(&dir, &s(&wt, "baseTree"), &t2).unwrap();
        assert_eq!(std::fs::read_to_string(join(&dir, "a.txt")).unwrap().trim(), "two");
        assert!(is_file(&join(&dir, "c.txt")));
        quality::restore(&dir, &undo).unwrap();
        assert!(!is_file(&join(&dir, "c.txt")));
        // removed and brought back with the run's files
        remove(&dir, &wt).unwrap();
        assert!(!is_dir(&s(&wt, "path")));
        recreate(&dir, &wt, &t2).unwrap();
        assert!(is_file(&join(&folder, "c.txt")));
        remove(&dir, &wt).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
