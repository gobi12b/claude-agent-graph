//! Guardrails (spec G7): per-step limits (spend, turns, time), protected paths a step must not change,
//! and secret redaction in logs, run files and judge input. Set on the workflow or a step; the step wins.

use std::collections::{BTreeMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{json, Map, Value};

use crate::util::*;

/// How a step's failure starts when it hit a limit or touched a protected file. Such a step isn't retried:
/// a retry would spend more, or build on the forbidden change.
pub const LIMIT_HIT: &str = "went over its limit";
pub const PROTECT_HIT: &str = "changed protected files";

pub const LIMIT_KEYS: [&str; 3] = ["budgetUsd", "maxTurns", "timeout"];
const MASK: &str = "[REDACTED]";

/// Secrets masked whenever `redact` is on: API keys, tokens and private keys of common services.
const BUILT_IN: [&str; 10] = [
    r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
    r"\bsk-ant-[A-Za-z0-9_-]{20,}",
    r"\bsk-(?:proj-)?[A-Za-z0-9_-]{20,}",
    r"\bgh[pousr]_[A-Za-z0-9]{30,}",
    r"\bgithub_pat_[A-Za-z0-9_]{20,}",
    r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b",
    r"\bxox[abposr]-[A-Za-z0-9-]{10,}",
    r"\bAIza[0-9A-Za-z_-]{35}",
    r"(?i)\bbearer\s+[A-Za-z0-9._~+/-]{20,}=*",
    // KEY=value / "token": "value": keep the name, mask the value
    r#"(?i)\b([A-Za-z0-9_]*(?:api[_-]?key|secret|token|password|passwd)[A-Za-z0-9_]*["']?\s*[:=]\s*["']?)[^\s"',;]{8,}"#,
];

fn built_in() -> &'static [Regex] {
    static RES: OnceLock<Vec<Regex>> = OnceLock::new();
    RES.get_or_init(|| BUILT_IN.iter().map(|p| Regex::new(p).unwrap()).collect())
}

/// "20m", "90s", "1h30m", "2h" or a plain number of seconds.
pub fn parse_duration(v: &Value) -> Res<u64> {
    if let Some(n) = v.as_f64() {
        return if n >= 1.0 { Ok(n as u64) } else { Err(format!("timeout should be at least 1 second, not {n}")) };
    }
    let t = py_str(v).trim().to_lowercase();
    static DUR: OnceLock<Regex> = OnceLock::new();
    let re = DUR.get_or_init(|| Regex::new(r"^(?:(\d+)h)?\s*(?:(\d+)m)?\s*(?:(\d+)s?)?$").unwrap());
    let bad = || format!("timeout “{t}” should look like 90s, 20m or 1h30m");
    let c = re.captures(&t).filter(|_| !t.is_empty()).ok_or_else(bad)?;
    let n = |i: usize| c.get(i).map(|m| m.as_str().parse::<u64>().unwrap_or(0)).unwrap_or(0);
    let secs = n(1) * 3600 + n(2) * 60 + n(3);
    if secs == 0 {
        return Err(bad());
    }
    Ok(secs)
}

/// Seconds back to the short form people write ("20m", "1h30m", "45s").
pub fn format_duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    let mut out = String::new();
    if h > 0 {
        out += &format!("{h}h");
    }
    if m > 0 {
        out += &format!("{m}m");
    }
    if s > 0 || out.is_empty() {
        out += &format!("{s}s");
    }
    out
}

fn items(v: &Value) -> Vec<String> {
    match v {
        Value::Array(a) => a.iter().map(py_str).map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect(),
        Value::String(t) => t.lines().map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect(),
        _ => vec![],
    }
}

/// Check and tidy the guardrail keys of a workflow or a step. Returns only the keys that are set,
/// so a workflow without them looks exactly as before.
pub fn clean(src: &Value, place: &str) -> Res<Map<String, Value>> {
    let mut out = Map::new();
    match src.get("limits") {
        None | Some(Value::Null) => {}
        Some(Value::Object(l)) => {
            let mut lim = Map::new();
            if let Some(v) = l.get("budgetUsd").filter(|v| !v.is_null() && *v != "") {
                let usd = py_float(v).map_err(|_| format!("{place}: limits.budgetUsd should be a number of dollars"))?;
                if usd <= 0.0 {
                    return Err(format!("{place}: limits.budgetUsd should be more than 0"));
                }
                lim.insert("budgetUsd".into(), json!(usd));
            }
            if let Some(v) = l.get("maxTurns").filter(|v| !v.is_null() && *v != "") {
                let n = py_int(v).map_err(|_| format!("{place}: limits.maxTurns should be a whole number"))?;
                if n < 1 {
                    return Err(format!("{place}: limits.maxTurns should be at least 1"));
                }
                lim.insert("maxTurns".into(), json!(n));
            }
            if let Some(v) = l.get("timeout").filter(|v| !v.is_null() && *v != "") {
                lim.insert("timeout".into(), json!(parse_duration(v).map_err(|e| format!("{place}: limits.{e}"))?));
            }
            if !lim.is_empty() {
                out.insert("limits".into(), Value::Object(lim));
            }
        }
        Some(_) => return Err(format!("{place}: limits should be a mapping (budgetUsd, maxTurns, timeout)")),
    }
    if let Some(v) = src.get("protect").filter(|v| !v.is_null()) {
        let globs = items(v);
        for g in &globs {
            if g.contains(',') || g.contains('{') {
                return Err(format!("{place}: protect pattern “{g}” can't contain , or {{ (list each pattern on its own)"));
            }
        }
        out.insert("protect".into(), json!(globs));
    }
    if let Some(v) = src.get("redact").filter(|v| !v.is_null()) {
        out.insert("redact".into(), json!(truthy(v)));
    }
    if let Some(v) = src.get("redactPatterns").filter(|v| !v.is_null()) {
        let pats = items(v);
        for p in &pats {
            Regex::new(p).map_err(|e| format!("{place}: redactPatterns “{p}” isn't a valid pattern: {e}"))?;
        }
        if !pats.is_empty() {
            out.insert("redactPatterns".into(), json!(pats));
        }
    }
    Ok(out)
}

/// The written form of a cleaned guardrail set (timeouts back as "20m").
pub fn to_doc(v: &Value, out: &mut Map<String, Value>) {
    if let Some(l) = v.get("limits").filter(|l| l.is_object()) {
        let mut l = l.clone();
        if let Some(t) = l.get("timeout").and_then(Value::as_u64) {
            l["timeout"] = json!(format_duration(t));
        }
        out.insert("limits".into(), l);
    }
    for k in ["protect", "redact", "redactPatterns"] {
        if let Some(x) = v.get(k).filter(|x| !x.is_null()) {
            out.insert(k.into(), x.clone());
        }
    }
}

/// The guardrails that apply to one step: each limit from the step, else the workflow.
#[derive(Clone, Debug, Default)]
pub struct Guard {
    pub budget: Option<f64>,
    pub max_turns: Option<i64>,
    pub timeout: Option<u64>,
    pub protect: Vec<String>,
    pub redactor: Option<Redactor>,
}

impl Guard {
    pub fn for_step(wf: &Value, step: &Value) -> Guard {
        let limit = |k: &str| step.get("limits").and_then(|l| l.get(k)).or_else(|| wf.get("limits").and_then(|l| l.get(k))).cloned();
        let pick = |k: &str| step.get(k).filter(|v| !v.is_null()).or_else(|| wf.get(k).filter(|v| !v.is_null()));
        let redact = pick("redact").map(truthy).unwrap_or(false);
        let mut patterns = str_list(wf, "redactPatterns");
        patterns.extend(str_list(step, "redactPatterns"));
        Guard {
            budget: limit("budgetUsd").and_then(|v| v.as_f64()),
            max_turns: limit("maxTurns").and_then(|v| v.as_i64()),
            timeout: limit("timeout").and_then(|v| v.as_u64()),
            protect: pick("protect").map(|p| str_list(&json!({"p": p}), "p")).unwrap_or_default(),
            redactor: redact.then(|| Redactor::new(&patterns)),
        }
    }

    /// Mask secrets when this step redacts; otherwise the text as it is.
    pub fn redact(&self, text: &str) -> String {
        match &self.redactor {
            Some(r) => r.redact(text),
            None => text.to_string(),
        }
    }

    /// Up-front rules for Claude: deny editing or writing the protected paths.
    /// Whether a failure reason means the step must not be retried.
    pub fn final_failure(reason: &str) -> bool {
        reason.starts_with(LIMIT_HIT) || reason.starts_with(PROTECT_HIT)
    }

    /// The `redact` field saved on an attempt record (its extra patterns), so logs are masked when shown later.
    pub fn record(&self) -> Option<Value> {
        self.redactor.as_ref().map(|r| json!(r.patterns))
    }

    pub fn disallowed_rules(&self) -> Vec<String> {
        self.protect.iter().flat_map(|g| [format!("Edit({g})"), format!("Write({g})")]).collect()
    }
}

/// Masks the built-in secret patterns plus any extra ones.
#[derive(Clone, Debug)]
pub struct Redactor {
    pub patterns: Vec<String>,
    extra: Vec<Regex>,
}

impl Redactor {
    pub fn new(patterns: &[String]) -> Redactor {
        Redactor { patterns: patterns.to_vec(), extra: patterns.iter().filter_map(|p| Regex::new(p).ok()).collect() }
    }

    /// From an attempt record's `redact` field (the extra patterns), if the step redacted.
    pub fn from_record(v: Option<&Value>) -> Option<Redactor> {
        let v = v?;
        if !truthy(v) && !v.is_array() {
            return None;
        }
        Some(Redactor::new(&items(v)))
    }

    pub fn redact(&self, text: &str) -> String {
        let mut out = text.to_string();
        for re in built_in().iter().chain(&self.extra) {
            if !re.is_match(&out) {
                continue;
            }
            out = re
                .replace_all(&out, |c: &regex::Captures| match c.get(1) {
                    Some(keep) if re.captures_len() == 2 => format!("{}{MASK}", keep.as_str()),
                    _ => MASK.to_string(),
                })
                .into_owned();
        }
        out
    }

    /// Mask the text fields of a log entry ({title, body, …}).
    pub fn redact_entry(&self, e: &mut Value) {
        if let Some(o) = e.as_object_mut() {
            for (_, v) in o.iter_mut() {
                if let Value::String(t) = v {
                    *t = self.redact(t);
                }
            }
        }
    }
}

// ---- protected paths ----------------------------------------------------------------------------------------

/// A protect pattern as a regex over paths relative to the folder, written with `/`.
/// Like .gitignore: a pattern without `/` matches a file name at any depth; `**` spans folders.
pub fn glob_regex(glob: &str) -> Regex {
    let g = glob.trim().trim_start_matches("./");
    let anchored = g.trim_end_matches('/').contains('/');
    let g = g.trim_start_matches('/');
    let mut re = String::from(if anchored { "^" } else { "^(?:.*/)?" });
    let chars: Vec<char> = g.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if chars.get(i + 1) == Some(&'*') => {
                i += 1;
                if chars.get(i + 1) == Some(&'/') {
                    i += 1;
                    re += "(?:.*/)?";
                } else {
                    re += ".*";
                }
            }
            '*' => re += "[^/]*",
            '?' => re += "[^/]",
            c => re += &regex::escape(&c.to_string()),
        }
        i += 1;
    }
    // a folder pattern covers everything inside it
    re += if g.ends_with('/') { ".*$" } else { "(?:/.*)?$" };
    Regex::new(&re).unwrap_or_else(|_| Regex::new("$^").unwrap())
}

pub fn matches_any(path: &str, globs: &[Regex]) -> bool {
    globs.iter().any(|g| g.is_match(path))
}

/// A fingerprint (size and content hash) of every file under `cwd` that a protect pattern covers,
/// git-ignored files such as .env included. Empty when nothing is protected.
pub fn fingerprint(cwd: &str, protect: &[String]) -> BTreeMap<String, (u64, u64)> {
    let mut out = BTreeMap::new();
    if protect.is_empty() {
        return out;
    }
    let globs: Vec<Regex> = protect.iter().map(|g| glob_regex(g)).collect();
    let mut stack = vec![(cwd.to_string(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.filter_map(|e| e.ok()) {
            let name = e.file_name().to_string_lossy().into_owned();
            let path = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                if name != ".git" {
                    stack.push((e.path().to_string_lossy().into_owned(), path));
                }
            } else if matches_any(&path, &globs) {
                let data = std::fs::read(e.path()).unwrap_or_default();
                let mut h = std::collections::hash_map::DefaultHasher::new();
                data.hash(&mut h);
                out.insert(path, (data.len() as u64, h.finish()));
            }
        }
    }
    out
}

/// Protected files that were added, changed or deleted between two fingerprints.
pub fn violations(before: &BTreeMap<String, (u64, u64)>, after: &BTreeMap<String, (u64, u64)>) -> Vec<String> {
    let keys: HashSet<&String> = before.keys().chain(after.keys()).collect();
    let mut out: Vec<String> = keys.into_iter().filter(|k| before.get(*k) != after.get(*k)).cloned().collect();
    out.sort();
    out
}

/// The step's failure reason for a protected-path violation.
pub fn protect_reason(files: &[String], can_rewind: bool) -> String {
    let shown: Vec<&str> = files.iter().take(10).map(String::as_str).collect();
    let more = if files.len() > 10 { format!(" and {} more", files.len() - 10) } else { String::new() };
    format!(
        "{PROTECT_HIT}: {}{more}.{}",
        shown.join(", "),
        if can_rewind { " Use Rewind this step to undo it." } else { "" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> String {
        let d = std::env::temp_dir().join(format!("ag-guard-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.to_string_lossy().into_owned()
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration(&json!("20m")).unwrap(), 1200);
        assert_eq!(parse_duration(&json!("1h30m")).unwrap(), 5400);
        assert_eq!(parse_duration(&json!("90s")).unwrap(), 90);
        assert_eq!(parse_duration(&json!("45")).unwrap(), 45);
        assert_eq!(parse_duration(&json!(600)).unwrap(), 600);
        assert!(parse_duration(&json!("soon")).is_err());
        assert!(parse_duration(&json!("0m")).is_err());
        assert_eq!(format_duration(1200), "20m");
        assert_eq!(format_duration(5430), "1h30m30s");
    }

    #[test]
    fn globs() {
        let m = |g: &str, p: &str| glob_regex(g).is_match(p);
        assert!(m(".env*", ".env"));
        assert!(m(".env*", ".env.local"));
        assert!(m(".env*", "apps/web/.env"));
        assert!(!m(".env*", "src/env.rs"));
        assert!(m(".github/**", ".github/workflows/ci.yml"));
        assert!(!m(".github/**", "docs/.github/x"));
        assert!(m("**/*.lock", "Cargo.lock"));
        assert!(m("**/*.lock", "a/b/yarn.lock"));
        assert!(!m("**/*.lock", "a/lock.txt"));
        assert!(m("secrets/", "secrets/key.pem"));
        assert!(m("/config.yml", "config.yml"));
        assert!(!m("/config.yml", "sub/config.yml"));
        assert!(m("src/*.rs", "src/main.rs"));
        assert!(!m("src/*.rs", "src/a/main.rs"));
    }

    #[test]
    fn clean_keys() {
        let c = clean(&json!({"limits": {"budgetUsd": "0.5", "maxTurns": 3, "timeout": "20m"}, "protect": [".env*", "**/*.lock"], "redact": true}), "workflow").unwrap();
        assert_eq!(c["limits"], json!({"budgetUsd": 0.5, "maxTurns": 3, "timeout": 1200}));
        assert_eq!(c["protect"], json!([".env*", "**/*.lock"]));
        assert_eq!(c["redact"], json!(true));
        assert!(clean(&json!({}), "w").unwrap().is_empty());
        assert!(clean(&json!({"limits": {"maxTurns": 0}}), "w").is_err());
        assert!(clean(&json!({"limits": {"budgetUsd": -1}}), "w").is_err());
        assert!(clean(&json!({"limits": "lots"}), "w").is_err());
        assert!(clean(&json!({"protect": ["a,b"]}), "w").is_err());
        assert!(clean(&json!({"redactPatterns": ["("]}), "w").is_err());
        let mut doc = Map::new();
        to_doc(&Value::Object(c), &mut doc);
        assert_eq!(doc["limits"]["timeout"], json!("20m"));
    }

    #[test]
    fn step_overrides_workflow() {
        let wf = json!({"maxBudgetUsd": 5.0, "limits": {"maxTurns": 40, "timeout": 600}, "protect": [".env*"], "redact": true, "redactPatterns": ["ACME-[0-9]+"]});
        let g = Guard::for_step(&wf, &json!({"limits": {"maxTurns": 3}}));
        assert_eq!((g.budget, g.max_turns, g.timeout), (None, Some(3), Some(600)));
        assert_eq!(g.protect, vec![".env*"]);
        assert_eq!(g.redact("id ACME-123"), "id [REDACTED]");
        let g = Guard::for_step(&wf, &json!({"limits": {"budgetUsd": 0.5}, "protect": [], "redact": false}));
        assert_eq!(g.budget, Some(0.5));
        assert!(g.protect.is_empty());
        assert_eq!(g.redact("id ACME-123"), "id ACME-123");
        let g = Guard::for_step(&json!({}), &json!({}));
        assert!(g.budget.is_none() && g.max_turns.is_none() && g.protect.is_empty() && g.redactor.is_none());
        assert_eq!(Guard::for_step(&wf, &json!({})).disallowed_rules(), vec!["Edit(.env*)", "Write(.env*)"]);
    }

    #[test]
    fn redaction() {
        let r = Redactor::new(&[]);
        let key = format!("sk-ant-api03-{}", "a".repeat(40));
        assert_eq!(r.redact(&format!("key is {key} ok")), "key is [REDACTED] ok");
        assert_eq!(r.redact(&format!("token ghp_{}", "x".repeat(36))), "token [REDACTED]");
        assert_eq!(r.redact("AWS AKIAABCDEFGHIJKLMNOP here"), "AWS [REDACTED] here");
        assert_eq!(r.redact("DB_PASSWORD=hunter2hunter2"), "DB_PASSWORD=[REDACTED]");
        assert_eq!(r.redact(r#"{"api_key": "abcdef123456"}"#), r#"{"api_key": "[REDACTED]"}"#);
        assert_eq!(r.redact("Authorization: Bearer abcdefghijklmnopqrstuvwxyz123"), "Authorization: [REDACTED]");
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIabc\n-----END RSA PRIVATE KEY-----";
        assert_eq!(r.redact(&format!("x\n{pem}\ny")), "x\n[REDACTED]\ny");
        assert_eq!(r.redact("nothing secret: a token of thanks"), "nothing secret: a token of thanks");
        let rec = Redactor::from_record(Some(&json!(["ACME-[0-9]+"]))).unwrap();
        assert_eq!(rec.redact("ACME-42"), "[REDACTED]");
        assert!(Redactor::from_record(None).is_none());
        assert!(Redactor::from_record(Some(&json!(false))).is_none());
        let mut e = json!({"title": format!("using {key}"), "t": 1.0});
        r.redact_entry(&mut e);
        assert_eq!(e["title"], "using [REDACTED]");
    }

    #[test]
    fn protected_edits_are_caught() {
        let d = tmp("fp");
        std::fs::write(format!("{d}/.env"), "A=1").unwrap();
        std::fs::create_dir_all(format!("{d}/src")).unwrap();
        std::fs::write(format!("{d}/src/main.rs"), "fn main() {}").unwrap();
        std::fs::create_dir_all(format!("{d}/.git")).unwrap();
        std::fs::write(format!("{d}/.git/x.lock"), "").unwrap();
        let protect = vec![".env*".to_string(), "**/*.lock".to_string()];
        let before = fingerprint(&d, &protect);
        assert_eq!(before.keys().collect::<Vec<_>>(), vec![".env"]); // .git is skipped
        std::fs::write(format!("{d}/src/main.rs"), "fn main() { changed() }").unwrap();
        assert!(violations(&before, &fingerprint(&d, &protect)).is_empty());
        std::fs::write(format!("{d}/.env"), "A=2").unwrap();
        std::fs::write(format!("{d}/Cargo.lock"), "x").unwrap();
        let v = violations(&before, &fingerprint(&d, &protect));
        assert_eq!(v, vec![".env", "Cargo.lock"]);
        assert!(protect_reason(&v, true).starts_with("changed protected files: .env, Cargo.lock."));
        assert!(fingerprint(&d, &[]).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }
}
