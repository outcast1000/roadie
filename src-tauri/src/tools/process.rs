//! Spawn, find and stop a daemon — as an **independent** process.
//!
//! Daemons outlive Roadie: nothing here ties the child's lifetime to ours (no
//! pipes held, unix children get their own session, Windows children a
//! hidden console). Liveness is a pid file plus "is that pid really our
//! binary", plus the recipe's HTTP health probe — a recycled pid or a
//! foreign instance on our port must not read as "running".
//!
//! Stopping is a ladder: the recipe's own API route, then a signal
//! (SIGTERM / Ctrl-Break), then a hard kill.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const PID_FILE: &str = "tool.pid";
pub const STDOUT_LOG: &str = "stdout.log";
pub const LAUNCHER_LOG: &str = "launcher.log";

#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
pub const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
#[cfg(windows)]
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PidFile {
    pub pid: u32,
    pub version: String,
    pub exe: PathBuf,
    pub started_at: u64,
    pub started_by: String,
}

pub fn pid_path(data_dir: &Path) -> PathBuf {
    data_dir.join(PID_FILE)
}

pub fn read_pid_file(data_dir: &Path) -> Option<PidFile> {
    serde_json::from_str(&std::fs::read_to_string(pid_path(data_dir)).ok()?).ok()
}

pub fn write_pid_file(data_dir: &Path, pf: &PidFile) -> Result<(), String> {
    let text = serde_json::to_string_pretty(pf).map_err(|e| e.to_string())?;
    std::fs::write(pid_path(data_dir), text).map_err(|e| format!("pid file write error: {e}"))
}

pub fn remove_pid_file(data_dir: &Path) {
    let _ = std::fs::remove_file(pid_path(data_dir));
}

pub struct SpawnPlan {
    pub exe: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
    pub log: PathBuf,
    /// Tools truncate per start (the failure classifier reads "the last
    /// start's output"); the service appends so restarts keep history.
    pub append_log: bool,
}

/// Start the daemon detached from this process. Returns its pid.
pub fn spawn_detached(p: &SpawnPlan) -> Result<u32, String> {
    if let Some(parent) = p.log.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    // Truncate the log per start so "the last start's output" is what the
    // failure classifier reads — unless the plan asks to append.
    let log = if p.append_log {
        std::fs::OpenOptions::new().create(true).append(true).open(&p.log)
    } else {
        std::fs::File::create(&p.log)
    }
    .map_err(|e| format!("open {}: {e}", p.log.display()))?;
    let log_err = log.try_clone().map_err(|e| format!("log handle error: {e}"))?;

    let mut cmd = std::process::Command::new(&p.exe);
    cmd.args(&p.args).stdin(std::process::Stdio::null()).stdout(log).stderr(log_err);
    cmd.current_dir(p.cwd.clone().or_else(|| p.exe.parent().map(Path::to_path_buf)).unwrap_or_default());
    for (k, v) in &p.env {
        cmd.env(k, v);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // New session: own process group, no controlling terminal.
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
        // A *hidden* console rather than DETACHED_PROCESS: the Ctrl-Break
        // fallback needs a console to attach to.
        // Out of our Job object too, when it allows that: a launcher, terminal or CI runner
        // that kills its job on close would otherwise take the daemon (or the service) with it.
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB);
        // CreateProcess passes on *every* inheritable handle, and our own std handles are
        // inheritable when a caller piped them to us (PowerShell, `Command::output`). The
        // child would then hold the caller's pipe open until it exits, so `roadie tool …`'s
        // output never reaches EOF while the service it started runs. The handles meant for
        // the child are fresh duplicates made by std, so ours need not be inheritable.
        unsafe { win::disinherit_std_handles() };
    }
    let spawned = cmd.spawn();
    // A job without JOB_OBJECT_LIMIT_BREAKAWAY_OK refuses the breakaway with access denied;
    // start inside the job then, which is still better than not starting.
    #[cfg(windows)]
    let spawned = match spawned {
        Err(e) if e.raw_os_error() == Some(5) => {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP).spawn()
        }
        other => other,
    };
    let mut child = spawned.map_err(|e| format!("failed to start {}: {e}", p.exe.display()))?;
    let pid = child.id();
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(pid)
}

// --- Liveness ---

pub fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        if unsafe { libc::kill(pid as i32, 0) } == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    unsafe {
        win::pid_alive(pid)
    }
}

pub fn pid_exe(pid: u32) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    unsafe {
        let mut buf = vec![0u8; 4096];
        let n = libc::proc_pidpath(pid as i32, buf.as_mut_ptr() as *mut libc::c_void, buf.len() as u32);
        if n <= 0 {
            return None;
        }
        buf.truncate(n as usize);
        Some(PathBuf::from(String::from_utf8_lossy(&buf).into_owned()))
    }
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/exe")).ok()
    }
    #[cfg(windows)]
    unsafe {
        win::pid_exe(pid)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        let _ = pid;
        None
    }
}

/// Alive **and** running a binary out of our versions dir.
pub fn is_ours(pid: u32, versions_dir: &Path) -> bool {
    if !pid_alive(pid) {
        return false;
    }
    match pid_exe(pid) {
        Some(exe) => {
            let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
            let dir = std::fs::canonicalize(versions_dir).unwrap_or_else(|_| versions_dir.to_path_buf());
            exe.starts_with(dir)
        }
        None => true,
    }
}

// --- Stop ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum StopMethod {
    Api,
    Signal,
    Forced,
}

fn wait_gone(pid: Option<u32>, still_answering: &dyn Fn() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let gone = match pid {
            Some(pid) => !pid_alive(pid),
            None => !still_answering(),
        };
        if gone {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

/// Stop the daemon. `api_stop` is the recipe's graceful route (`None` when it
/// has none); `pid` is `None` when we adopted an instance we did not spawn.
pub fn stop(
    name: &str,
    pid: Option<u32>,
    api_stop: Option<&dyn Fn() -> Result<(), String>>,
    still_answering: &dyn Fn() -> bool,
    grace: Duration,
) -> Result<StopMethod, String> {
    if let Some(api) = api_stop {
        match api() {
            Ok(()) => {
                if wait_gone(pid, still_answering, grace) {
                    return Ok(StopMethod::Api);
                }
                log::warn!("{name} accepted shutdown but is still alive after {grace:?}");
            }
            Err(e) => log::warn!("{name} API shutdown unavailable ({e}); falling back to a signal"),
        }
    }
    let Some(pid) = pid else {
        return Err("the daemon did not stop via its API and no pid is known to signal".to_string());
    };
    if send_terminate(pid) && wait_gone(Some(pid), still_answering, grace) {
        return Ok(StopMethod::Signal);
    }
    log::warn!("forcing {name} (pid {pid}) to exit");
    force_kill(pid);
    if wait_gone(Some(pid), still_answering, Duration::from_secs(5)) {
        Ok(StopMethod::Forced)
    } else {
        Err(format!("{name} (pid {pid}) could not be stopped"))
    }
}

fn send_terminate(pid: u32) -> bool {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM) == 0
    }
    #[cfg(windows)]
    unsafe {
        win::send_ctrl_break(pid)
    }
}

fn force_kill(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
    #[cfg(windows)]
    unsafe {
        win::terminate(pid);
    }
}

pub fn log_tail(logs_dir: &Path, lines: usize) -> String {
    let text = std::fs::read_to_string(logs_dir.join(STDOUT_LOG)).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

// --- Windows FFI (hand-declared; no crate) ---
#[cfg(windows)]
mod win {
    use std::path::PathBuf;

    type HANDLE = *mut std::ffi::c_void;
    type BOOL = i32;
    type DWORD = u32;

    const PROCESS_QUERY_LIMITED_INFORMATION: DWORD = 0x1000;
    const PROCESS_TERMINATE: DWORD = 0x0001;
    const STILL_ACTIVE: DWORD = 259;
    const CTRL_BREAK_EVENT: DWORD = 1;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: DWORD, inherit: BOOL, pid: DWORD) -> HANDLE;
        fn CloseHandle(h: HANDLE) -> BOOL;
        fn GetExitCodeProcess(h: HANDLE, code: *mut DWORD) -> BOOL;
        fn QueryFullProcessImageNameW(h: HANDLE, flags: DWORD, name: *mut u16, size: *mut DWORD) -> BOOL;
        fn TerminateProcess(h: HANDLE, code: u32) -> BOOL;
        fn FreeConsole() -> BOOL;
        fn AttachConsole(pid: DWORD) -> BOOL;
        fn SetConsoleCtrlHandler(handler: *const std::ffi::c_void, add: BOOL) -> BOOL;
        fn GenerateConsoleCtrlEvent(event: DWORD, group: DWORD) -> BOOL;
        fn GetStdHandle(which: DWORD) -> HANDLE;
        fn SetHandleInformation(h: HANDLE, mask: DWORD, flags: DWORD) -> BOOL;
    }

    const STD_HANDLES: [DWORD; 3] = [-10i32 as DWORD, -11i32 as DWORD, -12i32 as DWORD];
    const HANDLE_FLAG_INHERIT: DWORD = 0x1;

    /// Stop our stdin/stdout/stderr from leaking into children we spawn.
    pub unsafe fn disinherit_std_handles() {
        for which in STD_HANDLES {
            let h = GetStdHandle(which);
            if !h.is_null() && h != (-1isize as HANDLE) {
                SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }

    pub unsafe fn pid_alive(pid: u32) -> bool {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let mut code: DWORD = 0;
        let ok = GetExitCodeProcess(h, &mut code) != 0;
        CloseHandle(h);
        ok && code == STILL_ACTIVE
    }

    pub unsafe fn pid_exe(pid: u32) -> Option<PathBuf> {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let mut buf = vec![0u16; 32768];
        let mut len: DWORD = buf.len() as DWORD;
        let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len) != 0;
        CloseHandle(h);
        if !ok {
            return None;
        }
        buf.truncate(len as usize);
        Some(PathBuf::from(String::from_utf16_lossy(&buf)))
    }

    pub unsafe fn terminate(pid: u32) {
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if h.is_null() {
            return;
        }
        TerminateProcess(h, 1);
        CloseHandle(h);
    }

    /// Deliver Ctrl-Break to the daemon's hidden console. Roadie is a GUI
    /// process with no console of its own, so attaching is clean; the
    /// handler is disabled first so the event never lands on us.
    pub unsafe fn send_ctrl_break(pid: u32) -> bool {
        FreeConsole();
        if AttachConsole(pid) == 0 {
            return false;
        }
        SetConsoleCtrlHandler(std::ptr::null(), 1);
        let ok = GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, 0) != 0;
        std::thread::sleep(std::time::Duration::from_millis(200));
        FreeConsole();
        SetConsoleCtrlHandler(std::ptr::null(), 0);
        ok
    }
}
