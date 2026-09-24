//! The recipe interpreter: everything Roadie does *to* a tool, driven only by
//! its recipe and its `state.json`.
//!
//! Rules that hold everywhere in here:
//! - A daemon is **independent**. Nothing stops it when Roadie exits; the
//!   user stops it (Stop button / API) or the OS does at logout.
//! - An update or config change **never restarts a busy daemon**. It is
//!   staged and applied when the daemon is idle or stopped.
//! - Every mutation goes through the per-tool lock.
//! - Nothing here decides *whether* something may be installed — that is a
//!   user click, gated by the command layer / request queue.

pub mod autostart;
pub mod install;
pub mod process;
pub mod state;
#[cfg(test)]
mod probe;

use crate::consent;
use crate::paths::{self, ToolPaths};
use crate::recipe::template::{self, Ctx};
use crate::recipe::{self, httpsteps, jsonq, ConnectionPolicy, Kind, Platform, Recipe};
use install::{ApplyOutcome, DeferReason, LatestCache};
use serde_json::{Map, Value};
use state::ToolState;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub fn latest_cache() -> &'static LatestCache {
    static C: OnceLock<LatestCache> = OnceLock::new();
    C.get_or_init(LatestCache::default)
}

fn lock(name: &str) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    LOCKS.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap().entry(name.to_string()).or_default().clone()
}

/// Conflict / failure recorded by the last start attempt, shown until the
/// next successful start or an explicit stop.
fn last_errors() -> &'static Mutex<HashMap<String, (String, String)>> {
    static E: OnceLock<Mutex<HashMap<String, (String, String)>>> = OnceLock::new();
    E.get_or_init(|| Mutex::new(HashMap::new()))
}

// --- Status ---

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolStatus {
    pub name: String,
    pub display_name: String,
    pub author: String,
    pub revision: u32,
    pub platforms: Vec<String>,
    pub summary: String,
    pub kind: Kind,
    pub notes: Option<String>,
    pub homepage: Option<String>,
    pub supported: bool,
    pub installed: bool,
    pub version: Option<String>,
    /// Cache-only; populated by the daily tick or an explicit check.
    pub latest: Option<String>,
    pub update_available: bool,
    pub update_staged: Option<String>,
    pub update_deferred_reason: Option<DeferReason>,
    pub restart_pending: bool,
    pub running: bool,
    pub healthy: bool,
    pub starting: bool,
    pub pid: Option<u32>,
    /// A recipe `startFailures` code, `foreignInstanceOnPort`, or `startFailed`.
    pub conflict: Option<String>,
    pub conflict_detail: Option<String>,
    pub autostart: bool,
    /// Daemon base URL (no credentials).
    pub url: Option<String>,
    /// Cli tools: the stable path consumers should run.
    pub bin_path: Option<String>,
    pub connection_policy: ConnectionPolicy,
    pub approved_consumers: Vec<String>,
    /// Non-secret config values plus `has_<key>` for secret fields.
    pub config: Map<String, Value>,
    /// `health.extract` `details.*` and `logExtract` values.
    pub details: Map<String, Value>,
    pub reported_version: Option<String>,
    pub health_detail: Option<String>,
    pub logs_dir: String,
}

struct Liveness {
    pid: Option<u32>,
    running: bool,
    healthy: bool,
    starting: bool,
    conflict: Option<(String, String)>,
    reported_version: Option<String>,
    details: Map<String, Value>,
    health_detail: Option<String>,
}

impl Liveness {
    fn none() -> Self {
        Liveness { pid: None, running: false, healthy: false, starting: false, conflict: None, reported_version: None, details: Map::new(), health_detail: None }
    }
}

enum Health {
    Ok { version: Option<String>, details: Map<String, Value> },
    Unreachable(String),
    Unauthorized,
    Other(String),
}

fn build_ctx(recipe: &Recipe, st: &ToolState, p: &ToolPaths) -> Result<Ctx, String> {
    let mut ctx = Ctx::empty(Platform::current());
    ctx.home = paths::home_dir().to_string_lossy().into_owned();
    ctx.data = p.data.to_string_lossy().into_owned();
    ctx.bin = paths::bin_dir()?.to_string_lossy().into_owned();
    ctx.version = st.installed_version.clone().unwrap_or_default();
    ctx.ports = st.ports.clone();
    // Before install the state has no ports yet; the recipe defaults are
    // what the connection URL would be, which is all status needs.
    for (name, def) in &recipe.ports {
        ctx.ports.entry(name.clone()).or_insert(def.default);
    }
    ctx.config = st.config.clone();
    ctx.secrets = st.secrets.clone();
    ctx.consumers = consent::consumers_for(&recipe.name);
    if let Some(c) = &recipe.connection {
        ctx.connection_url = Some(template::expand_string(&c.url, &ctx)?);
    }
    Ok(ctx)
}

fn probe_health(recipe: &Recipe, ctx: &Ctx) -> Health {
    let Some(h) = &recipe.health else { return Health::Other("no health check".into()) };
    match httpsteps::send(&h.request, ctx) {
        Err(httpsteps::SendError::Unreachable(m)) => Health::Unreachable(m),
        Err(httpsteps::SendError::Other(m)) => Health::Other(m),
        Ok(reply) if h.unauthorized_status.contains(&reply.status) => Health::Unauthorized,
        Ok(reply) if (200..300).contains(&reply.status) => {
            let mut version = None;
            let mut details = Map::new();
            for (k, path) in &h.extract {
                let v = jsonq::first(&reply.json, path).cloned();
                if k == "version" {
                    version = v.and_then(|v| match v {
                        Value::String(s) => Some(s),
                        other => Some(other.to_string()),
                    });
                } else if let Some(name) = k.strip_prefix("details.") {
                    details.insert(name.to_string(), v.unwrap_or(Value::Null));
                }
            }
            Health::Ok { version, details }
        }
        Ok(reply) => Health::Other(format!("HTTP {}", reply.status)),
    }
}

fn answering(recipe: &Recipe, ctx: &Ctx) -> bool {
    !matches!(probe_health(recipe, ctx), Health::Unreachable(_))
}

/// Where the daemon stands right now: pid file cross-checked against the
/// binary it runs, plus the recipe's health probe. Cleans a stale pid file.
fn liveness(recipe: &Recipe, _st: &ToolState, p: &ToolPaths, ctx: &Ctx) -> Liveness {
    let mut lv = Liveness::none();
    if recipe.kind == Kind::Cli {
        return lv;
    }
    let health = probe_health(recipe, ctx);
    let grace = recipe.run.as_ref().map(|r| r.startup_grace_sec).unwrap_or(30);
    let port_desc = || {
        ctx.connection_url.clone().unwrap_or_else(|| "its port".into())
    };
    match process::read_pid_file(&p.data) {
        Some(pf) if process::is_ours(pf.pid, &p.versions) => {
            lv.pid = Some(pf.pid);
            lv.running = true;
            match health {
                Health::Ok { version, details } => {
                    lv.healthy = true;
                    lv.reported_version = version;
                    lv.details = details;
                }
                Health::Unreachable(detail) => {
                    lv.starting = paths::now_secs().saturating_sub(pf.started_at) < grace;
                    lv.health_detail = Some(detail);
                }
                Health::Unauthorized => {
                    lv.conflict = Some(("foreignInstanceOnPort".into(), format!("something else answers on {}", port_desc())));
                }
                Health::Other(detail) => lv.health_detail = Some(detail),
            }
        }
        other => {
            if other.is_some() {
                process::remove_pid_file(&p.data);
            }
            match health {
                Health::Ok { version, details } => {
                    // An instance we did not spawn that still accepts our key.
                    lv.running = true;
                    lv.healthy = true;
                    lv.reported_version = version;
                    lv.details = details;
                }
                Health::Unauthorized => {
                    lv.conflict = Some(("foreignInstanceOnPort".into(), format!("another {} answers on {}", recipe.display_name, port_desc())));
                }
                _ => {}
            }
        }
    }
    for (k, re) in &recipe.log_extract {
        if let Ok(rx) = regex::Regex::new(re) {
            let tail = process::log_tail(&p.logs, 200);
            if let Some(v) = rx.captures(&tail).and_then(|c| c.get(1)) {
                lv.details.insert(k.clone(), Value::String(v.as_str().to_string()));
            }
        }
    }
    if lv.conflict.is_none() && !lv.running {
        lv.conflict = last_errors().lock().unwrap().get(&recipe.name).cloned();
    }
    lv
}

/// Whether the daemon may be restarted right now.
fn restart_allowed(recipe: &Recipe, ctx: &Ctx) -> Result<(), DeferReason> {
    let Some(b) = &recipe.busy else {
        return match probe_health(recipe, ctx) {
            Health::Unreachable(_) => Err(DeferReason::Unreachable),
            _ => Ok(()),
        };
    };
    let rx = regex::Regex::new(&b.busy_if.regex).map_err(|_| DeferReason::Busy)?;
    for req in &b.requests {
        match httpsteps::send(req, ctx) {
            Err(httpsteps::SendError::Unreachable(_)) => return Err(DeferReason::Unreachable),
            Err(_) => return Err(DeferReason::Busy),
            Ok(reply) if !(200..300).contains(&reply.status) => return Err(DeferReason::Busy),
            Ok(reply) => {
                let hits = jsonq::query(&reply.json, &b.busy_if.path).map_err(|_| DeferReason::Busy)?;
                if hits.iter().any(|v| match v {
                    Value::String(s) => rx.is_match(s),
                    other => rx.is_match(&other.to_string()),
                }) {
                    return Err(DeferReason::Busy);
                }
            }
        }
    }
    Ok(())
}

pub fn status(recipe: &Recipe) -> ToolStatus {
    let platform = Platform::current();
    let supported = recipe.supported_on(&platform);
    let Ok(p) = paths::tool_paths(&recipe.name) else { return unavailable(recipe, supported) };
    // Defaults filled in memory only: an uninstalled tool shows what it
    // would use, and nothing is written before the user's Install click.
    let st = state::preview(recipe, &p.data, &platform);
    let version = install::current_version(recipe, &p);
    let installed = version.is_some();
    let latest = latest_cache().get(&recipe.name).flatten();
    let update_available = match (&version, &latest) {
        (Some(v), Some(l)) => {
            if l.floating {
                install::read_stamp(&p, v).map(|s| Some(s.archive_sha256) != st.archive_sha256).unwrap_or(false)
            } else {
                install::version_lt(v, &l.version)
            }
        }
        _ => false,
    };
    let staged = install::staged_upgrade(recipe, &p);
    let ctx = build_ctx(recipe, &st, &p).ok();
    let lv = match (&ctx, installed) {
        (Some(ctx), true) => liveness(recipe, &st, &p, ctx),
        _ => Liveness::none(),
    };
    let deferred = match (&ctx, (staged.is_some() || st.restart_pending) && lv.running) {
        (Some(ctx), true) => Some(restart_allowed(recipe, ctx).err().unwrap_or(DeferReason::RestartNotAllowed)),
        _ => None,
    };
    let bin_path = (recipe.kind == Kind::Cli && installed)
        .then(|| shim_path(recipe).ok().map(|s| s.to_string_lossy().into_owned()))
        .flatten();
    ToolStatus {
        name: recipe.name.clone(),
        display_name: recipe.display_name.clone(),
        author: recipe.author.clone(),
        revision: recipe.revision,
        platforms: recipe.platforms.clone(),
        summary: recipe.summary.clone(),
        kind: recipe.kind,
        notes: recipe.notes.clone(),
        homepage: recipe.homepage.clone(),
        supported,
        installed,
        version,
        latest: latest.map(|l| l.version),
        update_available,
        update_staged: staged,
        update_deferred_reason: deferred,
        restart_pending: st.restart_pending,
        running: lv.running,
        healthy: lv.healthy,
        starting: lv.starting,
        pid: lv.pid,
        conflict: lv.conflict.as_ref().map(|c| c.0.clone()),
        conflict_detail: lv.conflict.as_ref().map(|c| c.1.clone()),
        autostart: recipe.kind == Kind::Daemon && st.autostart,
        url: ctx.as_ref().and_then(|c| c.connection_url.clone()),
        bin_path,
        connection_policy: recipe.connection.as_ref().map(|c| c.policy).unwrap_or(ConnectionPolicy::None),
        approved_consumers: consent::consumers_for(&recipe.name).into_iter().map(|c| c.id).collect(),
        config: state::public_config(recipe, &st),
        details: lv.details,
        reported_version: lv.reported_version,
        health_detail: lv.health_detail,
        logs_dir: p.logs.to_string_lossy().into_owned(),
    }
}

fn unavailable(recipe: &Recipe, supported: bool) -> ToolStatus {
    ToolStatus {
        name: recipe.name.clone(),
        display_name: recipe.display_name.clone(),
        author: recipe.author.clone(),
        revision: recipe.revision,
        platforms: recipe.platforms.clone(),
        summary: recipe.summary.clone(),
        kind: recipe.kind,
        notes: recipe.notes.clone(),
        homepage: recipe.homepage.clone(),
        supported,
        installed: false,
        version: None,
        latest: None,
        update_available: false,
        update_staged: None,
        update_deferred_reason: None,
        restart_pending: false,
        running: false,
        healthy: false,
        starting: false,
        pid: None,
        conflict: None,
        conflict_detail: None,
        autostart: false,
        url: None,
        bin_path: None,
        connection_policy: ConnectionPolicy::None,
        approved_consumers: vec![],
        config: Map::new(),
        details: Map::new(),
        reported_version: None,
        health_detail: None,
        logs_dir: String::new(),
    }
}

// --- Rendering ---

/// A rendered config file, before or instead of writing it (dry runs show
/// these to the user and to assistants).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderedFile {
    pub path: String,
    pub secret: bool,
    pub contents: String,
}

pub fn render_files(recipe: &Recipe, ctx: &Ctx) -> Result<Vec<RenderedFile>, String> {
    recipe
        .files
        .iter()
        .map(|f| {
            let path = template::expand_string(&f.path, ctx)?;
            let content = template::expand_value(&f.content, ctx)?;
            let contents = recipe::emit::render(f.format, &content)?;
            Ok(RenderedFile { path, secret: f.secret, contents })
        })
        .collect()
}

/// Create the recipe's directories and (re)write its config files.
fn write_files(recipe: &Recipe, ctx: &Ctx) -> Result<(), String> {
    for f in recipe.config.iter().filter(|f| f.create_dir) {
        if let Some(Value::String(dir)) = ctx.config.get(&f.key) {
            std::fs::create_dir_all(dir).map_err(|e| format!("create {} ({dir}): {e}", f.label))?;
        }
    }
    for d in &recipe.create_dirs {
        let dir = template::expand_string(d, ctx)?;
        std::fs::create_dir_all(&dir).map_err(|e| format!("create {dir}: {e}"))?;
    }
    for rf in render_files(recipe, ctx)? {
        paths::write_atomic(&PathBuf::from(&rf.path), rf.contents.as_bytes(), rf.secret)?;
    }
    Ok(())
}

/// Keep a port that is free or already ours; otherwise the first free one
/// after the default. A *foreign* instance on the port is left in place and
/// reported by liveness.
fn choose_ports(recipe: &Recipe, st: &mut ToolState, p: &ToolPaths) -> Result<(), String> {
    for (name, def) in recipe.ports.iter().filter(|(_, d)| d.pick) {
        let port = *st.ports.get(name).unwrap_or(&def.default);
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            st.ports.insert(name.clone(), port);
            continue;
        }
        let ctx = build_ctx(recipe, st, p)?;
        match probe_health(recipe, &ctx) {
            Health::Ok { .. } | Health::Unauthorized => {
                st.ports.insert(name.clone(), port);
            }
            _ => {
                let picked = (def.default + 1..=def.default + 10).find(|q| std::net::TcpListener::bind(("127.0.0.1", *q)).is_ok());
                match picked {
                    Some(q) => {
                        st.ports.insert(name.clone(), q);
                    }
                    None => return Err(format!("no free port between {} and {}", def.default, def.default + 10)),
                }
            }
        }
    }
    Ok(())
}

// --- Cli shims ---

pub fn shim_path(recipe: &Recipe) -> Result<PathBuf, String> {
    Ok(paths::bin_dir()?.join(install::exe_name(&recipe.name)))
}

/// `bin/<name>` → the current version's main binary (symlink on unix, a copy
/// on Windows where symlinks need privileges).
fn refresh_shims(recipe: &Recipe, p: &ToolPaths, version: &str) -> Result<(), String> {
    let bin = paths::bin_dir()?;
    std::fs::create_dir_all(&bin).map_err(|e| format!("create {}: {e}", bin.display()))?;
    for (i, rel) in recipe.binaries().iter().enumerate() {
        let target = install::version_dir(p, version).join(install::exe_name(rel));
        let shim_name = if i == 0 {
            install::exe_name(&recipe.name)
        } else {
            install::exe_name(std::path::Path::new(rel).file_name().and_then(|n| n.to_str()).unwrap_or(rel))
        };
        let shim = bin.join(shim_name);
        let _ = std::fs::remove_file(&shim);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &shim).map_err(|e| format!("link {}: {e}", shim.display()))?;
        #[cfg(windows)]
        {
            let mut last = None;
            for _ in 0..5 {
                match std::fs::copy(&target, &shim) {
                    Ok(_) => {
                        last = None;
                        break;
                    }
                    Err(e) => {
                        last = Some(e);
                        std::thread::sleep(Duration::from_millis(300));
                    }
                }
            }
            if let Some(e) = last {
                return Err(format!("copy {}: {e}", shim.display()));
            }
        }
    }
    Ok(())
}

fn remove_shims(recipe: &Recipe) {
    if let Ok(bin) = paths::bin_dir() {
        for (i, rel) in recipe.binaries().iter().enumerate() {
            let name = if i == 0 {
                install::exe_name(&recipe.name)
            } else {
                install::exe_name(std::path::Path::new(rel).file_name().and_then(|n| n.to_str()).unwrap_or(rel))
            };
            let _ = std::fs::remove_file(bin.join(name));
        }
    }
}

// --- Actions ---

pub type Progress<'a> = &'a mut dyn FnMut(install::Phase, u64, Option<u64>);

/// Install (nothing current) or fetch-and-stage the latest release. Applies
/// immediately when the daemon is stopped or idle; otherwise stays staged.
pub fn install(recipe: &Recipe, progress: Progress) -> Result<ToolStatus, String> {
    let guard = lock(&recipe.name);
    let _g = guard.lock().unwrap();
    let platform = Platform::current();
    if !recipe.supported_on(&platform) {
        return Err(format!("{} has no build for this computer", recipe.display_name));
    }
    let p = paths::tool_paths(&recipe.name)?;
    std::fs::create_dir_all(&p.data).map_err(|e| format!("create {}: {e}", p.data.display()))?;
    paths::restrict_dir(&p.data);
    let mut st = state::load_or_init(recipe, &p.data, &platform)?;
    let resolved = install::latest(recipe, &platform, latest_cache())?;
    let current = install::current_version(recipe, &p);
    if current.as_deref() == Some(resolved.version.as_str()) && !resolved.floating {
        return Ok(status(recipe));
    }
    if !install::staged_versions(recipe, &p).iter().any(|v| v == &resolved.version) || resolved.floating {
        let sha = install::download_and_stage(recipe, &p, &resolved, progress)?;
        st.archive_sha256 = Some(sha);
    }
    if current.is_none() || resolved.floating {
        if recipe.kind == Kind::Daemon {
            choose_ports(recipe, &mut st, &p)?;
        }
        install::set_current(&p, &resolved.version)?;
        st.installed_version = Some(resolved.version.clone());
        let ctx = build_ctx(recipe, &st, &p)?;
        if recipe.kind == Kind::Daemon {
            write_files(recipe, &ctx)?;
        } else {
            refresh_shims(recipe, &p, &resolved.version)?;
        }
        state::save(&p.data, &st)?;
        install::prune_versions(&p, &[resolved.version.as_str()]);
    } else {
        state::save(&p.data, &st)?;
        apply_pending(recipe, &mut st, &p, true)?;
    }
    Ok(status(recipe))
}

/// Force a release lookup (the one networked status call).
pub fn check_updates(recipe: &Recipe) -> Result<ToolStatus, String> {
    let r = install::resolve_latest(recipe, &Platform::current());
    latest_cache().set(&recipe.name, r.as_ref().ok().cloned());
    r?;
    Ok(status(recipe))
}

/// Make a staged version and/or a pending config restart live. Never
/// restarts a busy daemon.
fn apply_pending(recipe: &Recipe, st: &mut ToolState, p: &ToolPaths, allow_restart: bool) -> Result<ApplyOutcome, String> {
    let staged = install::staged_upgrade(recipe, p);
    if staged.is_none() && !st.restart_pending {
        return Ok(ApplyOutcome::Nothing);
    }
    let from = install::current_version(recipe, p);
    let ctx = build_ctx(recipe, st, p)?;
    let lv = liveness(recipe, st, p, &ctx);
    if lv.running {
        if !allow_restart {
            return Ok(ApplyOutcome::Deferred { reason: DeferReason::RestartNotAllowed });
        }
        if let Err(reason) = restart_allowed(recipe, &ctx) {
            return Ok(ApplyOutcome::Deferred { reason });
        }
        stop_process(recipe, &ctx, lv.pid)?;
        process::remove_pid_file(&p.data);
    }
    if let Some(v) = &staged {
        install::set_current(p, v)?;
        st.installed_version = Some(v.clone());
        let mut keep: Vec<&str> = vec![v.as_str()];
        if let Some(f) = from.as_deref() {
            keep.push(f);
        }
        install::prune_versions(p, &keep);
        if recipe.kind == Kind::Cli {
            refresh_shims(recipe, p, v)?;
        }
    }
    st.restart_pending = false;
    state::save(&p.data, st)?;
    if lv.running {
        start_locked(recipe, st, p, "roadie")?;
    }
    Ok(match staged {
        Some(to) => ApplyOutcome::Applied { from, to },
        None => ApplyOutcome::Applied { from: from.clone(), to: from.unwrap_or_default() },
    })
}

pub fn start(recipe: &Recipe, started_by: &str) -> Result<ToolStatus, String> {
    if recipe.kind != Kind::Daemon {
        return Err(format!("{} is a command-line tool; there is nothing to start", recipe.display_name));
    }
    let guard = lock(&recipe.name);
    let _g = guard.lock().unwrap();
    let p = paths::tool_paths(&recipe.name)?;
    let mut st = state::load_or_init(recipe, &p.data, &Platform::current())?;
    let ctx = build_ctx(recipe, &st, &p)?;
    if !liveness(recipe, &st, &p, &ctx).running {
        if let Some(v) = install::staged_upgrade(recipe, &p) {
            install::set_current(&p, &v)?;
            st.installed_version = Some(v.clone());
            install::prune_versions(&p, &[v.as_str()]);
        }
        st.restart_pending = false;
    }
    start_locked(recipe, &mut st, &p, started_by)?;
    Ok(status(recipe))
}

fn start_locked(recipe: &Recipe, st: &mut ToolState, p: &ToolPaths, started_by: &str) -> Result<(), String> {
    let version = install::current_version(recipe, p).ok_or_else(|| format!("{} is not installed", recipe.display_name))?;
    st.installed_version = Some(version.clone());
    let ctx = build_ctx(recipe, st, p)?;
    let lv = liveness(recipe, st, p, &ctx);
    if lv.running {
        st.user_stopped = false;
        state::save(&p.data, st)?;
        return Ok(());
    }
    if let Some((kind, detail)) = &lv.conflict {
        if kind == "foreignInstanceOnPort" {
            return Err(format!("another {} is already using the port ({detail})", recipe.display_name));
        }
    }
    choose_ports(recipe, st, p)?;
    let ctx = build_ctx(recipe, st, p)?;
    write_files(recipe, &ctx)?;

    let run = recipe.run.as_ref().ok_or("recipe has no run block")?;
    let exe = install::binary_path(recipe, p, &version);
    let plan = process::SpawnPlan {
        exe: exe.clone(),
        args: run.args.iter().map(|a| template::expand_string(a, &ctx)).collect::<Result<_, _>>()?,
        env: run.env.iter().map(|(k, v)| template::expand_string(v, &ctx).map(|v| (k.clone(), v))).collect::<Result<_, _>>()?,
        cwd: run.cwd.as_ref().map(|c| template::expand_string(c, &ctx).map(PathBuf::from)).transpose()?,
        log: p.logs.join(process::STDOUT_LOG),
        append_log: false,
    };
    let pid = process::spawn_detached(&plan)?;
    let started_at = paths::now_secs();
    process::write_pid_file(&p.data, &process::PidFile { pid, version, exe, started_at, started_by: started_by.to_string() })?;
    st.user_stopped = false;
    st.last_start = Some(started_at);
    state::save(&p.data, st)?;

    // Wait briefly for health; a pid that dies before answering is how a
    // singleton refusal or a bad config shows itself.
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if matches!(probe_health(recipe, &ctx), Health::Ok { .. }) {
            last_errors().lock().unwrap().remove(&recipe.name);
            return Ok(());
        }
        if !process::pid_alive(pid) {
            process::remove_pid_file(&p.data);
            let tail = process::log_tail(&p.logs, 40);
            for f in &recipe.start_failures {
                if regex::Regex::new(&f.regex).map(|r| r.is_match(&tail)).unwrap_or(false) {
                    last_errors().lock().unwrap().insert(recipe.name.clone(), (f.code.clone(), f.message.clone()));
                    return Err(f.message.clone());
                }
            }
            last_errors().lock().unwrap().insert(recipe.name.clone(), ("startFailed".into(), tail.clone()));
            return Err(format!("{} exited during startup:\n{tail}", recipe.display_name));
        }
        if Instant::now() >= deadline {
            // Alive, just slow; status reads `starting` until it answers.
            last_errors().lock().unwrap().remove(&recipe.name);
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn stop_process(recipe: &Recipe, ctx: &Ctx, pid: Option<u32>) -> Result<process::StopMethod, String> {
    let grace = Duration::from_secs(recipe.stop.as_ref().map(|s| s.grace_sec).unwrap_or(15));
    let api_steps = recipe.stop.as_ref().map(|s| s.api.as_slice()).unwrap_or(&[]);
    let api = |steps: &[recipe::HttpRequest]| -> Result<(), String> { httpsteps::run_chain(steps, ctx) };
    let api_fn = move || api(api_steps);
    let still = || answering(recipe, ctx);
    process::stop(
        &recipe.display_name,
        pid,
        if api_steps.is_empty() { None } else { Some(&api_fn) },
        &still,
        grace,
    )
}

pub fn stop(recipe: &Recipe) -> Result<ToolStatus, String> {
    let guard = lock(&recipe.name);
    let _g = guard.lock().unwrap();
    let p = paths::tool_paths(&recipe.name)?;
    let mut st = state::load(&p.data);
    let ctx = build_ctx(recipe, &st, &p)?;
    let lv = liveness(recipe, &st, &p, &ctx);
    if lv.running {
        let method = stop_process(recipe, &ctx, lv.pid)?;
        log::info!("stopped {} via {:?}", recipe.name, method);
        process::remove_pid_file(&p.data);
    }
    last_errors().lock().unwrap().remove(&recipe.name);
    st.user_stopped = true;
    state::save(&p.data, &st)?;
    apply_pending(recipe, &mut st, &p, false)?;
    Ok(status(recipe))
}

pub fn restart(recipe: &Recipe) -> Result<ToolStatus, String> {
    stop(recipe)?;
    start(recipe, "roadie")
}

/// The engine-owned install decisions, split off a decisions map (the rest
/// is a config patch). A key the recipe does not offer is an error the API
/// turns into a 422, so a caller learns the recipe's vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallOptions {
    pub start_now: bool,
    pub autostart: bool,
}

pub fn install_options(recipe: &Recipe, values: &mut Map<String, Value>) -> Result<InstallOptions, String> {
    fn take(recipe: &Recipe, values: &mut Map<String, Value>, key: &str, offer: Option<recipe::InstallChoice>) -> Result<bool, String> {
        match (values.remove(key), offer) {
            (None, offer) => Ok(offer.map(|o| o.default).unwrap_or(false)),
            (Some(_), None) => Err(format!(
                "`{key}` is not a choice this recipe offers ({} is {})",
                recipe.display_name,
                if recipe.kind == Kind::Cli { "a command-line tool" } else { "a daemon without that option" }
            )),
            (Some(v), Some(_)) => v.as_bool().ok_or_else(|| format!("`{key}` must be true or false")),
        }
    }
    Ok(InstallOptions {
        start_now: take(recipe, values, "startNow", recipe.start_after_install)?,
        autostart: take(recipe, values, "autostart", recipe.autostart)?,
    })
}

pub fn set_autostart(recipe: &Recipe, enabled: bool) -> Result<ToolStatus, String> {
    if recipe.kind != Kind::Daemon {
        return Err("only daemons start at login".into());
    }
    let guard = lock(&recipe.name);
    let _g = guard.lock().unwrap();
    let p = paths::tool_paths(&recipe.name)?;
    let mut st = state::load_or_init(recipe, &p.data, &Platform::current())?;
    // "Start at login" is a flag the service acts on at its own start; a
    // per-tool login item from an older Roadie is removed when seen.
    if autostart::is_enabled(&recipe.name) {
        if let Err(e) = autostart::disable(&recipe.name) {
            log::warn!("could not remove the old {} login item: {e}", recipe.name);
        }
    }
    st.autostart = enabled;
    if enabled {
        st.user_stopped = false;
    }
    state::save(&p.data, &st)?;
    Ok(status(recipe))
}

/// Apply a config patch; rewrite the files; restart a running daemon when it
/// is idle, otherwise mark the restart pending.
pub fn configure(recipe: &Recipe, patch: &Map<String, Value>) -> Result<ToolStatus, String> {
    let guard = lock(&recipe.name);
    let _g = guard.lock().unwrap();
    let p = paths::tool_paths(&recipe.name)?;
    std::fs::create_dir_all(&p.data).map_err(|e| format!("create {}: {e}", p.data.display()))?;
    let mut st = state::load_or_init(recipe, &p.data, &Platform::current())?;
    state::apply_patch(recipe, &mut st, patch)?;
    let ctx = build_ctx(recipe, &st, &p)?;
    if recipe.kind == Kind::Daemon && install::current_version(recipe, &p).is_some() {
        write_files(recipe, &ctx)?;
        st.restart_pending = true;
        state::save(&p.data, &st)?;
        if !matches!(apply_pending(recipe, &mut st, &p, true)?, ApplyOutcome::Deferred { .. }) {
            st.restart_pending = false;
            state::save(&p.data, &st)?;
        }
    } else {
        state::save(&p.data, &st)?;
    }
    Ok(status(recipe))
}

/// A consumer's grant changed: re-render the config (the key list lives in
/// it) and restart when idle.
pub fn refresh_consumers(recipe: &Recipe) -> Result<ToolStatus, String> {
    configure(recipe, &Map::new())
}

/// Stop, drop the login item, delete the versions and (unless `keep_data`)
/// the data dir. User folders named by config are never touched.
pub fn uninstall(recipe: &Recipe, keep_data: bool) -> Result<(), String> {
    let guard = lock(&recipe.name);
    let _g = guard.lock().unwrap();
    let p = paths::tool_paths(&recipe.name)?;
    let st = state::load(&p.data);
    if recipe.kind == Kind::Daemon && install::current_version(recipe, &p).is_some() {
        if let Ok(ctx) = build_ctx(recipe, &st, &p) {
            let lv = liveness(recipe, &st, &p, &ctx);
            if lv.running {
                stop_process(recipe, &ctx, lv.pid)?;
            }
        }
        let _ = autostart::disable(&recipe.name);
    }
    process::remove_pid_file(&p.data);
    remove_shims(recipe);
    last_errors().lock().unwrap().remove(&recipe.name);
    if p.versions.is_dir() {
        std::fs::remove_dir_all(&p.versions).map_err(|e| format!("remove {}: {e}", p.versions.display()))?;
    }
    if keep_data {
        let mut s = st;
        s.installed_version = None;
        s.autostart = false;
        s.user_stopped = false;
        s.restart_pending = false;
        state::save(&p.data, &s)?;
    } else {
        if p.root.is_dir() {
            std::fs::remove_dir_all(&p.root).map_err(|e| format!("remove {}: {e}", p.root.display()))?;
        }
        // A clean slate includes the apps allowed to connect: a reinstall
        // asks the user again before any app gets a key.
        for c in consent::consumers_for(&recipe.name) {
            if let Err(e) = consent::revoke(&c.id, Some(&recipe.name)) {
                log::warn!("could not revoke {}'s grant for {}: {e}", c.id, recipe.name);
            }
        }
    }
    Ok(())
}

/// Dry run for recipe authors: resolve the release for this platform and
/// render the files with the current (or default) state — no download, no
/// write.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DryRun {
    pub platform: String,
    pub supported: bool,
    pub resolved: Option<install::Resolved>,
    pub resolve_error: Option<String>,
    pub asset_reachable: Option<bool>,
    pub files: Vec<RenderedFile>,
    pub run_args: Vec<String>,
    pub create_dirs: Vec<String>,
    pub connection_url: Option<String>,
    /// Where the release is unpacked (`…/tools/<name>/versions/<version>`).
    pub install_dir: String,
    /// The tool's private data dir (state, rendered config) and its logs.
    pub data_dir: String,
    pub logs_dir: String,
    /// Cli tools: the shim consumers run. Daemons: none.
    pub bin_path: Option<String>,
    /// Daemon ports as they would be chosen now (all bound to 127.0.0.1).
    pub ports: std::collections::BTreeMap<String, u16>,
}

pub fn dry_run(recipe: &Recipe) -> Result<DryRun, String> {
    let platform = Platform::current();
    let supported = recipe.supported_on(&platform);
    let (resolved, resolve_error) = if supported {
        match install::resolve_latest(recipe, &platform) {
            Ok(r) => (Some(r), None),
            Err(e) => (None, Some(e)),
        }
    } else {
        (None, None)
    };
    let asset_reachable = resolved.as_ref().map(|r| {
        reqwest::blocking::Client::builder()
            .user_agent("Roadie")
            .timeout(Duration::from_secs(20))
            .build()
            .ok()
            .and_then(|c| c.head(&r.download_url).send().ok())
            .map(|resp| resp.status().is_success())
            .unwrap_or(false)
    });
    // Render against an in-memory state: real values when installed,
    // defaults otherwise, with secrets masked so a dry run never leaks them.
    // Nothing is written — two dry runs at once (the card and a prompt)
    // must not race on a scratch file.
    let p = paths::tool_paths(&recipe.name)?;
    let mut st = state::preview(recipe, &p.data, &platform);
    for v in st.secrets.values_mut() {
        *v = "<secret>".into();
    }
    let mut ctx = build_ctx(recipe, &st, &p)?;
    if ctx.consumers.is_empty() {
        ctx.consumers = vec![template::Consumer { id: "example-consumer".into(), key: "<consumer-key>".into() }];
    }
    let resolved_version = resolved.as_ref().map(|r| r.version.clone());
    Ok(DryRun {
        platform: platform.key(),
        supported,
        resolved,
        resolve_error,
        asset_reachable,
        files: render_files(recipe, &ctx)?,
        run_args: recipe
            .run
            .as_ref()
            .map(|r| r.args.iter().map(|a| template::expand_string(a, &ctx)).collect::<Result<_, _>>())
            .transpose()?
            .unwrap_or_default(),
        create_dirs: recipe.create_dirs.iter().map(|d| template::expand_string(d, &ctx)).collect::<Result<_, _>>()?,
        connection_url: ctx.connection_url.clone(),
        install_dir: p.versions.join(resolved_version.as_deref().unwrap_or("<version>")).to_string_lossy().into_owned(),
        data_dir: p.data.to_string_lossy().into_owned(),
        logs_dir: p.logs.to_string_lossy().into_owned(),
        bin_path: (recipe.kind == Kind::Cli).then(|| shim_path(recipe).ok().map(|s| s.to_string_lossy().into_owned())).flatten(),
        ports: if recipe.kind == Kind::Daemon { st.ports.clone() } else { Default::default() },
    })
}

// --- Background ---

/// The service's startup pass — in effect "login" for the tools: adopt or
/// clean pid files, start every daemon marked "start at login", remove
/// per-tool login items left by an older Roadie, apply what was staged
/// while stopped.
pub fn reconcile(recipes: &[Recipe], emit: &dyn Fn(&str, Value)) {
    for recipe in recipes.iter().filter(|r| r.kind == Kind::Daemon) {
        let Ok(p) = paths::tool_paths(&recipe.name) else { continue };
        if install::current_version(recipe, &p).is_none() && install::staged_upgrade(recipe, &p).is_none() {
            continue;
        }
        let st = state::load(&p.data);
        if st.schema == 0 {
            continue;
        }
        let Ok(ctx) = build_ctx(recipe, &st, &p) else { continue };
        let lv = liveness(recipe, &st, &p, &ctx);
        if autostart::is_enabled(&recipe.name) {
            match autostart::disable(&recipe.name) {
                Ok(()) => log::info!("removed the old per-tool login item for {}; the service starts it now", recipe.name),
                Err(e) => log::warn!("could not remove the old {} login item: {e}", recipe.name),
            }
        }
        if !lv.running && lv.conflict.is_none() {
            if st.autostart {
                // Service start is the tools' login: a Stop from the previous
                // session does not carry over, exactly as a login item would
                // have started it.
                if st.user_stopped {
                    let guard = lock(&recipe.name);
                    let _g = guard.lock().unwrap();
                    let mut s = state::load(&p.data);
                    s.user_stopped = false;
                    let _ = state::save(&p.data, &s);
                }
                if let Err(e) = start(recipe, "service") {
                    log::warn!("autostart of {} failed: {e}", recipe.name);
                }
            } else {
                let guard = lock(&recipe.name);
                let _g = guard.lock().unwrap();
                let mut s = st.clone();
                if let Err(e) = apply_pending(recipe, &mut s, &p, false) {
                    log::warn!("could not apply staged {}: {e}", recipe.name);
                }
            }
        }
        emit("tool-status-changed", serde_json::json!({ "name": recipe.name }));
    }
}

/// Daily pass: stage the latest release in the background, then apply it
/// only if the tool is a cli, or a daemon that is stopped or idle.
pub fn auto_update(recipes: &[Recipe], emit: &dyn Fn(&str, Value)) {
    let platform = Platform::current();
    for recipe in recipes {
        let Ok(p) = paths::tool_paths(&recipe.name) else { continue };
        let Some(current) = install::current_version(recipe, &p) else { continue };
        latest_cache().invalidate(&recipe.name);
        let resolved = match install::latest(recipe, &platform, latest_cache()) {
            Ok(r) => r,
            Err(e) => {
                log::warn!("{} release lookup failed: {e}", recipe.name);
                continue;
            }
        };
        let guard = lock(&recipe.name);
        let _g = guard.lock().unwrap();
        let mut st = state::load(&p.data);
        let newer = if resolved.floating {
            false // floating sources are refreshed only on explicit Update
        } else {
            install::version_lt(&current, &resolved.version)
        };
        if newer && !install::staged_versions(recipe, &p).iter().any(|v| v == &resolved.version) {
            match install::download_and_stage(recipe, &p, &resolved, &mut |_, _, _| {}) {
                Ok(sha) => {
                    st.archive_sha256 = Some(sha);
                    let _ = state::save(&p.data, &st);
                }
                Err(e) => {
                    log::warn!("staging {} {} failed: {e}", recipe.name, resolved.version);
                    continue;
                }
            }
        }
        match apply_pending(recipe, &mut st, &p, true) {
            Ok(ApplyOutcome::Applied { from, to }) => {
                log::info!("{} updated {:?} -> {}", recipe.name, from, to);
                emit("tool-updated", serde_json::json!({ "name": recipe.name, "from": from, "to": to }));
            }
            Ok(ApplyOutcome::Deferred { reason }) => log::info!("{} update staged, deferred ({reason:?})", recipe.name),
            Ok(ApplyOutcome::Nothing) => {}
            Err(e) => log::warn!("applying {} update failed: {e}", recipe.name),
        }
        emit("tool-status-changed", serde_json::json!({ "name": recipe.name }));
    }
}

pub fn log_tail(recipe: &Recipe, lines: usize) -> Result<String, String> {
    let p = paths::tool_paths(&recipe.name)?;
    Ok(process::log_tail(&p.logs, lines))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slskd_files_render_like_the_reference_yml() {
        let recipe = recipe::load_builtin().remove(0);
        let mut ctx = Ctx::empty(Platform::current());
        ctx.home = "/Users/x".into();
        ctx.data = "/data".into();
        ctx.ports.insert("web".into(), 5031);
        ctx.ports.insert("listen".into(), 50300);
        ctx.config.insert("downloadsDir".into(), Value::String(r"C:\Users\x\Music\Soulseek".into()));
        ctx.config.insert("shareDownloads".into(), Value::Bool(true));
        ctx.config.insert("soulseekUsername".into(), Value::String("björk".into()));
        ctx.secrets.insert("internalKey".into(), "i".repeat(48));
        ctx.secrets.insert("webPassword".into(), "web-pass".into());
        ctx.secrets.insert("soulseekPassword".into(), "p#a:s\"s\\w'ord ü".into());
        ctx.connection_url = Some("http://127.0.0.1:5031".into());
        ctx.consumers = vec![template::Consumer { id: "viboplr".into(), key: "k".repeat(48) }];
        let files = render_files(&recipe, &ctx).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "/data/slskd.yml");
        assert!(files[0].secret);
        let y = &files[0].contents;
        assert!(y.contains("  port: 5031\n"), "{y}");
        assert!(y.contains("  ip_address: \"127.0.0.1\"\n"));
        assert!(y.contains(&format!("      roadie:\n        key: \"{}\"\n", "i".repeat(48))), "{y}");
        assert!(y.contains(&format!("      viboplr:\n        key: \"{}\"\n", "k".repeat(48))));
        assert!(y.contains(r#"  downloads: "C:\\Users\\x\\Music\\Soulseek""#));
        assert!(y.contains("  directories:\n    - \"C:\\\\Users"));
        assert!(y.contains("  username: \"björk\"\n"));
        assert!(y.contains(&format!("  password: {}\n", serde_json::to_string("p#a:s\"s\\w'ord ü").unwrap())));
        assert!(y.contains("  listen_port: 50300\n"));

        ctx.config.insert("shareDownloads".into(), Value::Bool(false));
        let y = render_files(&recipe, &ctx).unwrap().remove(0).contents;
        assert!(y.contains("shares:\n  directories: []\n"), "{y}");
    }
}
