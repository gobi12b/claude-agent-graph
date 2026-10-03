#!/bin/sh
# Claude Agent Graph installer for macOS and Linux.
#   curl -fsSL https://raw.githubusercontent.com/gobi12b/claude-agent-graph/main/install.sh | sh
# Installs the `agent-graph` command with uv or pipx when available, otherwise into its own
# virtual environment under ~/.local/share/claude-agent-graph.
set -eu

REPO="${AGENT_GRAPH_REPO:-https://github.com/gobi12b/claude-agent-graph}"
SRC="git+${REPO}"
# macOS gets a native window from pywebview; Linux uses the system GTK bindings (python3-gi), so the
# virtual environment is allowed to see system packages.
case "$(uname -s)" in Darwin) EXTRA="[window]"; SYSPKG="" ;; *) EXTRA=""; SYSPKG="--system-site-packages" ;; esac
[ -n "${AGENT_GRAPH_SRC:-}" ] && SRC="$AGENT_GRAPH_SRC"  # a local checkout, for testing
BIN="$HOME/.local/bin"

say() { printf '\033[1m==>\033[0m %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

command -v git >/dev/null 2>&1 || die "git is required (macOS: xcode-select --install; Linux: install git)."
if ! command -v claude >/dev/null 2>&1; then
    say "Note: the claude CLI isn't on your PATH. Install Claude Code first: https://claude.com/claude-code"
fi

if command -v uv >/dev/null 2>&1; then
    say "Installing with uv"
    uv tool install --force "claude-agent-graph${EXTRA} @ ${SRC}" || uv tool install --force "claude-agent-graph @ ${SRC}"
elif command -v pipx >/dev/null 2>&1; then
    say "Installing with pipx"
    pipx install --force $SYSPKG "claude-agent-graph${EXTRA} @ ${SRC}" || pipx install --force $SYSPKG "claude-agent-graph @ ${SRC}"
else
    PY=""
    for p in python3 python; do
        if command -v "$p" >/dev/null 2>&1 && "$p" -c 'import sys; sys.exit(sys.version_info < (3, 9))'; then
            PY="$p"; break
        fi
    done
    [ -n "$PY" ] || die "Python 3.9+ is required. Install it from https://www.python.org/downloads/ (or brew install python)."
    VENV="$HOME/.local/share/claude-agent-graph/venv"
    say "Installing into $VENV"
    "$PY" -m venv $SYSPKG "$VENV" || die "couldn't create a virtual environment (Debian/Ubuntu: sudo apt install python3-venv)."
    "$VENV/bin/python" -m pip install --quiet --upgrade pip
    "$VENV/bin/python" -m pip install --quiet --upgrade "claude-agent-graph${EXTRA} @ ${SRC}" \
        || "$VENV/bin/python" -m pip install --quiet --upgrade "claude-agent-graph @ ${SRC}"
    mkdir -p "$BIN"
    ln -sf "$VENV/bin/agent-graph" "$BIN/agent-graph"
fi

# Linux: add an app-menu entry, with the app's icon.
if [ "$(uname -s)" = "Linux" ] && [ -d "$HOME/.local/share" ]; then
    mkdir -p "$HOME/.local/share/applications" "$HOME/.local/share/claude-agent-graph"
    ICON="$HOME/.local/share/claude-agent-graph/icon.png"
    ICON_URL="${AGENT_GRAPH_ICON_URL:-https://raw.githubusercontent.com/gobi12b/claude-agent-graph/main/agent_graph/assets/icon.png}"
    if ! { curl -fsSL "$ICON_URL" -o "$ICON" 2>/dev/null || wget -qO "$ICON" "$ICON_URL" 2>/dev/null; }; then
        rm -f "$ICON"; ICON="network-workgroup"  # offline: a stock icon
    fi
    cat > "$HOME/.local/share/applications/claude-agent-graph.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Claude Agent Graph
Comment=Live view of your Claude Code sessions and workflows
Exec=$BIN/agent-graph
Icon=$ICON
StartupWMClass=claude-agent-graph
Terminal=false
Categories=Development;
EOF
fi

say "Done. Run: agent-graph"
case ":$PATH:" in *":$BIN:"*) ;; *) say "Add $BIN to your PATH if the command isn't found." ;; esac
