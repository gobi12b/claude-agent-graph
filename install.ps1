# Claude Agent Graph installer for Windows (PowerShell).
#   irm https://raw.githubusercontent.com/gobi12b/claude-agent-graph/main/install.ps1 | iex
# Installs the `agent-graph` command with uv or pipx when available, otherwise into its own
# virtual environment under %LOCALAPPDATA%\claude-agent-graph.
$ErrorActionPreference = "Stop"

$Repo = if ($env:AGENT_GRAPH_REPO) { $env:AGENT_GRAPH_REPO } else { "https://github.com/gobi12b/claude-agent-graph" }
$Src = if ($env:AGENT_GRAPH_SRC) { $env:AGENT_GRAPH_SRC } else { "git+$Repo" }
$Pkg = "claude-agent-graph @ $Src"
$PkgWindow = "claude-agent-graph[window] @ $Src"

function Say($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }

if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
    throw "git is required. Install Git for Windows: https://git-scm.com/download/win (Claude Code needs it too)."
}
if (-not (Get-Command claude -ErrorAction SilentlyContinue)) {
    Say "Note: the claude CLI isn't on your PATH. Install Claude Code first: https://claude.com/claude-code"
}

if (Get-Command uv -ErrorAction SilentlyContinue) {
    Say "Installing with uv"
    uv tool install --force $PkgWindow
    if ($LASTEXITCODE -ne 0) { uv tool install --force $Pkg }
}
elseif (Get-Command pipx -ErrorAction SilentlyContinue) {
    Say "Installing with pipx"
    pipx install --force $PkgWindow
    if ($LASTEXITCODE -ne 0) { pipx install --force $Pkg }
}
else {
    $Py = $null
    foreach ($cand in @(@("py", "-3"), @("python"), @("python3"))) {
        if (Get-Command $cand[0] -ErrorAction SilentlyContinue) {
            $args0 = @($cand | Select-Object -Skip 1)
            & $cand[0] @args0 -c "import sys; sys.exit(sys.version_info < (3, 9))" 2>$null
            if ($LASTEXITCODE -eq 0) { $Py = $cand; break }
        }
    }
    if (-not $Py) { throw "Python 3.9+ is required. Install it from https://www.python.org/downloads/ (tick 'Add python.exe to PATH')." }
    $Home0 = Join-Path $env:LOCALAPPDATA "claude-agent-graph"
    $Venv = Join-Path $Home0 "venv"
    Say "Installing into $Venv"
    $pyArgs = @($Py | Select-Object -Skip 1)
    & $Py[0] @pyArgs -m venv $Venv
    $VPy = Join-Path $Venv "Scripts\python.exe"
    & $VPy -m pip install --quiet --upgrade pip
    & $VPy -m pip install --quiet --upgrade $PkgWindow
    if ($LASTEXITCODE -ne 0) { & $VPy -m pip install --quiet --upgrade $Pkg }
    if ($LASTEXITCODE -ne 0) { throw "pip install failed." }

    # Put agent-graph on the user PATH.
    $Scripts = Join-Path $Venv "Scripts"
    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if (-not ($UserPath -split ";" | Where-Object { $_ -eq $Scripts })) {
        [Environment]::SetEnvironmentVariable("Path", "$UserPath;$Scripts", "User")
        $env:Path = "$env:Path;$Scripts"
        Say "Added $Scripts to your PATH (open a new terminal to pick it up)."
    }
}

# Start-menu shortcut.
$Exe = (Get-Command agent-graph -ErrorAction SilentlyContinue).Source
if ($Exe) {
    $Lnk = Join-Path ([Environment]::GetFolderPath("Programs")) "Claude Agent Graph.lnk"
    $Shell = New-Object -ComObject WScript.Shell
    $Shortcut = $Shell.CreateShortcut($Lnk)
    $Shortcut.TargetPath = $Exe
    $Shortcut.WindowStyle = 7  # minimized console
    $Shortcut.Save()
    Say "Added 'Claude Agent Graph' to the Start menu."
}

Say "Done. Run: agent-graph"
