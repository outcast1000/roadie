//! The owner channel: how the service knows a request comes from *the
//! user's window* and not from some other program running as the same user.
//!
//! The bearer token in `roadie-api.json` protects nothing from a same-user
//! process (it can read the file), and that is fine for start/stop. It is
//! not fine for approving an install: "nothing installs without the user's
//! click" must hold against local software too. So approvals and the other
//! owner-only routes need an **owner token**, and the only way to get one is
//! to connect to a credentialed local socket where the service reads the
//! peer's pid from the kernel and checks that the peer runs *this very
//! binary*. A connection that passes gets a fresh random token, valid while
//! the connection stays open; the window keeps it open for its lifetime.
//!
//! Unix: a Unix domain socket in the data dir (`SO_PEERCRED` on Linux,
//! `LOCAL_PEERPID` on macOS). Windows: a named pipe and
//! `GetNamedPipeClientProcessId`. No crate; the FFI is hand-declared like
//! the rest of `process.rs`.

use crate::tools::process;
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

pub const HEADER: &str = "x-roadie-owner";
#[cfg(unix)]
const SOCKET_NAME: &str = "owner.sock";

fn tokens() -> &'static Mutex<HashSet<String>> {
    static T: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Is this a live owner token? Constant-time enough for a 64-hex secret
/// compared against a handful of entries.
pub fn is_owner(token: &str) -> bool {
    tokens().lock().unwrap().iter().any(|t| {
        t.len() == token.len() && t.bytes().zip(token.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
    })
}

pub fn owners_connected() -> usize {
    tokens().lock().unwrap().len()
}

fn register(token: String) {
    tokens().lock().unwrap().insert(token);
}

fn unregister(token: &str) {
    let remaining = {
        let mut t = tokens().lock().unwrap();
        t.remove(token);
        t.len()
    };
    if remaining == 0 {
        if let Some(f) = ON_LAST_GONE.get() {
            f();
        }
    }
}

static ON_LAST_GONE: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

/// Called when the last owner (window) disconnects — the service uses it to
/// exit when "run in the background" is off.
pub fn on_last_owner_gone(f: impl Fn() + Send + Sync + 'static) {
    let _ = ON_LAST_GONE.set(Box::new(f));
}

/// Tests and in-process callers register a token without a socket.
#[cfg(test)]
pub fn register_for_test() -> String {
    let t = crate::paths::random_hex(32).unwrap();
    register(t.clone());
    t
}

/// The binary the service runs; a peer has to run the same one.
fn our_exe() -> Option<PathBuf> {
    std::env::current_exe().ok().and_then(|p| std::fs::canonicalize(p).ok())
}

/// Does `pid` run our executable? The kernel told us the pid, so the peer
/// cannot lie about it; `pid_exe` asks the kernel again for its image path.
pub fn peer_is_roadie(pid: u32) -> bool {
    let (Some(ours), Some(theirs)) = (our_exe(), process::pid_exe(pid).and_then(|p| std::fs::canonicalize(p).ok())) else {
        return false;
    };
    ours == theirs
}

/// Where the socket lives. Unix: inside the (0700) data dir. Windows: a
/// per-data-dir pipe name.
pub fn socket_path(data_root: &Path) -> PathBuf {
    #[cfg(unix)]
    {
        data_root.join(SOCKET_NAME)
    }
    #[cfg(windows)]
    {
        use sha2::{Digest, Sha256};
        let h = Sha256::digest(data_root.to_string_lossy().as_bytes());
        PathBuf::from(format!(r"\\.\pipe\com.outcast1000.roadie.owner.{:x}", u64::from_be_bytes(h[..8].try_into().unwrap())))
    }
}

/// One accepted connection: verify, mint, hand over, hold until EOF.
fn serve_peer<S: std::io::Read + Write>(mut stream: S, pid: Option<u32>) {
    let Some(pid) = pid else {
        let _ = stream.write_all(b"denied: no peer credentials\n");
        return;
    };
    if !peer_is_roadie(pid) {
        log::warn!("owner channel: pid {pid} is not Roadie ({:?}); refused", process::pid_exe(pid));
        let _ = stream.write_all(b"denied: not the Roadie binary\n");
        return;
    }
    let Ok(token) = crate::paths::random_hex(32) else { return };
    register(token.clone());
    log::info!("owner channel: window pid {pid} connected");
    if stream.write_all(format!("{token}\n").as_bytes()).is_ok() {
        // Hold the token for as long as the window keeps the socket open.
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        while let Ok(n) = reader.read_line(&mut line) {
            if n == 0 {
                break;
            }
            line.clear();
        }
    }
    unregister(&token);
    log::info!("owner channel: window pid {pid} disconnected");
}

/// Start accepting owner connections on a background thread.
pub fn serve(data_root: &Path) -> Result<(), String> {
    let path = socket_path(data_root);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::UnixListener;
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).map_err(|e| format!("bind {}: {e}", path.display()))?;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        std::thread::Builder::new()
            .name("owner-channel".into())
            .spawn(move || {
                for conn in listener.incoming() {
                    match conn {
                        Ok(stream) => {
                            let pid = unix::peer_pid(&stream);
                            std::thread::spawn(move || serve_peer(stream, pid));
                        }
                        Err(e) => log::warn!("owner channel accept: {e}"),
                    }
                }
            })
            .map_err(|e| format!("owner channel thread: {e}"))?;
        Ok(())
    }
    #[cfg(windows)]
    {
        // The first instance exists before `serve` returns (like `bind` on unix), and each
        // next one is created before a connected one is handed off, so a client never finds
        // the name missing for longer than `connect`'s retry.
        let mut listening = win::create(&path)?;
        std::thread::Builder::new()
            .name("owner-channel".into())
            .spawn(move || loop {
                let waited = win::wait(&listening);
                if let Err(e) = &waited {
                    log::warn!("owner channel: {e}");
                    std::thread::sleep(std::time::Duration::from_secs(2));
                }
                let next = loop {
                    match win::create(&path) {
                        Ok(p) => break p,
                        Err(e) => {
                            log::warn!("owner channel: {e}");
                            std::thread::sleep(std::time::Duration::from_secs(2));
                        }
                    }
                };
                let connected = std::mem::replace(&mut listening, next);
                if let Ok(pid) = waited {
                    std::thread::spawn(move || serve_peer(connected, pid));
                }
            })
            .map_err(|e| format!("owner channel thread: {e}"))?;
        Ok(())
    }
}

/// Client side (the window): connect, read the token, keep the connection
/// alive for the life of the process.
pub fn connect(data_root: &Path) -> Result<String, String> {
    let path = socket_path(data_root);
    #[cfg(unix)]
    let stream = std::os::unix::net::UnixStream::connect(&path).map_err(|e| format!("owner channel {}: {e}", path.display()))?;
    #[cfg(windows)]
    let stream = {
        // Between two clients the service is re-creating the listening instance: the name is
        // briefly missing (2) or busy (231). Retry for a moment rather than failing the window.
        let mut tries = 0;
        loop {
            match std::fs::OpenOptions::new().read(true).write(true).open(&path) {
                Ok(s) => break s,
                Err(e) if tries < 40 && matches!(e.raw_os_error(), Some(2) | Some(231)) => {
                    tries += 1;
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(e) => return Err(format!("owner channel {}: {e}", path.display())),
            }
        }
    };
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| format!("owner channel read: {e}"))?;
    let line = line.trim().to_string();
    if line.len() != 64 || !line.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("owner channel refused: {line}"));
    }
    HELD.get_or_init(|| Mutex::new(Vec::new())).lock().unwrap().push(Box::new(stream));
    Ok(line)
}

/// Streams the window keeps open so its owner tokens stay valid.
static HELD: OnceLock<Mutex<Vec<Box<dyn std::any::Any + Send>>>> = OnceLock::new();

#[cfg(unix)]
mod unix {
    use std::os::unix::io::AsRawFd;
    use std::os::unix::net::UnixStream;

    pub fn peer_pid(stream: &UnixStream) -> Option<u32> {
        #[cfg(target_os = "linux")]
        unsafe {
            let mut cred: libc::ucred = std::mem::zeroed();
            let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
            let rc = libc::getsockopt(stream.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, &mut cred as *mut _ as *mut libc::c_void, &mut len);
            (rc == 0).then_some(cred.pid as u32)
        }
        #[cfg(target_os = "macos")]
        unsafe {
            let mut pid: libc::pid_t = 0;
            let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
            let rc = libc::getsockopt(stream.as_raw_fd(), libc::SOL_LOCAL, libc::LOCAL_PEERPID, &mut pid as *mut _ as *mut libc::c_void, &mut len);
            (rc == 0 && pid > 0).then_some(pid as u32)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = stream;
            None
        }
    }
}

/// Windows: a message-mode named pipe. Code-complete, untested here — the
/// owner runs the Windows build.
#[cfg(windows)]
mod win {
    use std::io::{Read, Write};
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    type HANDLE = *mut std::ffi::c_void;
    const INVALID_HANDLE_VALUE: HANDLE = -1isize as HANDLE;
    const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
    const PIPE_TYPE_BYTE: u32 = 0x0000_0000;
    const PIPE_WAIT: u32 = 0x0000_0000;
    const PIPE_REJECT_REMOTE_CLIENTS: u32 = 0x0000_0008;
    const PIPE_UNLIMITED_INSTANCES: u32 = 255;
    const ERROR_PIPE_CONNECTED: u32 = 535;

    unsafe extern "system" {
        fn CreateNamedPipeW(name: *const u16, open_mode: u32, pipe_mode: u32, max_instances: u32, out_buf: u32, in_buf: u32, timeout: u32, sa: *mut std::ffi::c_void) -> HANDLE;
        fn ConnectNamedPipe(pipe: HANDLE, overlapped: *mut std::ffi::c_void) -> i32;
        fn GetNamedPipeClientProcessId(pipe: HANDLE, pid: *mut u32) -> i32;
        fn ReadFile(h: HANDLE, buf: *mut u8, n: u32, read: *mut u32, overlapped: *mut std::ffi::c_void) -> i32;
        fn WriteFile(h: HANDLE, buf: *const u8, n: u32, written: *mut u32, overlapped: *mut std::ffi::c_void) -> i32;
        fn DisconnectNamedPipe(pipe: HANDLE) -> i32;
        fn CloseHandle(h: HANDLE) -> i32;
        fn GetLastError() -> u32;
    }

    pub struct Pipe(HANDLE);
    unsafe impl Send for Pipe {}
    impl Drop for Pipe {
        fn drop(&mut self) {
            unsafe {
                DisconnectNamedPipe(self.0);
                CloseHandle(self.0);
            }
        }
    }
    impl Read for Pipe {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let mut n = 0u32;
            let ok = unsafe { ReadFile(self.0, buf.as_mut_ptr(), buf.len() as u32, &mut n, std::ptr::null_mut()) };
            if ok == 0 {
                return Ok(0); // client closed: read as EOF
            }
            Ok(n as usize)
        }
    }
    impl Write for Pipe {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let mut n = 0u32;
            let ok = unsafe { WriteFile(self.0, buf.as_ptr(), buf.len() as u32, &mut n, std::ptr::null_mut()) };
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(n as usize)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Create one listening pipe instance.
    pub fn create(path: &Path) -> Result<Pipe, String> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let h = unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                4096,
                4096,
                0,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(format!("CreateNamedPipe {}: error {}", path.display(), unsafe { GetLastError() }));
        }
        Ok(Pipe(h))
    }

    /// Block until a client connects to `pipe`; the kernel-reported client pid.
    pub fn wait(pipe: &Pipe) -> Result<Option<u32>, String> {
        let ok = unsafe { ConnectNamedPipe(pipe.0, std::ptr::null_mut()) };
        if ok == 0 {
            let err = unsafe { GetLastError() };
            if err != ERROR_PIPE_CONNECTED {
                unsafe { DisconnectNamedPipe(pipe.0) };
                return Err(format!("ConnectNamedPipe: error {err}"));
            }
        }
        let mut pid = 0u32;
        let got = unsafe { GetNamedPipeClientProcessId(pipe.0, &mut pid) };
        Ok((got != 0 && pid != 0).then_some(pid))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_binary_peer_gets_a_token_and_unknown_tokens_are_not_owners() {
        let dir = std::env::temp_dir().join(format!("roadie-owner-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        serve(&dir).unwrap();
        let token = connect(&dir).expect("this test process runs the same binary as the 'service'");
        assert_eq!(token.len(), 64);
        // The listener thread registers after writing; give it a moment.
        for _ in 0..50 {
            if is_owner(&token) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(is_owner(&token));
        assert!(!is_owner("0000000000000000000000000000000000000000000000000000000000000000"));
        assert!(!is_owner(""));
        assert!(peer_is_roadie(std::process::id()));
        #[cfg(unix)]
        assert!(!peer_is_roadie(1), "launchd/init is not Roadie");
        #[cfg(windows)]
        assert!(!peer_is_roadie(4), "the System process is not Roadie");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
