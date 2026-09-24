//! `roadie --serve [--data-dir <dir>]`: the background service. No window,
//! no Tauri. It owns everything that must keep working when no window is
//! open: the local API, the request queue, the owner channel, the startup
//! reconcile (which starts the daemons marked "start at login") and the
//! daily update pass.
//!
//! The window is a client of this process. It starts the service if none
//! answers, replaces it when the binary changed (`buildId`), connects to the
//! owner channel, and mirrors the event log into the webview.

use crate::{actions, api, events, owner, paths, recipe, requests, tools};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const SERVICE_LOG: &str = "roadie-service.log";

static SHUTDOWN: OnceLock<tokio::sync::Notify> = OnceLock::new();
static STOPPING: AtomicBool = AtomicBool::new(false);

fn shutdown_signal() -> &'static tokio::sync::Notify {
    SHUTDOWN.get_or_init(tokio::sync::Notify::new)
}

/// Idle exit, plain-app mode: with "run in the background" off, the service
/// leaves after this long without a window, an API call or a live request.
/// `ROADIE_IDLE_EXIT_SECS` overrides it (tests use a few seconds).
pub fn idle_exit() -> Duration {
    std::env::var("ROADIE_IDLE_EXIT_SECS").ok().and_then(|s| s.parse().ok()).map(Duration::from_secs).unwrap_or(Duration::from_secs(3 * 60))
}

static LAST_ACTIVITY: AtomicU64 = AtomicU64::new(0);

/// Every API request calls this; the idle timer counts from the last one.
pub fn touch() {
    LAST_ACTIVITY.store(paths::now_secs(), Ordering::Relaxed);
}

fn idle_for() -> Duration {
    Duration::from_secs(paths::now_secs().saturating_sub(LAST_ACTIVITY.load(Ordering::Relaxed)))
}

/// A request or a deep link needs the user and no window is connected:
/// open one. Debounced so a burst of requests spawns one window.
pub fn open_window_if_needed() {
    if owner::owners_connected() > 0 {
        return;
    }
    static LAST_OPEN: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    let mut last = LAST_OPEN.get_or_init(|| Mutex::new(None)).lock().unwrap();
    if last.is_some_and(|t| t.elapsed() < Duration::from_secs(15)) {
        return;
    }
    *last = Some(Instant::now());
    match open_window() {
        Ok(()) => log::info!("opened the window for a pending request"),
        Err(e) => log::warn!("could not open the window: {e}"),
    }
}

/// Launch the window process. Inside a macOS bundle, hand the bundle to
/// `open` so LaunchServices treats it as the app; elsewhere run the
/// executable with no arguments (that *is* the window).
pub fn open_window() -> Result<(), String> {
    let exe = tools::autostart::launcher_exe()?;
    let root = paths::data_root()?;
    // A window on a non-default data dir must be told so, or it would
    // connect to the default service instead of this one.
    let args: Vec<String> = if root == paths::default_data_root() { vec![] } else { vec!["--data-dir".into(), root.to_string_lossy().into_owned()] };
    #[cfg(target_os = "macos")]
    {
        let s = exe.to_string_lossy();
        if let Some(i) = s.find(".app/Contents/MacOS/") {
            let bundle = &s[..i + 4];
            let mut cmd = std::process::Command::new("/usr/bin/open");
            cmd.arg(bundle);
            if !args.is_empty() {
                cmd.arg("--args").args(&args);
            }
            let ok = cmd.status().map_err(|e| format!("open {bundle}: {e}"))?.success();
            return if ok { Ok(()) } else { Err(format!("open {bundle} failed")) };
        }
    }
    let logs = root.join("logs");
    let plan = tools::process::SpawnPlan { exe, args, env: Default::default(), cwd: None, log: logs.join("roadie-window.log"), append_log: true };
    tools::process::spawn_detached(&plan).map(|_| ())
}

/// Owner route: stop the service (the window does this before starting a
/// newer binary). Daemons are untouched — they are independent processes.
pub fn request_shutdown() {
    STOPPING.store(true, Ordering::SeqCst);
    shutdown_signal().notify_waiters();
    shutdown_signal().notify_one();
}

/// Something that changes whenever the executable does, so a window built
/// a minute ago notices a stale service from the previous build even when
/// the version string is the same (dev builds, reinstalls).
pub fn build_id() -> String {
    std::env::current_exe()
        .and_then(|p| std::fs::metadata(p))
        .map(|m| {
            let mtime = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
            format!("{}-{mtime}-{}", env!("CARGO_PKG_VERSION"), m.len())
        })
        .unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string())
}

pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

pub fn trusted_recipes() -> Vec<recipe::Recipe> {
    recipe::store::list().into_iter().filter(|s| s.trusted()).map(|s| s.recipe).collect()
}

/// Run the service until asked to stop. Returns the process exit code.
pub fn run(data_root: PathBuf) -> i32 {
    if let Err(e) = std::fs::create_dir_all(&data_root) {
        eprintln!("create {}: {e}", data_root.display());
        return 2;
    }
    paths::init(data_root.clone());
    recipe::store::load_all();

    // Already running (same data dir)? Then this launch is a no-op.
    if let Some(health) = api::probe(&data_root) {
        if health.get("app").and_then(|a| a.as_str()) == Some("roadie") {
            log::info!("a Roadie service already answers on port {}; exiting", health.get("port").and_then(|p| p.as_u64()).unwrap_or(0));
            return 0;
        }
    }

    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            log::error!("tokio runtime: {e}");
            return 2;
        }
    };
    let code = rt.block_on(async {
        let port = match api::start(&data_root, version(), build_id()) {
            Ok(p) => p,
            Err(e) => {
                log::error!("local API failed to start: {e}");
                return 2;
            }
        };
        if let Err(e) = owner::serve(&data_root) {
            log::error!("owner channel failed to start: {e}");
            return 2;
        }
        log::info!("Roadie service {} (build {}) on 127.0.0.1:{port}, data in {}", version(), build_id(), data_root.display());

        // The service's own login item follows the setting.
        if actions::load_settings().run_in_background {
            if let Err(e) = tools::autostart::enable_service(&data_root) {
                log::warn!("could not register the login item: {e}");
            }
        } else {
            let _ = tools::autostart::disable_service(&data_root);
        }
        // Plain-app mode: when the last window disconnects and the setting
        // is off, leave after a short grace (a relaunching window reconnects
        // within it). Checked at fire time so flipping the toggle counts.
        owner::on_last_owner_gone(|| {
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_secs(5));
                if owner::owners_connected() == 0 && !actions::load_settings().run_in_background {
                    log::info!("window closed and background mode is off; stopping");
                    request_shutdown();
                }
            });
        });

        touch();
        // Reconcile shortly after launch, then the daily update pass.
        std::thread::Builder::new()
            .name("reconcile".into())
            .spawn(move || {
                std::thread::sleep(Duration::from_secs(2));
                tools::reconcile(&trusted_recipes(), &events::emit);
                loop {
                    std::thread::sleep(Duration::from_secs(30));
                    if STOPPING.load(Ordering::SeqCst) {
                        return;
                    }
                    if actions::auto_update_enabled() {
                        tools::auto_update(&trusted_recipes(), &events::emit);
                    }
                    for _ in 0..(24 * 60 * 2) {
                        std::thread::sleep(Duration::from_secs(30));
                        if STOPPING.load(Ordering::SeqCst) {
                            return;
                        }
                    }
                }
            })
            .ok();
        // Plain-app mode, no window ever opened (a CLI or MCP client started
        // us): leave once nothing has happened for a while and no request
        // is pending or running.
        std::thread::Builder::new()
            .name("idle-exit".into())
            .spawn(|| loop {
                std::thread::sleep(Duration::from_secs(15).min(idle_exit() / 2).max(Duration::from_secs(1)));
                if STOPPING.load(Ordering::SeqCst) {
                    return;
                }
                if actions::load_settings().run_in_background || owner::owners_connected() > 0 {
                    continue;
                }
                let live = requests::live().len();
                if live == 0 && idle_for() >= idle_exit() {
                    log::info!("idle for {}s with no window and background mode off; stopping", idle_for().as_secs());
                    request_shutdown();
                    return;
                }
            })
            .ok();

        shutdown_signal().notified().await;
        log::info!("service stopping");
        0
    });
    // Leave the discovery file: a client reads `pid` and sees it is gone.
    code
}

/// The window's side: make sure a service with *this* build answers, and
/// return its port. Starts one if none answers; replaces a stale one.
pub fn ensure_running(data_root: &std::path::Path) -> Result<u16, String> {
    let wanted = build_id();
    if let Some(h) = api::probe(data_root) {
        let same = h.get("buildId").and_then(|b| b.as_str()) == Some(wanted.as_str());
        let port = h.get("port").and_then(|p| p.as_u64()).map(|p| p as u16);
        if let (true, Some(port)) = (same, port) {
            return Ok(port);
        }
        // Stale build: ask it to stop (owner route needs a token we may not
        // have yet; the shutdown route accepts the bearer token for exactly
        // this handoff), then wait for the port to free up.
        log::info!("service build {:?} differs from ours {wanted}; replacing it", h.get("buildId"));
        let _ = api::bearer_post(data_root, "/v1/shutdown");
        for _ in 0..50 {
            if api::probe(data_root).is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    spawn_service(data_root)?;
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(100));
        if let Some(h) = api::probe(data_root) {
            if let Some(port) = h.get("port").and_then(|p| p.as_u64()) {
                return Ok(port as u16);
            }
        }
    }
    Err(format!("the Roadie service did not answer within 10 s; see {}", data_root.join("logs").join(SERVICE_LOG).display()))
}

/// Start `roadie --serve` detached, logging to `logs/roadie-service.log`.
pub fn spawn_service(data_root: &std::path::Path) -> Result<u32, String> {
    let exe = tools::autostart::launcher_exe()?;
    let logs = data_root.join("logs");
    let plan = tools::process::SpawnPlan {
        exe,
        args: service_args(data_root),
        env: Default::default(),
        cwd: Some(data_root.to_path_buf()),
        log: logs.join(SERVICE_LOG),
        append_log: true,
    };
    tools::process::spawn_detached(&plan)
}

pub fn service_args(data_root: &std::path::Path) -> Vec<String> {
    vec!["--serve".to_string(), "--data-dir".to_string(), data_root.to_string_lossy().into_owned()]
}
