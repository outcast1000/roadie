//! Where Roadie keeps things. One data root (Tauri's `app_data_dir`, or the
//! `--data-dir` a launcher passes), set once, then per-tool directories under
//! it. Everything is per-user and independent of any window or profile,
//! because a daemon is one per user.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static DATA_ROOT: OnceLock<PathBuf> = OnceLock::new();

pub fn init(root: PathBuf) {
    let _ = DATA_ROOT.set(root);
}

/// Desktop release: Tauri's `app_data_dir` for `com.outcast1000.roadie`,
/// computed without Tauri so the headless service lands in the same place
/// as the window. CLI release: its own `com.outcast1000.roadie.cli`, so the
/// two releases never share state by accident. An app that bundles the CLI
/// passes its own `--data-dir`.
pub fn default_data_root() -> PathBuf {
    let id = if cfg!(feature = "service") { "com.outcast1000.roadie" } else { "com.outcast1000.roadie.cli" };
    #[cfg(target_os = "macos")]
    {
        home_dir().join("Library").join("Application Support").join(id)
    }
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| home_dir().join("AppData").join("Roaming")).join(id)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| home_dir().join(".local").join("share")).join(id)
    }
}

pub fn data_root() -> Result<&'static Path, String> {
    DATA_ROOT
        .get()
        .map(|p| p.as_path())
        .ok_or_else(|| "data root not initialized".to_string())
}

/// `tools/<name>/versions/<tag>/` holds installed releases side by side,
/// `data/` the tool's config, state, pid file and secrets, `logs/` its output.
#[derive(Debug, Clone)]
pub struct ToolPaths {
    pub root: PathBuf,
    pub versions: PathBuf,
    pub data: PathBuf,
    pub logs: PathBuf,
}

pub fn tool_paths(name: &str) -> Result<ToolPaths, String> {
    let root = data_root()?.join("tools").join(name);
    Ok(ToolPaths {
        versions: root.join("versions"),
        data: root.join("data"),
        logs: root.join("logs"),
        root,
    })
}

/// Stable entry points for `cli` tools (`bin/<name>`), so consumers get one
/// path that survives updates.
pub fn bin_dir() -> Result<PathBuf, String> {
    Ok(data_root()?.join("bin"))
}

/// User-authored and draft recipes. Built-ins are compiled in.
pub fn recipes_dir() -> Result<PathBuf, String> {
    Ok(data_root()?.join("recipes"))
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Temp + rename write; `secret` files are 0600 on unix (Windows relies on
/// the per-user AppData ACL).
pub fn write_atomic(path: &Path, contents: &[u8], secret: bool) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
        if secret {
            restrict_dir(parent);
        }
    }
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = path.with_file_name(format!(".{name}.tmp"));
    std::fs::write(&tmp, contents).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    if secret {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("write {}: {e}", path.display())
    })
}

pub fn restrict_dir(dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn random_hex(bytes: usize) -> Result<String, String> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).map_err(|e| format!("Failed to gather entropy: {e}"))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}
/// An exclusive lock on a file, held until dropped. Two Roadie processes on
/// one data dir (two CLI runs, or a CLI run and a `maintain` at login) take
/// it around every change to a tool. Unix `flock` and Windows `LockFileEx`
/// both conflict between separate opens, in one process or several.
pub struct FileLock {
    _file: std::fs::File,
}

pub fn lock_file(path: &Path) -> Result<FileLock, String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    let file = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(path).map_err(|e| format!("open lock {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        loop {
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if rc == 0 {
                break;
            }
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::Interrupted {
                return Err(format!("lock {}: {e}", path.display()));
            }
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        #[repr(C)]
        struct Overlapped {
            internal: usize,
            internal_high: usize,
            offset: u32,
            offset_high: u32,
            event: *mut std::ffi::c_void,
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn LockFileEx(file: *mut std::ffi::c_void, flags: u32, reserved: u32, low: u32, high: u32, overlapped: *mut Overlapped) -> i32;
        }
        const LOCKFILE_EXCLUSIVE_LOCK: u32 = 0x2;
        let mut ov = Overlapped { internal: 0, internal_high: 0, offset: 0, offset_high: 0, event: std::ptr::null_mut() };
        let ok = unsafe { LockFileEx(file.as_raw_handle() as *mut _, LOCKFILE_EXCLUSIVE_LOCK, 0, u32::MAX, u32::MAX, &mut ov) };
        if ok == 0 {
            return Err(format!("lock {}: {}", path.display(), std::io::Error::last_os_error()));
        }
    }
    Ok(FileLock { _file: file })
}

#[cfg(test)]
mod lock_tests {
    use super::*;

    #[test]
    fn a_file_lock_excludes_a_second_holder_until_dropped() {
        let path = std::env::temp_dir().join(format!("roadie-lock-test-{}", std::process::id())).join("t.lock");
        let first = lock_file(&path).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let p2 = path.clone();
        let waiter = std::thread::spawn(move || {
            let _second = lock_file(&p2).unwrap();
            tx.send(()).unwrap();
        });
        assert!(rx.recv_timeout(std::time::Duration::from_millis(300)).is_err(), "the second open must wait");
        drop(first);
        rx.recv_timeout(std::time::Duration::from_secs(5)).expect("released on drop");
        waiter.join().unwrap();
    }
}
