//! Process-level end-to-end: the real `roadie` binary, from "Roadie is not
//! running". Every test gets its own temp data dir (`--data-dir`), so the
//! real installation, its service and its login item are never touched;
//! `settings.json` is pre-seeded with background mode **off** so no login
//! item is registered for the temp dir and the service exits when idle.
//!
//! Approvals are deliberately absent: only a real window can click. The
//! in-process suite (`src/e2e.rs`) covers those through the owner channel.
//!
//! ```bash
//! cd src-tauri && cargo build && cargo test --test e2e_process -- --ignored --nocapture --test-threads=1
//! ```

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_roadie");

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("roadie-e2e-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // Plain-app mode: no login item, idle exit.
        std::fs::write(root.join("settings.json"), r#"{ "autoUpdateTools": false, "runInBackground": false }"#).unwrap();
        Sandbox { root }
    }

    /// Run the CLI against this sandbox. Returns (exit code, stdout JSON, stderr).
    fn roadie(&self, args: &[&str]) -> (i32, Value, String) {
        let out = Command::new(BIN)
            .args(args)
            .arg("--data-dir")
            .arg(&self.root)
            .env("ROADIE_IDLE_EXIT_SECS", "4")
            .output()
            .expect("roadie runs");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let json = serde_json::from_str(stdout.trim()).unwrap_or(Value::String(stdout.trim().to_string()));
        (out.status.code().unwrap_or(-1), json, String::from_utf8_lossy(&out.stderr).into_owned())
    }

    fn discovery(&self) -> Option<Value> {
        serde_json::from_str(&std::fs::read_to_string(self.root.join("roadie-api.json")).ok()?).ok()
    }

    fn health(&self) -> Option<Value> {
        let port = self.discovery()?["port"].as_u64()?;
        let resp = reqwest::blocking::Client::builder().timeout(Duration::from_secs(2)).build().ok()?.get(format!("http://127.0.0.1:{port}/v1/health")).send().ok()?;
        resp.json().ok()
    }

    fn service_pid(&self) -> Option<u32> {
        self.health()?["pid"].as_u64().map(|p| p as u32)
    }

    fn stop(&self) {
        let _ = self.roadie(&["service", "stop"]);
        wait(Duration::from_secs(10), || self.health().is_none().then_some(()));
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        self.stop();
        kill_windows_for(&self.root);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn wait<T>(timeout: Duration, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let start = Instant::now();
    loop {
        if let Some(v) = f() {
            return Some(v);
        }
        if start.elapsed() > timeout {
            return None;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, 0) == 0
    }
    #[cfg(not(unix))]
    {
        Command::new("tasklist").args(["/FI", &format!("PID eq {pid}")]).output().map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string())).unwrap_or(false)
    }
}

/// `(pid, command line)` of every running Roadie binary.
fn roadie_processes() -> Vec<(u32, String)> {
    #[cfg(unix)]
    let out = Command::new("ps").args(["-axo", "pid=,command="]).output().unwrap();
    #[cfg(windows)]
    let out = Command::new("powershell")
        .args(["-NoProfile", "-Command", "Get-CimInstance Win32_Process -Filter \"Name='roadie.exe'\" | ForEach-Object { \"$($_.ProcessId) $($_.CommandLine)\" }"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains(BIN))
        .filter_map(|l| {
            let (pid, cmd) = l.trim().split_once(' ')?;
            Some((pid.parse().ok()?, cmd.to_string()))
        })
        .collect()
}

/// Window processes the service opened for this sandbox (`roadie --data-dir <root>`).
fn windows_for(root: &Path) -> Vec<u32> {
    let root = root.display().to_string();
    roadie_processes().into_iter().filter(|(_, c)| c.contains("--data-dir") && c.contains(&root) && !c.contains("--serve")).map(|(p, _)| p).collect()
}

/// Any Roadie window at all (the user's real one included). Tauri's
/// single-instance plugin forwards a second window launch to it, so while
/// one is open no window can appear for another data dir.
#[cfg_attr(not(feature = "window"), allow(dead_code))]
fn any_window_running() -> bool {
    roadie_processes().iter().any(|(_, c)| !c.contains("--serve") && !c.contains("--data-dir"))
}

fn kill_windows_for(root: &Path) {
    for pid in windows_for(root) {
        #[cfg(unix)]
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
        #[cfg(windows)]
        let _ = Command::new("taskkill").args(["/PID", &pid.to_string(), "/F"]).output();
    }
}
#[test]
#[ignore = "spawns the real binary; run with --ignored"]
fn cli_starts_the_service_on_demand_and_stops_it() {
    let sb = Sandbox::new("lifecycle");

    // Roadie is not running: status says so without starting anything.
    let (code, v, _) = sb.roadie(&["service", "status"]);
    assert_eq!(code, 0);
    assert_eq!(v["running"], false, "{v}");
    assert!(sb.discovery().is_none(), "no discovery file before first start");

    // A tool query starts the service on demand and answers.
    let (code, v, err) = sb.roadie(&["tool", "status", "slskd"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(v["name"], "slskd");
    assert_eq!(v["installed"], false);
    let health = sb.health().expect("service answers after the CLI started it");
    assert_eq!(health["role"], "service");
    assert_eq!(health["windowConnected"], false, "the CLI is not a window");
    let pid = health["pid"].as_u64().unwrap() as u32;
    assert!(pid_alive(pid));
    assert!(sb.root.join("owner.sock").exists() || cfg!(windows), "owner channel is listening");
    assert!(sb.root.join("logs").join("roadie-service.log").is_file());

    // Exit codes: a cli tool cannot start (1); nonsense is usage (3).
    let (code, v, _) = sb.roadie(&["tool", "start", "yt-dlp"]);
    assert_eq!(code, 1);
    assert!(v["error"].as_str().unwrap().contains("command-line tool"), "{v}");
    let (code, _, err) = sb.roadie(&["tool", "dance", "slskd"]);
    assert_eq!(code, 3);
    assert!(err.contains("usage"), "{err}");
    let (code, _, err) = sb.roadie(&["tool", "install", "slskd", "--consumer", "nobody"]);
    assert_eq!(code, 3);
    assert!(err.contains("unknown consumer"), "{err}");

    // A second start is a no-op: same service, same pid.
    let (code, _, _) = sb.roadie(&["--serve"]);
    assert_eq!(code, 0, "a second --serve exits 0 when one already answers");
    assert_eq!(sb.service_pid(), Some(pid));

    // No login item was registered for this data dir.
    let (_, v, _) = sb.roadie(&["service", "status"]);
    assert_eq!(v["loginItem"], false, "{v}");

    // Stop: the process is gone, the API is dark, status reports it.
    let (code, v, _) = sb.roadie(&["service", "stop"]);
    assert_eq!(code, 0);
    assert_eq!(v["stopping"], true);
    wait(Duration::from_secs(10), || (!pid_alive(pid)).then_some(())).expect("service process exits");
    assert!(sb.health().is_none());
    let (_, v, _) = sb.roadie(&["service", "status"]);
    assert_eq!(v["running"], false);

    // Stopping twice is fine.
    let (code, v, _) = sb.roadie(&["service", "stop"]);
    assert_eq!(code, 0);
    assert_eq!(v["stopping"], false);
}

#[test]
#[ignore = "spawns the real binary; run with --ignored"]
fn plain_app_mode_exits_when_idle_with_no_window() {
    let sb = Sandbox::new("idle");
    let (code, _, err) = sb.roadie(&["tool", "list"]);
    assert_eq!(code, 0, "{err}");
    let pid = sb.service_pid().expect("running");
    // ROADIE_IDLE_EXIT_SECS=4 and no window, no calls: it should leave.
    wait(Duration::from_secs(30), || (!pid_alive(pid)).then_some(())).expect("service exits when idle in plain-app mode");
    assert!(sb.health().is_none());
    // A pending request keeps it alive — but background mode is off, so the
    // next CLI call simply starts a fresh one.
    let (code, v, _) = sb.roadie(&["tool", "status", "slskd"]);
    assert_eq!(code, 0);
    assert_eq!(v["installed"], false);
    assert_ne!(sb.service_pid(), Some(pid), "a new service");
}

#[test]
#[cfg(feature = "window")]
#[ignore = "spawns the real binary and opens a window; run with --ignored"]
fn a_request_with_no_window_opens_one() {
    let sb = Sandbox::new("window");
    let (code, v, err) = sb.roadie(&["--as", "Viboplr (e2e)", "tool", "install", "slskd", "--consumer", "viboplr", "--set", "soulseekUsername=e2e"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(v["status"], "pending", "{v}");
    let id = v["requestId"].as_str().unwrap().to_string();

    // The service opened a window for this data dir.
    let log = || std::fs::read_to_string(sb.root.join("logs").join("roadie-service.log")).unwrap_or_default();
    wait(Duration::from_secs(20), || log().contains("opened the window for a pending request").then_some(())).expect("the service opened a window");
    if any_window_running() {
        // The user's own window is open: single-instance forwarded our
        // launch to it, so no window for this data dir can exist. The rest
        // of this test needs a closed Roadie; say so and stop here.
        eprintln!("NOTE: a Roadie window is already open; single-instance forwards new launches to it, so the owner-connection half of this test was skipped. Close Roadie and rerun for full coverage.");
        return;
    }
    let pids = wait(Duration::from_secs(20), || {
        let w = windows_for(&sb.root);
        (!w.is_empty()).then_some(w)
    })
    .expect("a window process appears");
    assert_eq!(pids.len(), 1, "exactly one window: {pids:?}");
    wait(Duration::from_secs(20), || (sb.health()?["windowConnected"] == true).then_some(())).expect("the window connects to the owner channel");

    // Nothing installed; the request waits for the click we will not make.
    let (_, r, _) = sb.roadie(&["request", &id]);
    assert_eq!(r["status"], "pending");
    assert_eq!(r["requestedBy"], "Viboplr (e2e)");
    assert_eq!(r["consumer"], "viboplr");
    let (_, t, _) = sb.roadie(&["tool", "status", "slskd"]);
    assert_eq!(t["installed"], false);

    // A second request while the window is open does not spawn another.
    let (_, v2, _) = sb.roadie(&["tool", "install", "slskd"]);
    assert_eq!(v2["status"], "pending");
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(windows_for(&sb.root).len(), 1);

    // Close the window: with background mode off the service follows it.
    let pid = sb.service_pid().unwrap();
    kill_windows_for(&sb.root);
    wait(Duration::from_secs(10), || windows_for(&sb.root).is_empty().then_some(())).expect("window gone");
    // Live requests keep it alive for a bit, but the window's disconnect
    // with background mode off is a stop after a short grace.
    wait(Duration::from_secs(20), || (!pid_alive(pid)).then_some(())).expect("service exits after its window closed in plain-app mode");
}
