//! The few places where Linux, macOS and Windows differ: starting and stopping processes,
//! running shell commands, opening files and showing notifications.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::util::{which, Res};

pub const WINDOWS: bool = cfg!(windows);
pub const MAC: bool = cfg!(target_os = "macos");

/// Start a process in its own group, so it can be stopped with its children.
pub fn detached(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe and touches nothing in the parent.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
}

fn quiet(cmd: &mut Command) -> &mut Command {
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
}

/// Start a helper in the background, without output, and reap it when it exits.
pub fn spawn_bg(cmd: &mut Command) -> std::io::Result<()> {
    let mut child = quiet(cmd).spawn()?;
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// Stop a process started with detached(), and everything it started.
pub fn kill_tree(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::killpg(pid as libc::pid_t, libc::SIGTERM);
    }
    #[cfg(windows)]
    {
        let _ = quiet(&mut Command::new("taskkill")).args(["/PID", &pid.to_string(), "/T", "/F"]).status();
    }
}

/// Ask a process we didn't start to exit.
pub fn terminate(pid: u32) -> Res<()> {
    #[cfg(unix)]
    {
        if unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let out = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() && pid_alive(pid) {
            let msg = String::from_utf8_lossy(if out.stderr.is_empty() { &out.stdout } else { &out.stderr }).trim().to_string();
            return Err(if msg.is_empty() { "taskkill failed".into() } else { msg });
        }
        Ok(())
    }
}

/// Ask another process to stop as if Ctrl+C was pressed in its terminal.
pub fn interrupt(pid: u32) -> Res<()> {
    #[cfg(unix)]
    {
        if unsafe { libc::kill(pid as libc::pid_t, libc::SIGINT) } != 0 {
            return Err("That run's terminal is already closed.".into());
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let _ = pid; // no portable way to send Ctrl+C to another console
        Err("Stop it in its terminal with Ctrl+C.".into())
    }
}

pub fn pid_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return false;
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(handle, &mut code);
            CloseHandle(handle);
            ok != 0 && code == 259 // STILL_ACTIVE
        }
    }
    #[cfg(unix)]
    {
        if pid == 0 || pid > i32::MAX as u32 {
            return false;
        }
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            // zombies count as dead
            Ok(stat) => return stat.rsplit_once(')').and_then(|(_, r)| r.split_whitespace().next()).is_some_and(|s| s != "Z"),
            Err(_) if Path::new("/proc/self").is_dir() => return false,
            Err(_) => {}
        }
        // no /proc (macOS)
        if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

/// The process's command line (or image name on Windows), or "" if it can't be read.
pub fn process_command(pid: u32) -> String {
    if WINDOWS {
        return Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
    }
    let proc_path = format!("/proc/{pid}/cmdline");
    if Path::new(&proc_path).exists() {
        return std::fs::read(&proc_path).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
    }
    Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

pub fn looks_like_claude(pid: u32) -> bool {
    let cmd = process_command(pid).to_lowercase();
    cmd.contains("claude") || (WINDOWS && cmd.contains("node.exe"))
}

fn bash() -> Option<String> {
    if !WINDOWS {
        return which("bash");
    }
    // Claude Code on Windows uses Git Bash too; avoid System32\bash.exe, which runs inside WSL.
    let mut candidates: Vec<std::path::PathBuf> = vec![];
    if let Ok(p) = std::env::var("CLAUDE_CODE_GIT_BASH_PATH") {
        candidates.push(p.into());
    }
    if let Some(git) = which("git") {
        if let Some(root) = std::fs::canonicalize(&git).ok().and_then(|p| p.parent()?.parent().map(Path::to_path_buf)) {
            candidates.push(root.join("bin").join("bash.exe"));
            candidates.push(root.join("usr").join("bin").join("bash.exe"));
        }
    }
    for var in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
        if let Ok(base) = std::env::var(var) {
            candidates.push(Path::new(&base).join("Git").join("bin").join("bash.exe"));
            candidates.push(Path::new(&base).join("Programs").join("Git").join("bin").join("bash.exe"));
        }
    }
    candidates.into_iter().find(|c| c.is_file()).map(|c| c.to_string_lossy().into_owned())
}

/// A command that runs a shell step: bash where available (Git Bash on Windows), else cmd.exe.
pub fn shell_command(command: &str) -> Command {
    let mut cmd;
    if let Some(bash) = bash() {
        cmd = Command::new(bash);
        cmd.args(["-lc", command]);
    } else if WINDOWS {
        cmd = Command::new("cmd");
        cmd.args(["/d", "/s", "/c", command]);
    } else {
        cmd = Command::new("/bin/sh");
        cmd.args(["-c", command]);
    }
    if WINDOWS {
        cmd.env("CHERE_INVOKING", "1"); // keep Git Bash's login shell in the workflow folder
    }
    cmd
}

/// Full path to the claude CLI, so Windows finds claude.exe or claude.cmd.
pub fn claude_cmd() -> String {
    which("claude").unwrap_or_else(|| "claude".into())
}

/// Open a file or folder with the system's default app.
pub fn open_path(target: &str) -> Res<()> {
    let mut cmd = if WINDOWS {
        let mut c = Command::new("cmd");
        c.args(["/c", "start", "", target]);
        c
    } else if MAC {
        let mut c = Command::new("open");
        c.arg(target);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(target);
        c
    };
    detached(&mut cmd);
    spawn_bg(&mut cmd).map_err(|e| e.to_string())
}

pub fn open_in_browser(path: &str) -> Res<()> {
    let abs = std::fs::canonicalize(path).map_err(|e| e.to_string())?;
    let url = file_url(&abs);
    if !WINDOWS && !MAC && which("xdg-settings").is_some() && which("gtk-launch").is_some() {
        let desktop = Command::new("xdg-settings")
            .args(["get", "default-web-browser"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        if !desktop.is_empty() {
            let mut cmd = Command::new("gtk-launch");
            cmd.args([&desktop, &url]);
            detached(&mut cmd);
            if spawn_bg(&mut cmd).is_ok() {
                return Ok(());
            }
        }
    }
    open_url(&url)
}

/// Open a web address in the default browser.
pub fn open_url(url: &str) -> Res<()> {
    #[cfg(target_os = "macos")]
    {
        spawn_bg(Command::new("open").arg(url)).map_err(|e| e.to_string())
    }
    #[cfg(not(target_os = "macos"))]
    {
        webbrowser::open(url).map_err(|e| e.to_string())
    }
}

fn file_url(path: &Path) -> String {
    let mut s = path.to_string_lossy().replace('\\', "/");
    if let Some(rest) = s.strip_prefix("//?/") {
        s = rest.to_string();
    }
    if !s.starts_with('/') {
        s = format!("/{s}");
    }
    let mut out = String::from("file://");
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' | b':' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Best-effort desktop notification.
pub fn notify(title: &str, body: &str) {
    let mut cmd;
    if MAC {
        cmd = Command::new("osascript");
        cmd.args([
            "-e",
            "on run argv",
            "-e",
            "display notification (item 2 of argv) with title (item 1 of argv)",
            "-e",
            "end run",
            title,
            body,
        ]);
    } else if WINDOWS {
        let script = "[void][Reflection.Assembly]::LoadWithPartialName('System.Windows.Forms');\
            $n=New-Object System.Windows.Forms.NotifyIcon;$n.Icon=[System.Drawing.SystemIcons]::Information;\
            $n.Visible=$true;$n.ShowBalloonTip(8000,$env:AG_TITLE,$env:AG_BODY,'Info');Start-Sleep 9;$n.Dispose()";
        cmd = Command::new("powershell");
        cmd.args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", script]).env("AG_TITLE", title).env("AG_BODY", body);
        detached(&mut cmd);
    } else if which("notify-send").is_some() {
        cmd = Command::new("notify-send");
        cmd.args(["-a", "Claude Agent Graph", "-i", "dialog-question", title, body]);
    } else {
        return;
    }
    let _ = spawn_bg(&mut cmd);
}
