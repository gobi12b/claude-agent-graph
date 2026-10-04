"""Which AI models a workflow step can use, and how to run a step with each.

A step's model is a plain string:
  ""  / "haiku" / "sonnet" / "opus" / "fable"   Claude, through Claude Code (`claude -p`)
  "gemini" / "gemini:<model>"                   Google Gemini, through the Gemini CLI (`gemini`)
  "gpt"    / "gpt:<model>"                      OpenAI GPT, through the Codex CLI (`codex exec`)
  "ollama:<model>"                              a local model from Ollama, through Claude Code pointed at
                                                Ollama's Anthropic-compatible API, so it keeps Claude Code's tools
A bare "gemini" or "gpt" (or "gemini:" / "gpt:") means that tool's own default model.
"""

import json
import os
import shutil
import subprocess
import urllib.request

from . import wfstore

OLLAMA_URL = os.environ.get("OLLAMA_HOST", "http://localhost:11434")
if not OLLAMA_URL.startswith("http"):
    OLLAMA_URL = "http://" + OLLAMA_URL

INFO = {
    "claude": {"name": "Claude", "cli": "claude", "maker": "Anthropic",
               "about": "Claude models through Claude Code. Used by default.",
               "install": "npm install -g @anthropic-ai/claude-code", "signin": "Run `claude` once and sign in.",
               "docs": "https://docs.claude.com/en/docs/claude-code"},
    "gemini": {"name": "Gemini", "cli": "gemini", "maker": "Google",
               "about": "Google's Gemini models through the Gemini CLI. Steps can read and edit files in the project folder.",
               "install": "npm install -g @google/gemini-cli",
               "signin": "Run `gemini` once in a terminal and sign in with your Google account (or set GEMINI_API_KEY).",
               "docs": "https://github.com/google-gemini/gemini-cli"},
    "gpt": {"name": "GPT", "cli": "codex", "maker": "OpenAI",
            "about": "OpenAI's GPT models through the Codex CLI. Steps can read and edit files in the project folder.",
            "install": "npm install -g @openai/codex",
            "signin": "Run `codex` once in a terminal and sign in with ChatGPT (or set OPENAI_API_KEY).",
            "docs": "https://github.com/openai/codex"},
    "ollama": {"name": "Local models (Ollama)", "cli": "ollama", "maker": "runs on this computer",
               "about": "Free, private models that run on your own computer. Steps use Claude Code's tools with the local model, "
                        "so they can read and edit files. Needs Ollama 0.14 or newer, and a model good at coding and tool use "
                        "(for example qwen3-coder or gpt-oss).",
               "install": "curl -fsSL https://ollama.com/install.sh | sh     (macOS / Windows: download it from ollama.com)",
               "signin": "Start it with `ollama serve` (the desktop app starts it for you), then download a model: "
                         "`ollama pull qwen3-coder`.",
               "docs": "https://ollama.com"},
}
CLAUDE_MODELS = ["haiku", "sonnet", "opus", "fable"]


def split(model):
    """'gemini:gemini-pro' -> ('gemini', 'gemini-pro'); 'sonnet' or '' -> ('claude', 'sonnet' / '')."""
    model = str(model or "").strip()
    if model in ("gemini", "gpt"):  # YAML-friendly: `model: gemini` (a trailing colon would be a YAML error)
        return model, ""
    head, sep, rest = model.partition(":")
    if sep and head in ("gemini", "gpt", "ollama"):
        return head, rest.strip()
    return "claude", model


def _version(cli):
    try:
        p = subprocess.run([shutil.which(cli) or cli, "--version"], capture_output=True, text=True, timeout=8)
        return (p.stdout or p.stderr).strip().splitlines()[0][:80] if p.returncode == 0 else ""
    except (OSError, subprocess.TimeoutExpired, IndexError):
        return ""


def _ollama_models():
    """(server running?, [local model names])"""
    try:
        with urllib.request.urlopen(OLLAMA_URL + "/api/tags", timeout=2) as r:
            return True, sorted(m["name"] for m in json.load(r).get("models", []))
    except (OSError, ValueError):
        return False, []


def extra_models():
    """Model names you added on the AI models page, per provider."""
    return wfstore.read_settings().get("extraModels") or {}


def set_extra_models(provider, models):
    if provider not in ("gemini", "gpt", "ollama"):
        raise ValueError("Unknown provider.")
    clean = []
    for m in models or []:
        m = str(m).strip()
        if m and m not in clean:
            clean.append(m)
    s = wfstore.read_settings()
    s.setdefault("extraModels", {})[provider] = clean[:30]
    wfstore.write_settings(s)
    return status()


def status():
    """Each provider: installed? ready? which models? and what to do if not."""
    extra = extra_models()
    out = []
    for pid, info in INFO.items():
        path = shutil.which(info["cli"])
        p = {"id": pid, **info, "installed": bool(path), "version": _version(info["cli"]) if path else "",
             "ready": bool(path), "problem": "", "models": [], "extra": extra.get(pid, [])}
        if pid == "claude":
            p["models"] = CLAUDE_MODELS
        elif pid == "ollama":
            running, local = _ollama_models()
            p["running"], p["models"] = running, local
            if path and not running:
                p["ready"], p["problem"] = False, "Ollama is installed but not running. Start it with `ollama serve`, or open the Ollama app."
            elif running and not local:
                p["ready"], p["problem"] = False, "No local models yet. Download one, for example: `ollama pull qwen3-coder`."
            elif running and not shutil.which("claude"):
                p["ready"], p["problem"] = False, "Local models run through Claude Code, which isn't installed."
            elif not path and running:  # server reachable (e.g. another machine via OLLAMA_HOST) without the CLI
                p["installed"] = p["ready"] = True
        if not p["installed"]:
            p["problem"] = "Not installed."
        out.append(p)
    return out


def command(provider, name, *, claude_cmd, permission_mode, cwd, last_message_file=None):
    """(argv, extra_env) for running one step with a non-Claude provider. The prompt goes to stdin."""
    if provider == "gemini":
        cmd = [shutil.which("gemini") or "gemini"]
        if name:
            cmd += ["-m", name]
        if permission_mode != "plan":
            cmd += ["--yolo"]  # unattended: approve its own file edits and commands, like acceptEdits
        return cmd, {}
    if provider == "gpt":
        cmd = [shutil.which("codex") or "codex", "exec", "--skip-git-repo-check", "-C", cwd]
        if name:
            cmd += ["-m", name]
        if permission_mode == "plan":
            cmd += ["--sandbox", "read-only"]
        elif permission_mode == "bypassPermissions":
            cmd += ["--dangerously-bypass-approvals-and-sandbox"]
        else:
            cmd += ["--full-auto"]  # may edit files in the project folder
        if last_message_file:
            cmd += ["--output-last-message", last_message_file]
        return cmd + ["-"], {}  # "-": read the prompt from stdin
    if provider == "ollama":
        # Claude Code talks to Ollama's Anthropic-compatible API; every model role maps to the local model.
        env = {"ANTHROPIC_BASE_URL": OLLAMA_URL, "ANTHROPIC_AUTH_TOKEN": "ollama", "ANTHROPIC_API_KEY": "",
               "ANTHROPIC_DEFAULT_OPUS_MODEL": name, "ANTHROPIC_DEFAULT_SONNET_MODEL": name,
               "ANTHROPIC_DEFAULT_HAIKU_MODEL": name, "CLAUDE_CODE_SUBAGENT_MODEL": name}
        return [claude_cmd], env
    raise ValueError(f"Unknown model provider “{provider}”.")


def check_ready(provider, name):
    """Fail early, in plain words, when a step's model can't run here."""
    if provider == "claude":
        return
    info = INFO[provider]
    if provider == "ollama":
        if not name:
            raise ValueError("Choose which local model to use (Ollama).")
        running, local = _ollama_models()
        if not running:
            raise ValueError("Ollama isn't running. Start it with `ollama serve` (or open the Ollama app), then try again.")
        if name not in local and name + ":latest" not in local:
            raise ValueError(f"The local model “{name}” isn't downloaded. Download it with: ollama pull {name}")
        return
    if not shutil.which(info["cli"]):
        raise ValueError(f"{info['name']} isn't installed. Install it with: {info['install']}  Then: {info['signin']}")
