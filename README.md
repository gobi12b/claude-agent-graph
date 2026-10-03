# Claude Agent Graph

A live, visual map of everything [Claude Code](https://claude.com/claude-code) is doing on your machine: your sessions, the helper agents (subagents) they start, and the messages between them. It also includes a drag-and-drop **workflow builder** that chains Claude steps and shell steps, with retries, checks, review pauses and loop-backs.

Runs on **macOS, Linux and Windows**.

## Features

- **Live graph** of Claude Code sessions and subagents, built by tailing the transcripts in `~/.claude`
- **Activity feed**: prompts, helper starts and hand-backs, files written or edited
- **Stop a running session** from the graph
- **Workflow builder**: steps run as headless `claude -p` sessions or as free shell commands, in dependency order and in parallel where possible
- **Review checkpoints**: a step can pause for your approval or feedback before the workflow goes on
- **Agent library**: ready-made subagent roles (product owner, architect, reviewer, tester, …) you can drop into steps
- **Permissions editor** for `~/.claude/settings.json`
- Workflows are plain YAML in `<project>/.claude/workflows/`, so they're versioned with your code and editable by hand

## Requirements

| | |
|---|---|
| **Claude Code** | The `claude` CLI on your `PATH`, signed in. [Install it here](https://claude.com/claude-code). |
| **Python** | 3.9 or newer |
| **git** | Used by the installer. On Windows, Git for Windows also supplies the bash that shell steps use. |

## Install

### macOS / Linux

```sh
curl -fsSL https://raw.githubusercontent.com/gobi12b/claude-agent-graph/main/install.sh | sh
```

### Windows (PowerShell)

```powershell
irm https://raw.githubusercontent.com/gobi12b/claude-agent-graph/main/install.ps1 | iex
```

The installer uses [uv](https://docs.astral.sh/uv/) or [pipx](https://pipx.pypa.io/) when either is installed. Otherwise it creates a private virtual environment (`~/.local/share/claude-agent-graph` or `%LOCALAPPDATA%\claude-agent-graph`). It also adds an app-menu entry on Linux and a Start-menu shortcut on Windows.

### Manual install (any OS)

With uv or pipx:

```sh
uv tool install git+https://github.com/gobi12b/claude-agent-graph
# or
pipx install git+https://github.com/gobi12b/claude-agent-graph
```

From a clone, without installing:

```sh
git clone https://github.com/gobi12b/claude-agent-graph
cd claude-agent-graph
python -m pip install ruamel.yaml
python -m agent_graph
```

## Run

```sh
agent-graph              # opens a native window if one is available, otherwise your browser
agent-graph --browser    # always use the browser (serves on http://127.0.0.1:8765)
agent-graph --port 9000  # pick the port
```

The server listens only on `127.0.0.1`, and every API call needs a random token issued at startup, so other websites can't reach it.

### Native window vs. browser

| OS | Window | How |
|---|---|---|
| Linux | GTK WebKit | Uses the system GTK packages when present: `sudo apt install python3-gi gir1.2-webkit2-4.1` (Debian/Ubuntu) or `sudo dnf install python3-gobject webkit2gtk4.1` (Fedora). The installer's venv/pipx installs can see these system packages. |
| macOS | pywebview | The installer adds it automatically. |
| Windows | pywebview (Edge WebView2) | The installer tries to add it. If that fails (some new Python versions lack wheels), the app opens in your browser instead. |

Without a native window, the app opens in your default browser and works the same way.

## Platform notes

- **Shell steps** (`run:`) and **checks** (`check:`) run with `bash -lc` in the workflow folder: bash on macOS/Linux, Git Bash on Windows (found via `CLAUDE_CODE_GIT_BASH_PATH`, `git`, or the default install paths). If there's no bash on Windows, they fall back to `cmd.exe`.
- **Opening files** uses `xdg-open` on Linux, `open` on macOS and the default app association on Windows. If an IDE is installed (VS Code, Cursor, Windsurf, JetBrains IDEs, Zed or Sublime Text), its CLI is used instead.
- **Notifications** (e.g. "a step is waiting for your review") use `notify-send` on Linux, Notification Center on macOS and a tray balloon on Windows.
- On **Windows**, the app restarts itself in Python's UTF-8 mode, because transcripts and YAML files are UTF-8.

## Where things live

| What | Where |
|---|---|
| Claude Code transcripts (read only) | `~/.claude/projects/`, `~/.claude/sessions/` |
| Your workflows | `<project>/.claude/workflows/<id>.yaml` |
| Your agent and step additions | `~/.config/claude-agent-graph/agents.yaml`, `steps.yaml` |
| Run history and logs | `~/.config/claude-agent-graph/runs/` |
| Built-in agent library and step templates | `agent_graph/defaults/` |

On Windows, `~` means your user folder (`C:\Users\<you>`).

## Update / uninstall

```sh
# update: run the installer again, or
uv tool upgrade claude-agent-graph        # or: pipx upgrade claude-agent-graph

# uninstall
uv tool uninstall claude-agent-graph      # or: pipx uninstall claude-agent-graph
rm -rf ~/.local/share/claude-agent-graph ~/.local/bin/agent-graph   # if the installer used its own venv
```

On Windows without uv/pipx, delete `%LOCALAPPDATA%\claude-agent-graph` and the Start-menu shortcut. Your settings and run history stay in `~/.config/claude-agent-graph` until you delete that folder.

## Development

```sh
git clone https://github.com/gobi12b/claude-agent-graph
cd claude-agent-graph
python -m venv .venv && . .venv/bin/activate     # Windows: .venv\Scripts\activate
pip install -e ".[window]"
agent-graph --browser
```

Layout:

```
agent_graph/
  app.py        HTTP server, API routes, window
  watcher.py    tails ~/.claude transcripts and builds the graph
  workflows.py  workflow runner, IDE integration, permissions editor
  wfstore.py    YAML storage that keeps your comments on save
  compat.py     the Linux / macOS / Windows differences
  index.html    the whole UI (single file, no build step)
  defaults/     built-in agent library and step templates
```

## License

MIT
