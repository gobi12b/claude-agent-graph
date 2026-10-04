//! Which AI models a workflow step can use, and how to run a step with each.
//!
//! A step's model is a plain string:
//!   ""  / "haiku" / "sonnet" / "opus" / "fable"   Claude, through Claude Code (`claude -p`)
//!   "gemini" / "gemini:<model>"                   Google Gemini, through the Gemini CLI (`gemini`)
//!   "gpt"    / "gpt:<model>"                      OpenAI GPT, through the Codex CLI (`codex exec`)
//!   "ollama:<model>"                              a local model from Ollama, through Claude Code pointed at
//!                                                 Ollama's Anthropic-compatible API, so it keeps Claude Code's tools
//! A bare "gemini" or "gpt" (or "gemini:" / "gpt:") means that tool's own default model.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::process::Command;
use std::time::Duration;

use serde_json::{json, Value};

use crate::util::*;
use crate::wfstore;

pub const CLAUDE_MODELS: [&str; 4] = ["haiku", "sonnet", "opus", "fable"];

pub struct Info {
    pub id: &'static str,
    pub name: &'static str,
    pub cli: &'static str,
    pub maker: &'static str,
    pub about: &'static str,
    pub install: &'static str,
    pub signin: &'static str,
    pub docs: &'static str,
}

pub const INFO: [Info; 4] = [
    Info {
        id: "claude",
        name: "Claude",
        cli: "claude",
        maker: "Anthropic",
        about: "Claude models through Claude Code. Used by default.",
        install: "npm install -g @anthropic-ai/claude-code",
        signin: "Run `claude` once and sign in.",
        docs: "https://docs.claude.com/en/docs/claude-code",
    },
    Info {
        id: "gemini",
        name: "Gemini",
        cli: "gemini",
        maker: "Google",
        about: "Google's Gemini models through the Gemini CLI. Steps can read and edit files in the project folder.",
        install: "npm install -g @google/gemini-cli",
        signin: "Run `gemini` once in a terminal and sign in with your Google account (or set GEMINI_API_KEY).",
        docs: "https://github.com/google-gemini/gemini-cli",
    },
    Info {
        id: "gpt",
        name: "GPT",
        cli: "codex",
        maker: "OpenAI",
        about: "OpenAI's GPT models through the Codex CLI. Steps can read and edit files in the project folder.",
        install: "npm install -g @openai/codex",
        signin: "Run `codex` once in a terminal and sign in with ChatGPT (or set OPENAI_API_KEY).",
        docs: "https://github.com/openai/codex",
    },
    Info {
        id: "ollama",
        name: "Local models (Ollama)",
        cli: "ollama",
        maker: "runs on this computer",
        about: "Free, private models that run on your own computer. Steps use Claude Code's tools with the local model, \
                so they can read and edit files. Needs Ollama 0.14 or newer, and a model good at coding and tool use \
                (for example qwen3-coder or gpt-oss).",
        install: "curl -fsSL https://ollama.com/install.sh | sh     (macOS / Windows: download it from ollama.com)",
        signin: "Start it with `ollama serve` (the desktop app starts it for you), then download a model: \
                 `ollama pull qwen3-coder`.",
        docs: "https://ollama.com",
    },
];

pub fn info(provider: &str) -> &'static Info {
    INFO.iter().find(|i| i.id == provider).unwrap_or(&INFO[0])
}

pub fn ollama_url() -> String {
    let url = std::env::var("OLLAMA_HOST").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "http://localhost:11434".into());
    if url.starts_with("http") { url } else { format!("http://{url}") }
}

/// "gemini:gemini-pro" -> ("gemini", "gemini-pro"); "sonnet" or "" -> ("claude", "sonnet" / "").
pub fn split(model: &str) -> (String, String) {
    let model = model.trim();
    if model == "gemini" || model == "gpt" {
        // YAML-friendly: `model: gemini` (a trailing colon would be a YAML error)
        return (model.into(), String::new());
    }
    if let Some((head, rest)) = model.split_once(':') {
        if ["gemini", "gpt", "ollama"].contains(&head) {
            return (head.into(), rest.trim().into());
        }
    }
    ("claude".into(), model.into())
}

fn version(cli: &str) -> String {
    let mut cmd = Command::new(which(cli).unwrap_or_else(|| cli.into()));
    cmd.arg("--version");
    match run_capture(cmd, None, Some(Duration::from_secs(8))) {
        Ok(Output { code: Some(0), stdout, stderr }) => {
            let text = if stdout.trim().is_empty() { stderr } else { stdout };
            first_chars(&first_line(text.trim()), 80)
        }
        _ => String::new(),
    }
}

/// A plain HTTP GET (Ollama's API is local and unencrypted). Returns the body.
fn http_get(url: &str, timeout: Duration) -> Option<String> {
    let rest = url.strip_prefix("http://")?;
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let addr_text = if hostport.contains(':') && !hostport.ends_with(']') { hostport.to_string() } else { format!("{hostport}:80") };
    let addr = addr_text.to_socket_addrs().ok()?.next()?;
    let mut stream = TcpStream::connect_timeout(&addr, timeout).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;
    write!(stream, "GET {path} HTTP/1.0\r\nHost: {hostport}\r\nAccept: application/json\r\nConnection: close\r\n\r\n").ok()?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = text.split_once("\r\n\r\n")?;
    if !head.split_whitespace().nth(1).is_some_and(|c| c.starts_with('2')) {
        return None;
    }
    Some(body.to_string())
}

/// (server running?, [local model names])
pub fn ollama_models() -> (bool, Vec<String>) {
    let Some(body) = http_get(&format!("{}/api/tags", ollama_url()), Duration::from_secs(2)) else {
        return (false, vec![]);
    };
    let Ok(v) = serde_json::from_str::<Value>(&body) else {
        return (false, vec![]);
    };
    let mut names: Vec<String> = arr(&v, "models").iter().map(|m| s(m, "name")).collect();
    names.sort();
    (true, names)
}

/// Model names you added on the AI models page, per provider.
pub fn extra_models() -> Value {
    wfstore::read_settings().get("extraModels").filter(|v| truthy(v)).cloned().unwrap_or_else(|| json!({}))
}

pub fn set_extra_models(provider: &str, models: &Value) -> Res<Value> {
    if !["gemini", "gpt", "ollama"].contains(&provider) {
        return err("Unknown provider.");
    }
    let mut clean: Vec<String> = vec![];
    for m in models.as_array().map(Vec::as_slice).unwrap_or(&[]) {
        let m = py_str(m).trim().to_string();
        if !m.is_empty() && !clean.contains(&m) {
            clean.push(m);
        }
    }
    clean.truncate(30);
    let mut st = wfstore::read_settings();
    if !st.get("extraModels").is_some_and(Value::is_object) {
        st["extraModels"] = json!({});
    }
    st["extraModels"][provider] = json!(clean);
    wfstore::write_settings(&st)?;
    Ok(status())
}

/// Each provider: installed? ready? which models? and what to do if not.
pub fn status() -> Value {
    let extra = extra_models();
    let mut out = vec![];
    for i in &INFO {
        let path = which(i.cli);
        let installed = path.is_some();
        let mut p = json!({"id": i.id, "name": i.name, "cli": i.cli, "maker": i.maker, "about": i.about,
            "install": i.install, "signin": i.signin, "docs": i.docs, "installed": installed,
            "version": if installed { version(i.cli) } else { String::new() },
            "ready": installed, "problem": "", "models": [], "extra": extra.get(i.id).cloned().unwrap_or(json!([]))});
        if i.id == "claude" {
            p["models"] = json!(CLAUDE_MODELS);
        } else if i.id == "ollama" {
            let (running, local) = ollama_models();
            p["running"] = json!(running);
            p["models"] = json!(local);
            if installed && !running {
                p["ready"] = json!(false);
                p["problem"] = json!("Ollama is installed but not running. Start it with `ollama serve`, or open the Ollama app.");
            } else if running && local.is_empty() {
                p["ready"] = json!(false);
                p["problem"] = json!("No local models yet. Download one, for example: `ollama pull qwen3-coder`.");
            } else if running && which("claude").is_none() {
                p["ready"] = json!(false);
                p["problem"] = json!("Local models run through Claude Code, which isn't installed.");
            } else if !installed && running {
                // server reachable (e.g. another machine via OLLAMA_HOST) without the CLI
                p["installed"] = json!(true);
                p["ready"] = json!(true);
            }
        }
        if !b(&p, "installed") {
            p["problem"] = json!("Not installed.");
        }
        out.push(p);
    }
    Value::Array(out)
}

/// The command and extra environment for running one step with a non-Claude provider. The prompt goes to stdin.
pub fn command(provider: &str, name: &str, permission_mode: &str, cwd: &str, last_message_file: Option<&str>) -> Res<(Vec<String>, Vec<(String, String)>)> {
    match provider {
        "gemini" => {
            let mut cmd = vec![which("gemini").unwrap_or_else(|| "gemini".into())];
            if !name.is_empty() {
                cmd.extend(["-m".into(), name.into()]);
            }
            if permission_mode != "plan" {
                cmd.push("--yolo".into()); // unattended: approve its own file edits and commands, like acceptEdits
            }
            Ok((cmd, vec![]))
        }
        "gpt" => {
            let mut cmd = vec![which("codex").unwrap_or_else(|| "codex".into()), "exec".into(), "--skip-git-repo-check".into(), "-C".into(), cwd.into()];
            if !name.is_empty() {
                cmd.extend(["-m".into(), name.into()]);
            }
            match permission_mode {
                "plan" => cmd.extend(["--sandbox".into(), "read-only".into()]),
                "bypassPermissions" => cmd.push("--dangerously-bypass-approvals-and-sandbox".into()),
                _ => cmd.push("--full-auto".into()), // may edit files in the project folder
            }
            if let Some(f) = last_message_file {
                cmd.extend(["--output-last-message".into(), f.into()]);
            }
            cmd.push("-".into()); // read the prompt from stdin
            Ok((cmd, vec![]))
        }
        "ollama" => {
            // Claude Code talks to Ollama's Anthropic-compatible API; every model role maps to the local model.
            let env = [
                ("ANTHROPIC_BASE_URL", ollama_url()),
                ("ANTHROPIC_AUTH_TOKEN", "ollama".into()),
                ("ANTHROPIC_API_KEY", String::new()),
                ("ANTHROPIC_DEFAULT_OPUS_MODEL", name.into()),
                ("ANTHROPIC_DEFAULT_SONNET_MODEL", name.into()),
                ("ANTHROPIC_DEFAULT_HAIKU_MODEL", name.into()),
                ("CLAUDE_CODE_SUBAGENT_MODEL", name.into()),
            ];
            Ok((vec![crate::compat::claude_cmd()], env.into_iter().map(|(k, v)| (k.to_string(), v)).collect()))
        }
        _ => Err(format!("Unknown model provider “{provider}”.")),
    }
}

/// Fail early, in plain words, when a step's model can't run here.
pub fn check_ready(provider: &str, name: &str) -> Res<()> {
    if provider == "claude" {
        return Ok(());
    }
    if provider == "ollama" {
        if name.is_empty() {
            return err("Choose which local model to use (Ollama).");
        }
        let (running, local) = ollama_models();
        if !running {
            return err("Ollama isn't running. Start it with `ollama serve` (or open the Ollama app), then try again.");
        }
        if !local.iter().any(|m| m == name || *m == format!("{name}:latest")) {
            return Err(format!("The local model “{name}” isn't downloaded. Download it with: ollama pull {name}"));
        }
        return Ok(());
    }
    let i = info(provider);
    if which(i.cli).is_none() {
        return Err(format!("{} isn't installed. Install it with: {}  Then: {}", i.name, i.install, i.signin));
    }
    Ok(())
}
