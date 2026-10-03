//! Small helpers shared by every module: paths, JSON values with Python-like truthiness,
//! atomic JSON writes and running a command with a timeout.

use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

pub type Obj = Map<String, Value>;
pub type Res<T> = Result<T, String>;

// ---- paths ------------------------------------------------------------------------------

pub fn home() -> String {
    static HOME: OnceLock<String> = OnceLock::new();
    HOME.get_or_init(|| {
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        std::env::var(var).ok().filter(|h| !h.is_empty()).unwrap_or_else(|| ".".into())
    })
    .clone()
}

pub fn sep() -> char {
    std::path::MAIN_SEPARATOR
}

pub fn join(base: &str, rest: &str) -> String {
    Path::new(base).join(rest).to_string_lossy().into_owned()
}

pub fn config_dir() -> String {
    join(&home(), &format!(".config{}claude-agent-graph", sep()))
}

pub fn claude_dir() -> String {
    join(&home(), ".claude")
}

/// `~` and `~/…` → the home folder.
pub fn expanduser(p: &str) -> String {
    if p == "~" {
        return home();
    }
    if let Some(rest) = p.strip_prefix("~/").or_else(|| if cfg!(windows) { p.strip_prefix("~\\") } else { None }) {
        return join(&home(), rest);
    }
    p.to_string()
}

/// The home folder written as `~`, for display.
pub fn tilde(p: &str) -> String {
    let h = home();
    if p == h {
        return "~".into();
    }
    match p.strip_prefix(&h) {
        Some(rest) if rest.starts_with(sep()) => format!("~{rest}"),
        _ => p.to_string(),
    }
}

/// Lexically normalised path (like os.path.normpath).
pub fn normpath(p: &str) -> String {
    let path = Path::new(p);
    let mut out = PathBuf::new();
    let mut depth = 0usize;
    let absolute = path.has_root();
    for c in path.components() {
        match c {
            Component::Prefix(_) | Component::RootDir => out.push(c.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if depth > 0 {
                    out.pop();
                    depth -= 1;
                } else if !absolute {
                    out.push("..");
                }
            }
            Component::Normal(s) => {
                out.push(s);
                depth += 1;
            }
        }
    }
    let s = out.to_string_lossy().into_owned();
    if s.is_empty() { ".".into() } else { s }
}

/// Absolute, normalised path (like os.path.abspath).
pub fn abspath(p: &str) -> String {
    let path = Path::new(p);
    if path.is_absolute() {
        normpath(p)
    } else {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        normpath(&cwd.join(path).to_string_lossy())
    }
}

/// Symlinks resolved where the path exists (like os.path.realpath, which also accepts missing paths).
pub fn realpath(p: &str) -> String {
    let abs = abspath(p);
    if let Ok(c) = std::fs::canonicalize(&abs) {
        return strip_verbatim(c);
    }
    let mut existing = PathBuf::from(&abs);
    let mut rest = Vec::new();
    while let Some(name) = existing.file_name().map(|n| n.to_os_string()) {
        existing.pop();
        rest.push(name);
        if let Ok(c) = std::fs::canonicalize(&existing) {
            let mut out = PathBuf::from(strip_verbatim(c));
            for r in rest.iter().rev() {
                out.push(r);
            }
            return out.to_string_lossy().into_owned();
        }
    }
    abs
}

fn strip_verbatim(p: PathBuf) -> String {
    let s = p.to_string_lossy().into_owned();
    s.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(s)
}

pub fn dirname(p: &str) -> String {
    Path::new(p).parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default()
}

pub fn basename(p: &str) -> String {
    Path::new(p).file_name().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default()
}

pub fn is_dir(p: &str) -> bool {
    !p.is_empty() && Path::new(p).is_dir()
}

pub fn is_file(p: &str) -> bool {
    !p.is_empty() && Path::new(p).is_file()
}

pub fn exists(p: &str) -> bool {
    !p.is_empty() && Path::new(p).exists()
}

/// Names in a folder, sorted, without hidden ones (like glob's `*`).
pub fn list_dir(dir: &str) -> Vec<String> {
    let mut names: Vec<String> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect(),
        Err(_) => return vec![],
    };
    names.retain(|n| !n.starts_with('.'));
    names.sort();
    names
}

pub fn to_secs(t: SystemTime) -> f64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

pub fn now() -> f64 {
    to_secs(SystemTime::now())
}

pub fn mtime(p: &str) -> Option<f64> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok().map(to_secs)
}

pub fn file_size(p: &str) -> Option<u64> {
    std::fs::metadata(p).ok().map(|m| m.len())
}

pub fn which(cmd: &str) -> Option<String> {
    which::which(cmd).ok().map(|p| p.to_string_lossy().into_owned())
}

pub fn hex_id(n: usize) -> String {
    uuid::Uuid::new_v4().simple().to_string()[..n].to_string()
}

pub fn err<T>(e: impl std::fmt::Display) -> Res<T> {
    Err(e.to_string())
}

// ---- files --------------------------------------------------------------------------------

/// Write through a temporary file, so a crash never leaves half a file.
pub fn write_atomic(path: &str, text: &str) -> Res<()> {
    let tmp = format!("{path}.{}.tmp", hex_id(8));
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })
}

pub fn write_json(path: &str, data: &Value) -> Res<()> {
    std::fs::create_dir_all(dirname(path)).map_err(|e| e.to_string())?;
    let text = serde_json::to_string_pretty(data).map_err(|e| e.to_string())?;
    write_atomic(path, &(text + "\n"))
}

pub fn read_json(path: &str) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

pub fn read_lossy(path: &str) -> std::io::Result<String> {
    std::fs::read(path).map(|b| String::from_utf8_lossy(&b).into_owned())
}

// ---- values with Python semantics ------------------------------------------------------------

pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

pub fn opt_truthy(v: Option<&Value>) -> bool {
    v.map(truthy).unwrap_or(false)
}

/// str(value), the way Python would print it.
pub fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// str(obj.get(key) or default)
pub fn s_or(o: &Value, key: &str, default: &str) -> String {
    match o.get(key) {
        Some(v) if truthy(v) => py_str(v),
        _ => default.to_string(),
    }
}

/// obj.get(key) as a string, "" when missing or not truthy.
pub fn s(o: &Value, key: &str) -> String {
    s_or(o, key, "")
}

/// int(value), the way Python would convert it.
pub fn py_int(v: &Value) -> Res<i64> {
    match v {
        Value::Bool(b) => Ok(*b as i64),
        Value::Number(n) => Ok(n.as_i64().unwrap_or_else(|| n.as_f64().unwrap_or(0.0).trunc() as i64)),
        Value::String(s) => s
            .trim()
            .parse::<i64>()
            .map_err(|_| format!("invalid literal for int() with base 10: '{s}'")),
        Value::Null => Err("int() argument must be a string or a number, not 'NoneType'".into()),
        _ => Err("int() argument must be a string or a number".into()),
    }
}

pub fn py_float(v: &Value) -> Res<f64> {
    match v {
        Value::Bool(b) => Ok(*b as i64 as f64),
        Value::Number(n) => Ok(n.as_f64().unwrap_or(0.0)),
        Value::String(s) => s.trim().parse::<f64>().map_err(|_| format!("could not convert string to float: '{s}'")),
        _ => Err("float() argument must be a string or a number".into()),
    }
}

pub fn f(o: &Value, key: &str) -> f64 {
    o.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

pub fn b(o: &Value, key: &str) -> bool {
    opt_truthy(o.get(key))
}

pub fn arr<'a>(o: &'a Value, key: &str) -> &'a [Value] {
    o.get(key).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

pub fn str_list(o: &Value, key: &str) -> Vec<String> {
    arr(o, key).iter().map(py_str).collect()
}

/// A required key in a request body, failing with the same message as a Python KeyError.
pub fn req<'a>(body: &'a Value, key: &str) -> Res<&'a Value> {
    body.get(key).ok_or_else(|| format!("'{key}'"))
}

pub fn req_str(body: &Value, key: &str) -> Res<String> {
    req(body, key).map(py_str)
}

/// " ".join(text.split()), cut to n characters with an ellipsis.
pub fn short(text: &str, n: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= n {
        text
    } else {
        text.chars().take(n - 1).collect::<String>() + "…"
    }
}

pub fn first_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

pub fn last_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count <= n { s.to_string() } else { s.chars().skip(count - n).collect() }
}

pub fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// The first line of a text (like s.splitlines()[0]), "" when empty.
pub fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").to_string()
}

pub fn slug(text: &str, fallback: &str, max: usize) -> String {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new("[^a-z0-9]+").unwrap());
    let s = re.replace_all(&text.to_lowercase(), "-").trim_matches('-').to_string();
    let s = first_chars(&s, max);
    if s.is_empty() { fallback.into() } else { s }
}

pub fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

pub fn local_time(fmt: &str) -> String {
    chrono::Local::now().format(fmt).to_string()
}

// ---- processes ------------------------------------------------------------------------------

pub struct Output {
    pub code: Option<i32>, // None: timed out (and killed)
    pub stdout: String,
    pub stderr: String,
}

/// Run a command to the end, feeding `input` to stdin, with an optional time limit.
pub fn run_capture(mut cmd: Command, input: Option<&str>, timeout: Option<Duration>) -> std::io::Result<Output> {
    cmd.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::compat::detached(&mut cmd);
    let mut child = cmd.spawn()?;
    collect(&mut child, input, timeout, |_| {})
}

/// Feed stdin and read both pipes of a spawned child until it exits (or the time limit kills it).
pub fn collect(
    child: &mut std::process::Child,
    input: Option<&str>,
    timeout: Option<Duration>,
    on_spawn: impl FnOnce(u32),
) -> std::io::Result<Output> {
    on_spawn(child.id());
    let writer = child.stdin.take().map(|mut w| {
        let data = input.unwrap_or("").to_string();
        std::thread::spawn(move || {
            let _ = w.write_all(data.as_bytes());
        })
    });
    let read = |r: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut r) = r {
                let _ = r.read_to_end(&mut buf);
            }
            String::from_utf8_lossy(&buf).into_owned()
        })
    };
    let out = read(child.stdout.take().map(|r| Box::new(r) as Box<dyn Read + Send>));
    let errs = read(child.stderr.take().map(|r| Box::new(r) as Box<dyn Read + Send>));
    let code = wait_child(child, timeout)?;
    if let Some(w) = writer {
        let _ = w.join();
    }
    Ok(Output { code, stdout: out.join().unwrap_or_default(), stderr: errs.join().unwrap_or_default() })
}

/// Wait for a child; past the time limit, stop it and everything it started. None = timed out.
pub fn wait_child(child: &mut std::process::Child, timeout: Option<Duration>) -> std::io::Result<Option<i32>> {
    let Some(limit) = timeout else {
        let st = child.wait()?;
        return Ok(Some(exit_code(st)));
    };
    let end = Instant::now() + limit;
    loop {
        if let Some(st) = child.try_wait()? {
            return Ok(Some(exit_code(st)));
        }
        if Instant::now() >= end {
            crate::compat::kill_tree(child.id());
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub fn exit_code(st: std::process::ExitStatus) -> i32 {
    if let Some(c) = st.code() {
        return c;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = st.signal() {
            return -sig;
        }
    }
    -1
}

/// The last JSON line a `claude -p --output-format json` call printed.
pub fn last_json_line(stdout: &str) -> Option<Value> {
    serde_json::from_str(stdout.trim().lines().last()?).ok()
}
