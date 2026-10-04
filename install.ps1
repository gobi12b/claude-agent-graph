# Claude Agent Graph installer for Windows (PowerShell).
#   irm https://raw.githubusercontent.com/gobi12b/claude-agent-graph/main/install.ps1 | iex
# Downloads the prebuilt agent-graph.exe into %LOCALAPPDATA%\Programs\claude-agent-graph and puts it on your PATH.
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"  # much faster downloads

$Base = if ($env:AGENT_GRAPH_BASE) { $env:AGENT_GRAPH_BASE } else { "https://raw.githubusercontent.com/gobi12b/claude-agent-graph/main" }
$Dir = Join-Path $env:LOCALAPPDATA "Programs\claude-agent-graph"
$Name = "agent-graph-windows-x86_64.exe"  # also runs on Windows on ARM, through its x64 emulation

function Say($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }

if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
    Say "Note: git isn't installed. Install Git for Windows (https://git-scm.com/download/win): Claude Code, shell steps and step diffs need it."
}
if (-not (Get-Command claude -ErrorAction SilentlyContinue)) {
    Say "Note: the claude CLI isn't on your PATH. Install Claude Code first: https://claude.com/claude-code"
}

New-Item -ItemType Directory -Force -Path $Dir | Out-Null
$Exe = Join-Path $Dir "agent-graph.exe"
$Tmp = Join-Path $Dir "agent-graph.download"
Say "Downloading $Name"
Invoke-WebRequest -UseBasicParsing -Uri "$Base/binaries/$Name" -OutFile $Tmp
$Sums = (Invoke-WebRequest -UseBasicParsing -Uri "$Base/binaries/SHA256SUMS").Content
$Want = ($Sums -split "`n" | Where-Object { $_ -match " $([regex]::Escape($Name))\s*$" } | ForEach-Object { ($_ -split " ")[0] }) | Select-Object -First 1
$Got = (Get-FileHash -Algorithm SHA256 $Tmp).Hash.ToLower()
if ($Want -and $Want -ne $Got) { Remove-Item $Tmp; throw "$Name doesn't match its checksum; try again later." }
Move-Item -Force $Tmp $Exe
Say "Installed $(& $Exe --version) to $Exe"

# Put agent-graph on the user PATH.
$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (-not ($UserPath -split ";" | Where-Object { $_ -eq $Dir })) {
    [Environment]::SetEnvironmentVariable("Path", "$UserPath;$Dir", "User")
    $env:Path = "$env:Path;$Dir"
    Say "Added $Dir to your PATH (open a new terminal to pick it up)."
}

# Start-menu shortcut.
$Lnk = Join-Path ([Environment]::GetFolderPath("Programs")) "Claude Agent Graph.lnk"
$Shell = New-Object -ComObject WScript.Shell
$Shortcut = $Shell.CreateShortcut($Lnk)
$Shortcut.TargetPath = $Exe
$Ico = Join-Path $Dir "icon.ico"
try {
    Invoke-WebRequest -UseBasicParsing -Uri "$Base/assets/icon.ico" -OutFile $Ico
    $Shortcut.IconLocation = $Ico
} catch { }  # offline: the shortcut keeps the default icon
$Shortcut.WindowStyle = 7  # minimized console
$Shortcut.Save()
Say "Added 'Claude Agent Graph' to the Start menu."

Say "Done. Run: agent-graph"
