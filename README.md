# Claude Agent Graph

A live, visual map of everything [Claude Code](https://claude.com/claude-code) is doing on your machine: your sessions, the helper agents (subagents) they start, and the messages between them. It also includes a drag-and-drop **workflow builder** that chains Claude steps and shell steps, with retries, checks, review pauses and loop-backs.

Runs on **macOS, Linux and Windows**.

## Features

- **Live graph** of Claude Code sessions and subagents, built by tailing the transcripts in `~/.claude`
- **Activity feed**: prompts, helper starts and hand-backs, files written or edited
- **Stop a running session** from the graph
- **Workflow builder**: steps run as headless `claude -p` sessions or as free shell commands, in dependency order and in parallel where possible
- **Review checkpoints**: a step can pause for your approval or feedback before the workflow goes on
- **AI judge**: write a step's acceptance criteria in plain words; an independent AI grades the step's real file changes against them, and its feedback drives the retry
- **Step diffs and rewind**: every step is checkpointed, so you see exactly what it changed and can put the project back to before it (and undo that)
- **Token insight**: each step's input/output tokens, prompt-cache hit rate and turns
- **Steering**: send Claude guidance while a step runs (it continues the same conversation with your message), or redo a finished step with your feedback
- **Sample workflows**: ready-made workflows (fix a bug, build a feature, explain a project, security check, …) to start from
- **Command line**: list, run and validate workflows from the terminal, by name or straight from a workflow YAML file (`agent-graph run flow.yaml`, `agent-graph validate flows/*.yaml`), handy for scripts and CI
- **Other AI models**: run a workflow, or a single step, on Google Gemini, OpenAI GPT, or a free local model through Ollama, alongside Claude
- **Agent library**: ready-made subagent roles (product owner, architect, reviewer, tester, …) you can drop into steps
- **Permissions editor** for `~/.claude/settings.json`
- Workflows are plain YAML in `<project>/.claude/workflows/`, so they're versioned with your code and editable by hand

## Requirements

Every platform needs these three things:

| | |
|---|---|
| **Claude Code** | The `claude` CLI on your `PATH`, signed in (run `claude` once and log in). |
| **Python** | 3.9 or newer |
| **git** | Used by the installer. On Windows, Git for Windows also supplies the bash that shell steps use. |

Check what you already have:

```sh
claude --version
python3 --version   # Windows: py --version
git --version
```

## Prerequisites by platform

### macOS

1. **Command Line Tools.** These provide `git` and a `python3` (3.9):
   ```sh
   xcode-select --install
   ```
   If you'd rather have a newer Python, run `brew install python` ([Homebrew](https://brew.sh)).
2. **Claude Code:**
   ```sh
   curl -fsSL https://claude.ai/install.sh | bash
   claude   # sign in once
   ```
3. *Optional:* `brew install uv` (or `brew install pipx`) for a cleaner install and easy upgrades.

The native window (pywebview) is installed automatically and uses the WebKit built into macOS. Nothing else is needed.

### Linux

1. **Python, venv and git.** Some distros split `venv` into its own package, so install it explicitly:

   | Distro | Command |
   |---|---|
   | Debian / Ubuntu / Mint | `sudo apt install python3 python3-venv python3-pip git` |
   | Fedora | `sudo dnf install python3 python3-pip git` |
   | Arch | `sudo pacman -S python python-pip git` |
   | openSUSE | `sudo zypper install python3 python3-pip git` |

2. **Claude Code:**
   ```sh
   curl -fsSL https://claude.ai/install.sh | bash
   claude   # sign in once
   ```
3. *Optional, for a native window* instead of a browser tab:

   | Distro | Command |
   |---|---|
   | Debian / Ubuntu / Mint | `sudo apt install python3-gi gir1.2-webkit2-4.1` |
   | Fedora | `sudo dnf install python3-gobject webkit2gtk4.1` |
   | Arch | `sudo pacman -S python-gobject webkit2gtk-4.1` |

4. *Optional, for desktop notifications:* `notify-send` (Debian/Ubuntu: `sudo apt install libnotify-bin`; most desktops already have it).
5. *Optional, for file opening:* `xdg-utils`, which most desktops already include.
6. *Optional:* [uv](https://docs.astral.sh/uv/) or `pipx` (`sudo apt install pipx`).

### Windows 10 / 11

1. **Git for Windows.** Claude Code needs it too, and its Git Bash runs the workflows' shell steps:
   ```powershell
   winget install --id Git.Git -e
   ```
   You can also download it from [git-scm.com](https://git-scm.com/download/win).
2. **Python 3.9+:**
   ```powershell
   winget install --id Python.Python.3.12 -e
   ```
   You can also use the [python.org installer](https://www.python.org/downloads/); in that case, tick **"Add python.exe to PATH"**.
3. **Claude Code** (in PowerShell):
   ```powershell
   irm https://claude.ai/install.ps1 | iex
   claude   # sign in once
   ```
4. **Open a new terminal** after installing, so the updated `PATH` is picked up.
5. *Optional:* `winget install --id astral-sh.uv -e` for a cleaner install and easy upgrades.

The native window uses the Microsoft Edge **WebView2** runtime, which comes preinstalled on Windows 10 and 11. Without it, the app opens in your browser.

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

## Run workflows from the terminal

Every workflow can also run from the command line: the ones you built in the app, and any workflow YAML file. The terminal shows live progress, and the run is saved like any other, so it also shows up (live) on the app's **Runs** page.

```sh
agent-graph list                                  # workflows the app knows: name, id, steps and project folder
agent-graph run "Fix a bug"                       # run one by name, the start of its name, or its id
agent-graph run ./flows/nightly.yaml              # run a workflow file
agent-graph run ./flows/nightly.yaml -C ~/code/app   # ...in another project folder
agent-graph run fix-a-bug --yes                   # approve review points automatically (scripts, CI)
agent-graph validate ./flows/*.yaml               # check workflow files without running them
agent-graph runs                                  # recent runs: id, when, status, duration, cost
```

| Option | For |
|---|---|
| `run <workflow>` | A workflow file (anything ending in `.yaml`/`.yml`, or a path), or the id, exact name or start of the name of a workflow the app knows (not case-sensitive). If several match, it lists them so you can use the id. |
| `-C`, `--folder <dir>` | Run (or validate) in this project folder instead of the workflow's `cwd`. Handy for keeping one workflow file and running it on many projects. |
| `-y`, `--yes` | Approve every review point without asking. |
| `-q`, `--quiet` | Don't print the final step's result at the end. |
| `--json` | Print a JSON summary (status, cost, each step's output) instead of progress. Also works with `list` and `runs`. |
| `runs -n 30` | How many recent runs to list (default 15). |

**Review points.** When a step pauses for review, the terminal shows that step's result and asks:

```
👤 “Find the cause” is done and waiting for your review.
Approve and continue [a], request changes [c], or stop [s]?
```

- `a` continues. You can add optional notes for the next step.
- `c` asks what should change, then redoes the step with your feedback.
- `s` stops the run.

If nobody can answer (no terminal, for example in CI) and `--yes` isn't set, the run stops at the review point and exits with code `3`.

### Workflow files

A workflow file is the same YAML the app saves in `<project>/.claude/workflows/`. You can keep it anywhere: in a repo, in a shared folder of team workflows, or next to a script. The smallest useful one:

```yaml
name: Say hello
steps:
  - name: Hello
    prompt: Write hello.txt containing a friendly greeting.
  - name: Check
    run: test -f hello.txt        # a shell step: no AI, no cost
```

Which folder it runs in:

1. `--folder` / `-C`, if given.
2. Otherwise `cwd:` in the file. A relative `cwd` (such as `cwd: ..`) is relative to the file, not to where you run the command.
3. Otherwise the project the file belongs to, if it's inside `<project>/.claude/workflows/`; else the file's own folder.

The other settings are the ones the builder writes (open any workflow with **Open as text** to see them all): `model`, `permissionMode`, `allowedTools`, `disallowedTools`, `passOutput`, `maxBudgetUsd`, `agents`, and per step `prompt` or `run`, `retries`, `check`, `model`, `review`, `agents`, `dependsOn`, `onSuccess`, `onFailure` and `loopBack`. Steps run one after another unless `dependsOn` says otherwise. Replaying a step of a file run from the app reads the file again.

### Check workflows: `validate`

`validate` loads workflows the same way `run` does, but doesn't run anything:

```sh
agent-graph validate                       # every workflow the app knows
agent-graph validate flows/*.yaml          # these files
agent-graph validate "Fix a bug" --strict  # warnings count as errors too
```

```
✓ Say hello  ~/flows/hello.yaml · 2 steps · runs in ~/flows
⚠ Nightly  ~/flows/nightly.yaml · 3 steps · runs in ~/code/app
    warning: step 1 (“Plan”): unknown setting “promt” (did you mean “prompt”?)
    warning: workflow: model “gemini” can't run on this computer yet: Gemini isn't installed. Install it with: …
✗ ~/flows/broken.yaml
    error: line 5, column 1: found unexpected end of stream
2 of 3 valid (2 warning(s))
```

- **Errors** stop a workflow from loading or running. Examples: invalid YAML (with line and column), a missing `name` or `steps`, a step with no prompt or command, a missing `cwd` folder, `dependsOn`/`loopBack` pointing at a step that doesn't exist, a dependency loop, or an undefined helper.
- **Warnings** are probably mistakes, but the workflow would still run:
  - unknown settings, with a "did you mean" guess
  - a model that isn't set up on this computer
  - an unfilled `<describe …>` or `{task}` placeholder
  - a helper no step uses

Exit code: `0` when everything is valid; `1` when anything has an error, or a warning with `--strict`. `--json` prints the full report. To check workflows in CI:

```sh
agent-graph validate .claude/workflows/*.yaml --strict
```

**Stopping.** Press `Ctrl+C` to stop a run cleanly. You can also press **Stop** on the run in the app: it asks the terminal to stop, as if you had pressed `Ctrl+C` there (macOS and Linux). Reviews and steering of a terminal run happen in that terminal. Once it has finished, you can replay or steer its steps from the app.

**Exit codes**, for scripts:

| Code | Meaning |
|---|---|
| `0` | Finished successfully |
| `1` | A step failed, or the workflow wasn't found |
| `2` | Stopped (`Ctrl+C`, Stop in the app, or `s` at a review) |
| `3` | Paused for review, but there was no terminal to answer it (use `--yes`) |

Example: run a workflow every night with cron, and keep the log:

```sh
0 2 * * *  cd ~/code/my-app && agent-graph run "Review & tidy up code" --yes >> ~/agent-graph-nightly.log 2>&1
```

## AI models: Claude, Gemini, GPT and local models

Steps run on Claude by default. You can also use other models for a whole workflow (the **AI model** menu in the builder's top bar) or for one step (the step's **Advanced options**). The app's **AI models** page shows what's installed and ready. If something isn't, it shows how to set it up, with commands you can copy. Click **Check again** after installing.

| Provider | Runs through | Set up |
|---|---|---|
| **Claude** (default) | Claude Code: `claude -p` | Already required by the app. |
| **Gemini** (Google) | [Gemini CLI](https://github.com/google-gemini/gemini-cli): `gemini` | `npm install -g @google/gemini-cli`, then run `gemini` once and sign in (or set `GEMINI_API_KEY`). |
| **GPT** (OpenAI) | [Codex CLI](https://github.com/openai/codex): `codex exec` | `npm install -g @openai/codex`, then run `codex` once and sign in with ChatGPT (or set `OPENAI_API_KEY`). |
| **Local models** (Ollama) | Claude Code, pointed at [Ollama](https://ollama.com)'s Anthropic-compatible API | Install Ollama 0.14 or newer, start it (`ollama serve` or the desktop app), and download a model: `ollama pull qwen3-coder`. |

How each one works in a workflow:

- **Gemini and GPT** steps can read and edit files in the project folder. Gemini runs with `--yolo`; Codex runs with `--full-auto`, which `plan` mode makes read-only. They have no subagents, so a step's helpers are given to them as role instructions in the prompt. Steering a running step restarts it with your guidance added (Claude continues the same conversation instead). Their cost isn't reported, so it shows as $0.00.
- **Local models** keep everything Claude Code offers (tools, helpers, steering) and cost nothing. All of Claude Code's model roles map to the model you pick. Pick one that is good at coding and tool use; small models may struggle with multi-step work. To use Ollama on another machine, set `OLLAMA_HOST` (for example `OLLAMA_HOST=http://192.168.1.20:11434`).
- If a step's model isn't available when it runs (tool not installed, Ollama not running, model not downloaded), the step fails straight away with a message saying what to do.

In workflow YAML, `model:` (for the workflow or one step) is `haiku`, `sonnet`, `opus` or `fable` for Claude; `gemini` or `gemini:<model>`; `gpt` or `gpt:<model>`; or `ollama:<model>`, for example `ollama:qwen3-coder`. A plain `gemini` or `gpt` means that tool's default model. Don't write `gemini:` with nothing after the colon by hand: that isn't valid YAML unless it's quoted. Model names you add on the AI models page are offered in the menus.

## AI judge, step diffs and rewind

### AI judge

A shell `check:` can only test what a command can test. For everything else (“did it fix the root cause?”, “is every requirement in the spec covered?”), give the step acceptance criteria:

```yaml
- id: fix
  name: Fix it
  retries: 2
  prompt: Fix the bug using the plan from the previous step.
  judge: |
    - The root cause is fixed, not just the symptom
    - A test reproduces the bug and now passes
    - No unrelated files changed
```

After the step (and its `check:`, if any) succeeds, a separate model with a fresh context and no tools grades it:

- It judges each criterion on **evidence**: the git diff of everything the step changed (or, outside git, the files it wrote). The step's own summary is treated as a claim, not proof.
- The verdict is structured (pass, a 0–100 score, and met / not met with evidence for each criterion), so it can't be vague.
- If it fails, the step fails, and the judge's feedback becomes the reason given to the retry or loop-back. That makes retries an evaluate → fix loop instead of trying the same thing again.

The judge uses Haiku by default (usually a cent or two per verdict). Choose Sonnet or Opus in the workflow's **Advanced options** (`judgeModel:` in YAML). A judge that isn't the model that did the work tends to be stricter. Verdicts show on the step's **Result** tab and in the terminal (`⚖ judge 92/100`).

### Step diffs and rewind

When the workflow folder is a git repository, the project is snapshotted before and after every step. Snapshots use a private index, so your branch, staged changes, HEAD and stash are never touched; uncommitted and untracked files are included and `.gitignore` is respected.

- The step's **Changes** tab shows the files it added, edited or deleted and the full diff.
- **⏪ Rewind to before this step** puts the folder back exactly as it was when that step started (undoing it and every later change). The current state is snapshotted first, so **↶ Undo rewind** brings it all back. Rewinding is only possible while the run isn't running.
- Steps that run in parallel share the folder, so one's diff can include the other's changes.
- Snapshots are ordinary git objects that nothing references, so `git gc` removes them after a couple of weeks; old runs then show no diff.

### Token insight

Each Claude step's **Result** tab shows the tokens it read and wrote, its number of turns, and its **prompt-cache hit rate**. A low cache rate on a long step usually means the context kept changing, which costs more and runs slower.

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
| Built-in agent library, step templates and sample workflows | `agent_graph/defaults/` (`agents.yaml`, `steps.yaml`, `samples.yaml`) |

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
  quality.py    step checkpoints (diff, rewind) and the AI judge
  compat.py     the Linux / macOS / Windows differences
  index.html    the whole UI (single file, no build step)
  defaults/     built-in agent library and step templates
```

## License

MIT
