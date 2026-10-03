"""The few places where Linux, macOS and Windows differ: starting and stopping processes,
running shell commands, opening files and showing notifications."""

import os
import shutil
import signal
import subprocess
import sys
from pathlib import Path

WINDOWS = os.name == "nt"
MAC = sys.platform == "darwin"
QUIET = {"stdout": subprocess.DEVNULL, "stderr": subprocess.DEVNULL}


def detached():
    """Popen kwargs that start a process in its own group, so it can be stopped with its children."""
    if WINDOWS:
        return {"creationflags": subprocess.CREATE_NEW_PROCESS_GROUP}
    return {"start_new_session": True}


def kill_tree(pid):
    """Stop a process started with detached(), and everything it started."""
    try:
        if WINDOWS:
            subprocess.run(["taskkill", "/PID", str(pid), "/T", "/F"], capture_output=True)
        else:
            os.killpg(pid, signal.SIGTERM)
    except (ProcessLookupError, PermissionError):
        pass


def terminate(pid):
    """Ask a process we didn't start to exit."""
    if WINDOWS:
        p = subprocess.run(["taskkill", "/PID", str(pid), "/T", "/F"], capture_output=True, text=True)
        if p.returncode != 0 and pid_alive(pid):
            raise OSError(p.stderr.strip() or p.stdout.strip() or "taskkill failed")
    else:
        os.kill(pid, signal.SIGTERM)


def pid_alive(pid):
    pid = int(pid)
    if WINDOWS:  # os.kill(pid, 0) would terminate the process on Windows
        import ctypes

        handle = ctypes.windll.kernel32.OpenProcess(0x1000, False, pid)  # PROCESS_QUERY_LIMITED_INFORMATION
        if not handle:
            return False
        code = ctypes.c_ulong()
        ok = ctypes.windll.kernel32.GetExitCodeProcess(handle, ctypes.byref(code))
        ctypes.windll.kernel32.CloseHandle(handle)
        return bool(ok) and code.value == 259  # STILL_ACTIVE
    try:
        with open(f"/proc/{pid}/stat") as f:
            return f.read().rsplit(")", 1)[1].split()[0] != "Z"  # zombies count as dead
    except FileNotFoundError:
        if os.path.isdir("/proc/self"):
            return False
    except (OSError, IndexError):
        return False
    try:  # no /proc (macOS)
        os.kill(pid, 0)
        return True
    except PermissionError:
        return True
    except OSError:
        return False


def process_command(pid):
    """The process's command line (or image name on Windows), or "" if it can't be read."""
    try:
        if WINDOWS:
            p = subprocess.run(["tasklist", "/FI", f"PID eq {int(pid)}", "/FO", "CSV", "/NH"],
                               capture_output=True, text=True)
            return p.stdout
        if os.path.exists(f"/proc/{pid}/cmdline"):
            with open(f"/proc/{pid}/cmdline", "rb") as f:
                return f.read().decode(errors="replace")
        return subprocess.run(["ps", "-o", "command=", "-p", str(int(pid))], capture_output=True, text=True).stdout
    except (OSError, ValueError):
        return ""


def looks_like_claude(pid):
    cmd = process_command(pid).lower()
    return "claude" in cmd or (WINDOWS and "node.exe" in cmd)


def _bash():
    if not WINDOWS:
        return shutil.which("bash")
    # Claude Code on Windows uses Git Bash too; avoid System32\bash.exe, which runs inside WSL.
    candidates = [os.environ.get("CLAUDE_CODE_GIT_BASH_PATH")]
    git = shutil.which("git")
    if git:
        root = Path(git).resolve().parent.parent
        candidates += [root / "bin" / "bash.exe", root / "usr" / "bin" / "bash.exe"]
    for base in (os.environ.get("ProgramFiles"), os.environ.get("ProgramFiles(x86)"), os.environ.get("LOCALAPPDATA")):
        if base:
            candidates += [Path(base) / "Git" / "bin" / "bash.exe", Path(base) / "Programs" / "Git" / "bin" / "bash.exe"]
    return next((str(c) for c in candidates if c and os.path.isfile(c)), None)


def shell_command(command):
    """argv that runs a shell step: bash where available (Git Bash on Windows), else cmd.exe."""
    bash = _bash()
    if bash:
        return [bash, "-lc", command]
    if WINDOWS:
        return ["cmd", "/d", "/s", "/c", command]
    return ["/bin/sh", "-c", command]


def shell_env(env):
    if WINDOWS:
        env = dict(env, CHERE_INVOKING="1")  # keep Git Bash's login shell in the workflow folder
    return env


def claude_cmd():
    """Full path to the claude CLI, so Windows finds claude.exe or claude.cmd."""
    return shutil.which("claude") or "claude"


def open_path(target):
    """Open a file or folder with the system's default app."""
    if WINDOWS:
        os.startfile(target)
    elif MAC:
        subprocess.Popen(["open", target], **QUIET, **detached())
    else:
        subprocess.Popen(["xdg-open", target], **QUIET, **detached())


def open_in_browser(path):
    url = Path(path).resolve().as_uri()
    if not WINDOWS and not MAC and shutil.which("xdg-settings") and shutil.which("gtk-launch"):
        desktop = subprocess.run(["xdg-settings", "get", "default-web-browser"],
                                 capture_output=True, text=True).stdout.strip()
        if desktop:
            subprocess.Popen(["gtk-launch", desktop, url], **QUIET, **detached())
            return
    import webbrowser
    webbrowser.open_new_tab(url)


def notify(title, body):
    """Best-effort desktop notification."""
    try:
        if MAC:
            subprocess.Popen(["osascript", "-e", "on run argv", "-e",
                              "display notification (item 2 of argv) with title (item 1 of argv)",
                              "-e", "end run", title, body], **QUIET)
        elif WINDOWS:
            script = ("[void][Reflection.Assembly]::LoadWithPartialName('System.Windows.Forms');"
                      "$n=New-Object System.Windows.Forms.NotifyIcon;$n.Icon=[System.Drawing.SystemIcons]::Information;"
                      "$n.Visible=$true;$n.ShowBalloonTip(8000,$env:AG_TITLE,$env:AG_BODY,'Info');Start-Sleep 9;$n.Dispose()")
            subprocess.Popen(["powershell", "-NoProfile", "-WindowStyle", "Hidden", "-Command", script],
                             env=dict(os.environ, AG_TITLE=title, AG_BODY=body),
                             creationflags=subprocess.CREATE_NO_WINDOW, **QUIET)
        elif shutil.which("notify-send"):
            subprocess.Popen(["notify-send", "-a", "Claude Agent Graph", "-i", "dialog-question", title, body], **QUIET)
    except OSError:
        pass
