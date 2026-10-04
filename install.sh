#!/bin/sh
# Claude Agent Graph installer for macOS and Linux.
#   curl -fsSL https://raw.githubusercontent.com/gobi12b/claude-agent-graph/main/install.sh | sh
# Downloads the prebuilt `agent-graph` program for this computer into ~/.local/bin.
set -eu

BASE="${AGENT_GRAPH_BASE:-https://raw.githubusercontent.com/gobi12b/claude-agent-graph/main}"
BIN="${AGENT_GRAPH_BIN:-$HOME/.local/bin}"
DATA="$HOME/.local/share/claude-agent-graph"

say() { printf '\033[1m==>\033[0m %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

fetch() {  # fetch URL FILE
    if command -v curl >/dev/null 2>&1; then curl -fsSL "$1" -o "$2"
    elif command -v wget >/dev/null 2>&1; then wget -qO "$2" "$1"
    else die "curl or wget is needed to download the app."
    fi
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
    elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

command -v git >/dev/null 2>&1 || say "Note: git isn't installed. Step diffs and rewind need it (macOS: xcode-select --install)."
command -v claude >/dev/null 2>&1 || say "Note: the claude CLI isn't on your PATH. Install Claude Code first: https://claude.com/claude-code"

OS="$(uname -s)"; ARCH="$(uname -m)"
case "$OS/$ARCH" in
    Darwin/*) CHOICES="agent-graph-macos-universal" ;;
    Linux/x86_64|Linux/amd64)
        # The native window needs WebKitGTK; without it, the app opens in your browser.
        if ldconfig -p 2>/dev/null | grep -q 'libwebkit2gtk-4\.1\.so\.0'; then
            CHOICES="agent-graph-linux-x86_64-window agent-graph-linux-x86_64"
        else
            CHOICES="agent-graph-linux-x86_64"
        fi ;;
    Linux/aarch64|Linux/arm64) CHOICES="agent-graph-linux-arm64" ;;
    *) die "No prebuilt app for $OS/$ARCH yet. Build it from source: https://github.com/gobi12b/claude-agent-graph#build-from-source" ;;
esac

TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
fetch "$BASE/binaries/SHA256SUMS" "$TMP/SHA256SUMS" || die "couldn't download the checksums from $BASE/binaries/"
mkdir -p "$BIN"
INSTALLED=""
for NAME in $CHOICES; do
    say "Downloading $NAME"
    fetch "$BASE/binaries/$NAME" "$TMP/$NAME" || die "couldn't download $BASE/binaries/$NAME"
    WANT="$(grep " $NAME\$" "$TMP/SHA256SUMS" | cut -d' ' -f1)"
    GOT="$(sha256 "$TMP/$NAME" || true)"
    if [ -n "$GOT" ] && [ "$WANT" != "$GOT" ]; then die "$NAME doesn't match its checksum; try again later."; fi
    chmod +x "$TMP/$NAME"
    if "$TMP/$NAME" --version >/dev/null 2>&1; then  # runs here (system libraries are new enough)
        mv "$TMP/$NAME" "$BIN/agent-graph"
        INSTALLED="$NAME"
        break
    fi
    say "$NAME doesn't run on this system; trying the next build."
done
[ -n "$INSTALLED" ] || die "None of the prebuilt apps run here. Build it from source: https://github.com/gobi12b/claude-agent-graph#build-from-source"
say "Installed $("$BIN/agent-graph" --version) ($INSTALLED) to $BIN/agent-graph"

# Linux: add an app-menu entry, with the app's icon.
if [ "$OS" = "Linux" ] && [ -d "$HOME/.local/share" ]; then
    mkdir -p "$HOME/.local/share/applications" "$DATA"
    ICON="$DATA/icon.png"
    fetch "$BASE/assets/icon.png" "$ICON" 2>/dev/null || { rm -f "$ICON"; ICON="network-workgroup"; }  # offline: a stock icon
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
