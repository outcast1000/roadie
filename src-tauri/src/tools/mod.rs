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
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub fn latest_cache() -> &'static LatestCache {
    static C: OnceLock<LatestCache> = OnceLock::new();
    C.get_or_init(LatestCache::default)
}

/// Held around every change to a tool: an in-process mutex (the threads of
/// one service) and a file lock in `<data>/locks/` (separate processes: two
/// CLI runs, or one and a `maintain` at login). A lock file that cannot be
/// taken is logged and the change goes ahead under the mutex alone. Fields
/// drop in order, so the file lock is released before the mutex.
struct ToolLock {
    _file: Option<paths::FileLock>,
    _guard: std::sync::MutexGuard<'static, ()>,
}

fn lock(name: &str) -> ToolLock {
    static LOCKS: OnceLock<Mutex<HashMap<String, &'static Mutex<()>>>> = OnceLock::new();
    let m: &'static Mutex<()> = LOCKS.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap().entry(name.to_string()).or_insert_with(|| Box::leak(Box::new(Mutex::new(()))));
    let guard = m.lock().unwrap_or_else(|e| e.into_inner());
    let file = paths::data_root().ok().and_then(|root| {
        paths::lock_file(&root.join("locks").join(format!("{name}.lock")))
            .map_err(|e| log::warn!("{e}; relying on the in-process lock"))
            .ok()
    });
    ToolLock { _file: file, _guard: guard }
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
    /// The recipe declares a `connection.webLogin`. The login itself is a
    /// secret and comes only from the owner route / an approved connection.
    pub has_web_login: bool,
    pub approved_consumers: Vec<String>,
    /// Non-secret config values plus `has_<key>` for secret fields.
    pub config: Map<String, Value>,
    /// `health.extract` `details.*` and `logExtract` values.
    pub details: Map<String, Value>,
    pub reported_version: Option<String>,
    pub health_detail: Option<String>,
    /// Where the installed release is unpacked (`…/tools/<name>/versions/<version>`);
    /// none until installed.
    pub install_dir: Option<String>,
    /// An install running now (in this or another process): `{phase,
    /// downloaded, total, updatedAt}`; phase `resolving` before the download.
    pub installing: Option<Value>,
    /// Where releases are unpacked (`installDir`, or the default), installed
    /// or not: the install prompt shows it.
    pub versions_dir: String,
    /// False once the tool owns its configuration (every file `writeOnce`
    /// and written): Roadie's settings no longer apply.
    pub configurable: bool,
    /// The tool's private data dir: state, secrets and its rendered config.
    pub data_dir: String,
    pub logs_dir: String,
    /// The recipe's `files`, paths only, so an app can show the user where the
    /// tool's configuration lives. Contents never travel here: `secret` marks a
    /// file that holds secrets.
    pub config_files: Vec<ConfigFile>,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConfigFile {
    pub path: String,
    pub secret: bool,
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

/// The recipe's `connection.webLogin`, expanded against the tool's state:
/// `Ok(None)` when the recipe declares none. A secret — callers decide who
/// may see it (the owner, an approved consumer), status never carries it.
pub fn web_login(recipe: &Recipe) -> Result<Option<Value>, String> {
    let Some(w) = recipe.connection.as_ref().and_then(|c| c.web_login.as_ref()) else { return Ok(None) };
    let p = paths::tool_paths(&recipe.name)?;
    let st = state::load(&p.data);
    let ctx = build_ctx(recipe, &st, &p)?;
    Ok(Some(serde_json::json!({
        "username": template::expand_string(&w.username, &ctx)?,
        "password": template::expand_string(&w.password, &ctx)?,
    })))
}

/// Another copy of the tool already running here, found before Roadie's
/// own copy runs: something answers the recipe's health check on a port the
/// install would use but rejects Roadie's key (or accepts any). Roadie never
/// fights it; clients and the prompt say so before the user installs.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OtherInstance {
    /// Where it answers (the recipe's connection URL on that port).
    pub url: String,
    /// `singleton`: Roadie's copy will not start at all while it runs.
    /// Otherwise only the port is taken.
    pub blocks_start: bool,
    pub message: String,
}

/// Look for another copy on the ports the install would use (`chosen` over
/// the defaults) and, for a `singleton`, on the default ports too. `None`
/// while Roadie's own copy is the one running, or when nothing answers.
pub fn other_instance(recipe: &Recipe, chosen: &std::collections::BTreeMap<String, u16>) -> Option<OtherInstance> {
    if recipe.kind != Kind::Daemon || recipe.health.is_none() {
        return None;
    }
    let p = paths::tool_paths(&recipe.name).ok()?;
    let st = state::preview(recipe, &p.data, &Platform::current());
    if install::current_version(recipe, &p).is_some() && build_ctx(recipe, &st, &p).is_ok_and(|ctx| liveness(recipe, &st, &p, &ctx).running) {
        return None;
    }
    let mut planned = st.ports.clone();
    planned.extend(chosen.iter().map(|(k, v)| (k.clone(), *v)));
    let mut candidates = vec![planned];
    if recipe.singleton {
        let defaults: std::collections::BTreeMap<String, u16> = recipe.ports.iter().map(|(k, d)| (k.clone(), d.default)).collect();
        if !candidates.contains(&defaults) {
            candidates.push(defaults);
        }
    }
    let name = &recipe.display_name;
    for ports in candidates {
        let mut s = st.clone();
        s.ports = ports;
        let Ok(ctx) = build_ctx(recipe, &s, &p) else { continue };
        if matches!(probe_health(recipe, &ctx), Health::Unauthorized | Health::Ok { .. }) {
            let url = ctx.connection_url.clone().unwrap_or_else(|| format!("{:?}", s.ports));
            let message = if recipe.singleton {
                format!("Another {name} is already running on this computer ({url}). {name} runs one copy per computer, so Roadie's copy will not start until that one is quit.")
            } else {
                format!("Something already answers on {url}. Roadie's {name} cannot start on that port while it runs: choose another port, or quit it.")
            };
            return Some(OtherInstance { url, blocks_start: recipe.singleton, message });
        }
    }
    None
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
/// binary it runs, plus the recipe's health probe. Cleans a stale pid file;
/// records one for a daemon its login item started.
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
    let pid_file = match process::read_pid_file(&p.data) {
        Some(pf) if process::is_ours(pf.pid, &p.versions) => Some(pf),
        other => {
            if other.is_some() {
                process::remove_pid_file(&p.data);
            }
            adopt_login_instance(recipe, p)
        }
    };
    match pid_file {
        Some(pf) => {
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
        None => {
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
    let install_dir = version.as_deref().map(|v| install::version_dir(&p, v).to_string_lossy().into_owned());
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
        has_web_login: recipe.connection.as_ref().is_some_and(|c| c.web_login.is_some()),
        approved_consumers: consent::consumers_for(&recipe.name).into_iter().map(|c| c.id).collect(),
        config: state::public_config(recipe, &st),
        details: lv.details,
        reported_version: lv.reported_version,
        health_detail: lv.health_detail,
        install_dir,
        installing: installing(&p),
        versions_dir: p.versions.to_string_lossy().into_owned(),
        configurable: !ctx.as_ref().is_some_and(|c| settings_frozen(recipe, c)),
        data_dir: p.data.to_string_lossy().into_owned(),
        logs_dir: p.logs.to_string_lossy().into_owned(),
        config_files: ctx.as_ref().map(|c| config_file_paths(recipe, c)).unwrap_or_default(),
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
        has_web_login: false,
        approved_consumers: vec![],
        config: Map::new(),
        details: Map::new(),
        reported_version: None,
        health_detail: None,
        install_dir: None,
        installing: None,
        versions_dir: String::new(),
        configurable: true,
        data_dir: String::new(),
        logs_dir: String::new(),
        config_files: vec![],
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
        .enumerate()
        .map(|(i, f)| {
            let path = template::expand_string(&f.path, ctx)?;
            let mut content = template::expand_value(&f.content, ctx)?;
            recipe::apply_entries(recipe, i, &mut content, &ctx.config);
            let contents = recipe::emit::render(f.format, &content)?;
            Ok(RenderedFile { path, secret: f.secret, contents })
        })
        .collect()
}

/// Where the recipe's files live, without rendering their contents (a
/// status read must stay cheap and never touch secrets). A path that fails
/// to expand is left out rather than failing the status.
pub fn config_file_paths(recipe: &Recipe, ctx: &Ctx) -> Vec<ConfigFile> {
    recipe
        .files
        .iter()
        .filter_map(|f| template::expand_string(&f.path, ctx).ok().map(|path| ConfigFile { path, secret: f.secret }))
        .collect()
}

/// True once a `writeOnce` file exists: from then on the tool owns it, and
/// anything it carries (ports, keys) must not change under it.
fn files_frozen(recipe: &Recipe, ctx: &Ctx) -> bool {
    recipe.files.iter().filter(|f| f.write_once).any(|f| template::expand_string(&f.path, ctx).is_ok_and(|path| std::path::Path::new(&path).exists()))
}

/// After install, every config file is the tool's: Roadie's settings no
/// longer reach it.
fn settings_frozen(recipe: &Recipe, ctx: &Ctx) -> bool {
    !recipe.files.is_empty() && recipe.files.iter().all(|f| f.write_once) && files_frozen(recipe, ctx)
}

/// Create the recipe's directories and (re)write its config files. A
/// `writeOnce` file that exists is the tool's and is left alone.
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
    for (f, rf) in recipe.files.iter().zip(render_files(recipe, ctx)?) {
        let path = PathBuf::from(&rf.path);
        if f.write_once && path.exists() {
            continue;
        }
        paths::write_atomic(&path, rf.contents.as_bytes(), rf.secret)?;
    }
    Ok(())
}

/// Keep a port that is free or already ours; otherwise the first free one
/// after the default. A *foreign* instance on the port is left in place and
/// reported by liveness.
fn choose_ports(recipe: &Recipe, st: &mut ToolState, p: &ToolPaths) -> Result<(), String> {
    if files_frozen(recipe, &build_ctx(recipe, st, p)?) {
        return Ok(());
    }
    for (name, def) in recipe.ports.iter().filter(|(n, d)| d.pick && !st.chosen_ports.contains(*n)) {
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
                // Never a port another of this tool's ports already has.
                let taken: Vec<u16> = st.ports.iter().filter(|(n, _)| *n != name).map(|(_, p)| *p).collect();
                let picked = (def.default + 1..=def.default + 10).find(|q| !taken.contains(q) && std::net::TcpListener::bind(("127.0.0.1", *q)).is_ok());
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
/// While an install runs, `<data>/installing.json` says how far it got, so
/// `status` in any process (a client polling the CLI) can report it.
const INSTALLING_FILE: &str = "installing.json";

/// Install or update, reporting progress to `progress` and to status
/// (`installing`) until it finishes, either way.
pub fn install(recipe: &Recipe, progress: Progress) -> Result<ToolStatus, String> {
    let marker = paths::tool_paths(&recipe.name).ok().map(|p| p.data.join(INSTALLING_FILE));
    let write = |phase: Value, downloaded: u64, total: Option<u64>| {
        if let Some(m) = &marker {
            let v = serde_json::json!({ "phase": phase, "downloaded": downloaded, "total": total, "pid": std::process::id(), "updatedAt": paths::now_secs() });
            let _ = paths::write_atomic(m, v.to_string().as_bytes(), false);
        }
    };
    write(Value::String("resolving".into()), 0, None);
    let mut last = std::time::Instant::now();
    let mut both = |phase: install::Phase, done: u64, total: Option<u64>| {
        if last.elapsed() >= Duration::from_millis(250) || Some(done) == total {
            write(serde_json::to_value(phase).unwrap_or_default(), done, total);
            last = std::time::Instant::now();
        }
        progress(phase, done, total);
    };
    let out = install_locked(recipe, &mut both);
    if let Some(m) = &marker {
        let _ = std::fs::remove_file(m);
    }
    out
}

/// An install in progress, from its marker; a marker whose process is gone
/// (a crash) is not one.
fn installing(p: &ToolPaths) -> Option<Value> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(p.data.join(INSTALLING_FILE)).ok()?).ok()?;
    let pid = v.get("pid")?.as_u64()? as u32;
    let alive = pid == std::process::id() || process::pid_alive(pid);
    alive.then(|| serde_json::json!({ "phase": v["phase"], "downloaded": v["downloaded"], "total": v["total"], "updatedAt": v["updatedAt"] }))
}

fn install_locked(recipe: &Recipe, progress: Progress) -> Result<ToolStatus, String> {
    let _g = lock(&recipe.name);
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
        sync_login_item_logged(recipe, &st, &p);
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
    if staged.is_some() {
        sync_login_item_logged(recipe, st, p);
    }
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
    let _g = lock(&recipe.name);
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

    let plan = launch_plan(recipe, &ctx, p, &version)?;
    let pid = process::spawn_detached(&plan)?;
    let started_at = paths::now_secs();
    process::write_pid_file(&p.data, &process::PidFile { pid, version, exe: plan.exe.clone(), started_at, started_by: started_by.to_string() })?;
    st.user_stopped = false;
    st.last_start = Some(started_at);
    state::save(&p.data, st)?;
    // The ports may have moved; the login item starts it with these.
    sync_login_item_logged(recipe, st, p);

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

/// How the daemon is started: by Roadie (`spawn_detached`) and by its own
/// login item, which must run exactly this.
fn launch_plan(recipe: &Recipe, ctx: &Ctx, p: &ToolPaths, version: &str) -> Result<process::SpawnPlan, String> {
    let run = recipe.run.as_ref().ok_or("recipe has no run block")?;
    Ok(process::SpawnPlan {
        exe: install::binary_path(recipe, p, version),
        args: run.args.iter().map(|a| template::expand_string(a, ctx)).collect::<Result<_, _>>()?,
        env: run.env.iter().map(|(k, v)| template::expand_string(v, ctx).map(|v| (k.clone(), v))).collect::<Result<_, _>>()?,
        cwd: run.cwd.as_ref().map(|c| template::expand_string(c, ctx).map(PathBuf::from)).transpose()?,
        log: p.logs.join(process::STDOUT_LOG),
        append_log: false,
    })
}

/// Keep the daemon's own login item in step with its state: present while
/// "start at login" is on and a version is installed, running the current
/// version with the args, env and ports it would start with now; gone
/// otherwise. Unchanged items are left alone, so every mutation calls this.
/// Callers hold the tool's lock.
fn sync_login_item(recipe: &Recipe, st: &ToolState, p: &ToolPaths) -> Result<(), String> {
    // Tests run against the real home folder; a login item is not theirs to write.
    if !autostart::NATIVE_TOOL_ITEMS || recipe.kind != Kind::Daemon || cfg!(test) {
        return Ok(());
    }
    let root = paths::data_root()?;
    match install::current_version(recipe, p) {
        Some(version) if st.autostart => {
            let mut st = st.clone();
            st.installed_version = Some(version.clone());
            let ctx = build_ctx(recipe, &st, p)?;
            autostart::enable_tool(&recipe.name, root, &launch_plan(recipe, &ctx, p, &version)?)
        }
        _ => autostart::disable_tool(&recipe.name, root),
    }
}

/// For changes whose own work succeeded: a login item that could not be
/// rewritten is logged, and the next change tries again.
fn sync_login_item_logged(recipe: &Recipe, st: &ToolState, p: &ToolPaths) {
    if let Err(e) = sync_login_item(recipe, st, p) {
        log::warn!("could not update {}'s login item: {e}", recipe.name);
    }
}

/// Bring every daemon's login item in step (the CLI release after each
/// command: items from an older Roadie, or a data dir that moved).
pub fn sync_login_items(recipes: &[Recipe]) {
    for recipe in recipes.iter().filter(|r| r.kind == Kind::Daemon) {
        let Ok(p) = paths::tool_paths(&recipe.name) else { continue };
        let _g = lock(&recipe.name);
        sync_login_item_logged(recipe, &state::load(&p.data), &p);
    }
}

/// A daemon its login item started has no pid file; launchd knows its pid.
/// Recording it lets Stop signal it and keeps a start right after login
/// (an app running `maintain`) from racing a second copy onto the port.
fn adopt_login_instance(recipe: &Recipe, p: &ToolPaths) -> Option<process::PidFile> {
    let root = paths::data_root().ok()?;
    if !autostart::NATIVE_TOOL_ITEMS || !autostart::tool_enabled(&recipe.name, root) {
        return None;
    }
    let pid = autostart::tool_item_pid(&recipe.name, root)?;
    if !process::is_ours(pid, &p.versions) {
        return None;
    }
    let pf = process::PidFile {
        pid,
        version: install::current_version(recipe, p).unwrap_or_default(),
        exe: process::pid_exe(pid).unwrap_or_default(),
        started_at: paths::now_secs(),
        started_by: "login".into(),
    };
    if let Err(e) = process::write_pid_file(&p.data, &pf) {
        log::warn!("could not record the {} its login item started: {e}", recipe.name);
    }
    Some(pf)
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
    let _g = lock(&recipe.name);
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstallOptions {
    pub start_now: bool,
    pub autostart: bool,
    /// `installDir`: where the release is unpacked instead of the default.
    pub install_dir: Option<PathBuf>,
    /// `ports.<name>` the app or user chose.
    pub ports: std::collections::BTreeMap<String, u16>,
    /// `secrets.<key>` the app or user gave.
    pub secrets: std::collections::BTreeMap<String, String>,
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
    let mut o = InstallOptions {
        start_now: take(recipe, values, "startNow", recipe.start_after_install)?,
        autostart: take(recipe, values, "autostart", recipe.autostart)?,
        ..Default::default()
    };
    // A blank answer in a form means "the default".
    let blank = |v: &Value| v.is_null() || v.as_str().is_some_and(|t| t.trim().is_empty());
    values.retain(|k, v| !((k == "installDir" || k.starts_with(recipe::PORT_DECISION) || k.starts_with(recipe::SECRET_DECISION)) && blank(v)));
    if let Some(v) = values.remove("installDir") {
        let dir = v.as_str().map(str::trim).filter(|d| !d.is_empty()).ok_or("`installDir` must be a folder path")?;
        let dir = PathBuf::from(dir);
        if !dir.is_absolute() {
            return Err(format!("`installDir` must be an absolute path, not {}", dir.display()));
        }
        o.install_dir = Some(dir);
    }
    let keys: Vec<String> = values.keys().filter(|k| k.starts_with(recipe::PORT_DECISION) || k.starts_with(recipe::SECRET_DECISION)).cloned().collect();
    for key in keys {
        let v = values.remove(&key).unwrap_or(Value::Null);
        if let Some(name) = key.strip_prefix(recipe::PORT_DECISION) {
            if !recipe.ports.contains_key(name) {
                return Err(format!("`{key}` names no port of {} (it has: {})", recipe.display_name, recipe.ports.keys().cloned().collect::<Vec<_>>().join(", ")));
            }
            let n = v.as_u64().or_else(|| v.as_str().and_then(|t| t.trim().parse().ok()));
            let port = n.filter(|n| (1..=65535).contains(n)).ok_or_else(|| format!("`{key}` must be a port number (1–65535)"))?;
            o.ports.insert(name.to_string(), port as u16);
        } else if let Some(name) = key.strip_prefix(recipe::SECRET_DECISION) {
            let def = recipe.secrets.iter().find(|s| s.key == name).ok_or_else(|| format!("`{key}` names no secret of {}", recipe.display_name))?;
            let value = v.as_str().unwrap_or_default();
            if value.chars().count() < def.min_len() || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err(format!("`{key}` must be at least {} characters, with no spaces", def.min_len()));
            }
            o.secrets.insert(name.to_string(), value.to_string());
        }
    }
    Ok(o)
}

/// Refuse to install while a secret with no `generate` has no value.
pub fn require_secrets(recipe: &Recipe) -> Result<(), String> {
    let p = paths::tool_paths(&recipe.name)?;
    let st = state::load(&p.data);
    let missing: Vec<&str> = recipe.secrets.iter().filter(|s| s.required() && !st.secrets.contains_key(&s.key)).map(|s| s.label.as_deref().unwrap_or(&s.key)).collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!("{} needs {} to install; Roadie cannot generate it", recipe.display_name, missing.join(", ")))
    }
}

/// Record the install decisions that are not config: the install folder,
/// chosen ports and secrets. Before the first install only: an installed
/// tool keeps its folder (reinstall to move it), and ports or keys already
/// written into a file the tool owns cannot change under it.
pub fn apply_install_choices(recipe: &Recipe, o: &InstallOptions) -> Result<(), String> {
    if o.install_dir.is_none() && o.ports.is_empty() && o.secrets.is_empty() {
        return Ok(());
    }
    let _g = lock(&recipe.name);
    let p = paths::tool_paths(&recipe.name)?;
    let installed = install::current_version(recipe, &p).is_some();
    if let Some(dir) = &o.install_dir {
        if installed && dir != &p.versions {
            return Err(format!("{} is installed in {}; uninstall it to install it elsewhere", recipe.display_name, p.versions.display()));
        }
        let empty = std::fs::read_dir(dir).map(|mut d| d.next().is_none()).unwrap_or(true);
        if !installed && !empty {
            return Err(format!("{} is not empty; choose a new or empty folder for {} (Roadie removes it on uninstall)", dir.display(), recipe.display_name));
        }
        std::fs::create_dir_all(&p.root).map_err(|e| format!("create {}: {e}", p.root.display()))?;
        paths::write_atomic(&p.root.join(paths::INSTALL_DIR_FILE), dir.to_string_lossy().as_bytes(), false)?;
    }
    std::fs::create_dir_all(&p.data).map_err(|e| format!("create {}: {e}", p.data.display()))?;
    let mut st = state::load(&p.data);
    let frozen = files_frozen(recipe, &build_ctx(recipe, &st, &p)?);
    for (name, port) in &o.ports {
        if frozen && st.ports.get(name) != Some(port) {
            return Err(format!("{} already runs with port {name} {}, in the configuration it owns; change it there", recipe.display_name, st.ports.get(name).copied().unwrap_or_default()));
        }
        st.ports.insert(name.clone(), *port);
        st.chosen_ports.insert(name.clone());
    }
    for (key, value) in &o.secrets {
        if frozen && st.secrets.get(key) != Some(value) {
            return Err(format!("{} already has its {key}, in the configuration it owns; change it there", recipe.display_name));
        }
        st.secrets.insert(key.clone(), value.clone());
    }
    state::save(&p.data, &st)
}

pub fn set_autostart(recipe: &Recipe, enabled: bool) -> Result<ToolStatus, String> {
    if recipe.kind != Kind::Daemon {
        return Err("only daemons start at login".into());
    }
    let _g = lock(&recipe.name);
    let p = paths::tool_paths(&recipe.name)?;
    let mut st = state::load_or_init(recipe, &p.data, &Platform::current())?;
    // A per-tool item from an older Roadie (`roadie --start-tool`) goes.
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
    // The daemon's own login item (macOS); on Windows the flag is what
    // Roadie's reconcile acts on at login.
    sync_login_item(recipe, &st, &p)?;
    Ok(status(recipe))
}

/// Apply a config patch; rewrite the files; restart a running daemon when it
/// is idle, otherwise mark the restart pending.
pub fn configure(recipe: &Recipe, patch: &Map<String, Value>) -> Result<ToolStatus, String> {
    let _g = lock(&recipe.name);
    let p = paths::tool_paths(&recipe.name)?;
    std::fs::create_dir_all(&p.data).map_err(|e| format!("create {}: {e}", p.data.display()))?;
    let mut st = state::load_or_init(recipe, &p.data, &Platform::current())?;
    if !patch.is_empty() && settings_frozen(recipe, &build_ctx(recipe, &st, &p)?) {
        return Err(format!("{} manages its own settings since it was installed; change them in {} itself (its web UI)", recipe.display_name, recipe.display_name));
    }
    state::apply_patch(recipe, &mut st, patch)?;
    let ctx = build_ctx(recipe, &st, &p)?;
    if recipe.kind == Kind::Daemon && install::current_version(recipe, &p).is_some() {
        write_files(recipe, &ctx)?;
        st.restart_pending = true;
        state::save(&p.data, &st)?;
        sync_login_item_logged(recipe, &st, &p);
        if !matches!(apply_pending(recipe, &mut st, &p, true)?, ApplyOutcome::Deferred { .. }) {
            st.restart_pending = false;
            state::save(&p.data, &st)?;
        }
    } else {
        state::save(&p.data, &st)?;
    }
    Ok(status(recipe))
}

/// A trusted recipe is being replaced by `new`. A file Roadie managed that
/// `new` hands to the tool (`writeOnce`) is removed, so the next write
/// renders it once more from `new` and hands over a file in the new shape.
/// Otherwise the tool would keep the old file forever.
pub fn hand_over_files(old: &Recipe, new: &Recipe) -> Result<(), String> {
    let _g = lock(&new.name);
    let p = paths::tool_paths(&new.name)?;
    let st = state::load(&p.data);
    let ctx = build_ctx(new, &st, &p)?;
    for f in new.files.iter().filter(|f| f.write_once) {
        if old.files.iter().any(|o| o.path == f.path && !o.write_once) {
            let path = template::expand_string(&f.path, &ctx)?;
            if std::path::Path::new(&path).exists() {
                std::fs::remove_file(&path).map_err(|e| format!("replace {path}: {e}"))?;
                log::info!("{}: {path} is now {}'s own; written once more from the new recipe", new.name, new.display_name);
            }
        }
    }
    Ok(())
}

/// A consumer's grant changed: re-render the config (the key list lives in
/// it) and restart when idle. Only per-consumer keys live in the config; a
/// shared key or an open connection has nothing to re-render.
pub fn refresh_consumers(recipe: &Recipe) -> Result<ToolStatus, String> {
    if recipe.connection.as_ref().map(|c| c.policy) != Some(recipe::ConnectionPolicy::PerConsumerKey) {
        return Ok(status(recipe));
    }
    configure(recipe, &Map::new())
}

/// Stop, drop the login item, delete the versions and (unless `keep_data`)
/// the data dir. User folders named by config are never touched.
pub fn uninstall(recipe: &Recipe, keep_data: bool) -> Result<(), String> {
    let _g = lock(&recipe.name);
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
        if autostart::NATIVE_TOOL_ITEMS {
            let root = paths::data_root()?;
            autostart::disable_tool(&recipe.name, root)?;
        }
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
    /// Another copy already running here, which Roadie's would clash with.
    pub other_instance: Option<OtherInstance>,
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
        other_instance: other_instance(recipe, &Default::default()),
    })
}

// --- Background ---

/// The service's startup pass: adopt or clean pid files, bring each daemon's
/// own login item in step (removing the `--start-tool` items of an older
/// Roadie), start the daemons marked "start at login" that no login item of
/// their own starts (Windows, or the first login after an upgrade), apply
/// what was staged while stopped.
pub fn reconcile(recipes: &[Recipe], emit: &dyn Fn(&str, Value)) {
    reconcile_with(recipes, emit, true)
}

/// `at_login` false (the CLI's `maintain` run by an app, not at login): a
/// daemon the user stopped stays stopped.
pub fn reconcile_with(recipes: &[Recipe], emit: &dyn Fn(&str, Value), at_login: bool) {
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
                Ok(()) => log::info!("removed the old per-tool login item for {}", recipe.name),
                Err(e) => log::warn!("could not remove the old {} login item: {e}", recipe.name),
            }
        }
        // At login, a daemon with its own login item is launchd's to start
        // (it may not have got to it yet): a second copy would only fail on
        // the port. The first login after an upgrade has no item yet, so
        // Roadie starts it that once, as before.
        let own_item = autostart::NATIVE_TOOL_ITEMS && paths::data_root().is_ok_and(|r| autostart::tool_enabled(&recipe.name, r));
        let login_item_starts_it = at_login && own_item && st.autostart;
        {
            let _g = lock(&recipe.name);
            let mut s = state::load(&p.data);
            // Started at login despite last session's Stop, as a login does.
            if lv.running && s.user_stopped && at_login {
                s.user_stopped = false;
                let _ = state::save(&p.data, &s);
            }
            sync_login_item_logged(recipe, &s, &p);
        }
        if !lv.running && lv.conflict.is_none() && !login_item_starts_it {
            if st.autostart && (at_login || !st.user_stopped) {
                // Service start is the tools' login: a Stop from the previous
                // session does not carry over, exactly as a login item would
                // have started it.
                if st.user_stopped {
                    let _g = lock(&recipe.name);
                    let mut s = state::load(&p.data);
                    s.user_stopped = false;
                    let _ = state::save(&p.data, &s);
                }
                if let Err(e) = start(recipe, "service") {
                    log::warn!("autostart of {} failed: {e}", recipe.name);
                }
            } else {
                let _g = lock(&recipe.name);
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
        let _g = lock(&recipe.name);
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
        let recipe = recipe::fixtures::recipe("slskd");
        let mut ctx = Ctx::empty(Platform::current());
        ctx.home = "/Users/x".into();
        ctx.data = "/data".into();
        ctx.ports.insert("web".into(), 5031);
        ctx.ports.insert("listen".into(), 50300);
        ctx.config.insert("downloadsDir".into(), Value::String(r"C:\Users\x\Music\Soulseek".into()));
        ctx.config.insert("shareDownloads".into(), Value::Bool(true));
        ctx.config.insert("soulseekUsername".into(), Value::String("björk".into()));
        ctx.secrets.insert("internalKey".into(), "i".repeat(48));
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
        assert!(y.contains("    username: \"slskd\"\n    password: \"slskd\"\n"), "slskd's default web login: {y}");
        assert!(y.contains(&format!("      viboplr:\n        key: \"{}\"\n", "k".repeat(48))));
        assert!(y.contains(r#"  downloads: "C:\\Users\\x\\Music\\Soulseek""#));
        assert!(y.contains("  directories:\n    - \"C:\\\\Users"));
        assert!(y.contains("  username: \"björk\"\n"));
        assert!(y.contains(&format!("  password: {}\n", serde_json::to_string("p#a:s\"s\\w'ord ü").unwrap())));
        assert!(y.contains("  listen_port: 50300\n"));

        // slskd refuses every download whose incomplete path isn't already
        // normalized, so on Windows the `/.incomplete` suffix must come out `\`.
        let mut win = ctx.clone();
        win.platform = Platform { os: "windows", arch: "x64" };
        let y = render_files(&recipe, &win).unwrap().remove(0).contents;
        assert!(y.contains(r#"  incomplete: "C:\\Users\\x\\Music\\Soulseek\\.incomplete""#), "{y}");

        ctx.config.insert("shareDownloads".into(), Value::Bool(false));
        let y = render_files(&recipe, &ctx).unwrap().remove(0).contents;
        assert!(y.contains("shares:\n  directories: []\n"), "{y}");

        // The app's folders join the downloads folder in one flat list.
        ctx.config.insert("shareDownloads".into(), Value::Bool(true));
        ctx.config.insert("shares.directories".into(), serde_json::json!(["D:\\Music", "E:\\Rock"]));
        let y = render_files(&recipe, &ctx).unwrap().remove(0).contents;
        assert!(y.contains("  directories:\n    - \"C:\\\\Users\\\\x\\\\Music\\\\Soulseek\"\n    - \"D:\\\\Music\"\n    - \"E:\\\\Rock\"\n"), "{y}");
    }
    #[test]
    fn status_names_the_config_files_by_path_only() {
        let recipe = recipe::fixtures::recipe("slskd");
        let mut ctx = Ctx::empty(Platform::current());
        ctx.data = "/data".into();
        assert_eq!(config_file_paths(&recipe, &ctx), vec![ConfigFile { path: "/data/slskd.yml".into(), secret: true }]);
        let json = serde_json::to_value(ConfigFile { path: "/data/slskd.yml".into(), secret: true }).unwrap();
        assert_eq!(json, serde_json::json!({ "path": "/data/slskd.yml", "secret": true }), "no contents, camelCase on the wire");
    }

    fn r6(name: &str) -> Recipe {
        let mut r = recipe::fixtures::recipe("slskd@6");
        r.name = name.into();
        r
    }

    #[test]
    fn install_decisions_take_ports_secrets_and_a_folder_and_refuse_the_rest() {
        let r = r6("opts-test");
        let mut v = Map::new();
        v.insert("ports.web".into(), serde_json::json!(6000));
        v.insert("secrets.internalKey".into(), serde_json::json!("k".repeat(20)));
        v.insert("installDir".into(), serde_json::json!(if cfg!(windows) { "C:\\Tools\\slskd" } else { "/opt/slskd" }));
        v.insert("soulseekUsername".into(), serde_json::json!("bj"));
        let o = install_options(&r, &mut v).unwrap();
        assert_eq!((o.ports["web"], o.secrets["internalKey"].len(), o.install_dir.is_some()), (6000, 20, true));
        assert_eq!(v.keys().collect::<Vec<_>>(), vec!["soulseekUsername"], "the rest is config");

        for (k, val, why) in [
            ("secrets.internalKey", serde_json::json!("short"), "at least 16"),
            ("ports.web", serde_json::json!(0), "port number"),
            ("ports.nope", serde_json::json!(1), "names no port"),
            ("installDir", serde_json::json!("relative/dir"), "absolute"),
        ] {
            let mut v = Map::new();
            v.insert(k.into(), val);
            let e = install_options(&r, &mut v).unwrap_err();
            assert!(e.contains(why), "{k}: {e}");
        }
        let mut per = recipe::fixtures::recipe("slskd");
        per.name = "opts-test".into();
        let mut v = Map::new();
        v.insert("ports.web".into(), serde_json::json!("6000"));
        assert_eq!(install_options(&per, &mut v).unwrap().ports["web"], 6000, "any port may be chosen, askOnInstall or not; a form's text counts");
    }

    #[test]
    fn chosen_values_stick_and_a_write_once_file_is_the_tools() {
        crate::recipe::store::test_root();
        let r = r6("write-once-test");
        let _ = uninstall(&r, false);
        let dir = std::env::temp_dir().join(format!("roadie-installdir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let o = InstallOptions {
            install_dir: Some(dir.clone()),
            ports: [("web".to_string(), 6031u16)].into(),
            secrets: [("internalKey".to_string(), "chosen-by-the-app-123".to_string())].into(),
            ..Default::default()
        };
        apply_install_choices(&r, &o).unwrap();
        let p = paths::tool_paths(&r.name).unwrap();
        assert_eq!(p.versions, dir, "the chosen folder is where releases go");
        let st = state::load_or_init(&r, &p.data, &Platform::current()).unwrap();
        assert_eq!((st.ports["web"], st.secrets["internalKey"].as_str()), (6031, "chosen-by-the-app-123"), "a chosen key is not regenerated");
        assert!(st.chosen_ports.contains("web"));

        // Installed (faked): the first write lands, then the file is slskd's.
        let bin = install::binary_path(&r, &p, "1.0.0");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, b"x").unwrap();
        install::set_current(&p, "1.0.0").unwrap();
        configure(&r, &Map::new()).unwrap();
        let file = p.data.join("slskd.yml");
        let first = std::fs::read_to_string(&file).unwrap();
        assert!(first.contains("port: 6031") && first.contains("chosen-by-the-app-123") && first.contains("remote_configuration: true"), "{first}");
        std::fs::write(&file, "edited in slskd's web UI\n").unwrap();
        configure(&r, &Map::new()).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "edited in slskd's web UI\n", "never rewritten");
        let mut patch = Map::new();
        patch.insert("soulseekUsername".into(), serde_json::json!("bj"));
        assert!(configure(&r, &patch).unwrap_err().contains("manages its own settings"));
        assert!(!status(&r).configurable);
        let o2 = InstallOptions { ports: [("web".to_string(), 7000u16)].into(), ..Default::default() };
        assert!(apply_install_choices(&r, &o2).unwrap_err().contains("configuration it owns"), "a port baked into slskd's file cannot change under it");
        let elsewhere = InstallOptions { install_dir: Some(dir.join("other")), ..Default::default() };
        assert!(apply_install_choices(&r, &elsewhere).unwrap_err().contains("uninstall it"));

        uninstall(&r, false).unwrap();
        assert!(!dir.exists(), "uninstall removes the chosen folder");
        assert_eq!(paths::tool_paths(&r.name).unwrap().versions, p.root.join("versions"), "and forgets it");
    }

    #[test]
    fn an_install_folder_must_be_new_or_empty() {
        crate::recipe::store::test_root();
        let r = r6("installdir-busy-test");
        let dir = std::env::temp_dir().join(format!("roadie-installdir-busy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("someone-elses.txt"), b"x").unwrap();
        let e = apply_install_choices(&r, &InstallOptions { install_dir: Some(dir.clone()), ..Default::default() }).unwrap_err();
        assert!(e.contains("not empty"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_recipe_update_to_write_once_hands_over_a_fresh_file() {
        crate::recipe::store::test_root();
        let mut old = recipe::fixtures::recipe("slskd");
        old.name = "hand-over-test".into();
        let new = r6("hand-over-test");
        let _ = uninstall(&old, false);
        let p = paths::tool_paths(&old.name).unwrap();
        let bin = install::binary_path(&old, &p, "1.0.0");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, b"x").unwrap();
        install::set_current(&p, "1.0.0").unwrap();
        configure(&old, &Map::new()).unwrap();
        let file = p.data.join("slskd.yml");
        assert!(std::fs::read_to_string(&file).unwrap().contains("remote_configuration: false"));

        hand_over_files(&old, &new).unwrap();
        configure(&new, &Map::new()).unwrap();
        let handed = std::fs::read_to_string(&file).unwrap();
        assert!(handed.contains("remote_configuration: true"), "written once more, in the new shape: {handed}");
        hand_over_files(&new, &new).unwrap();
        assert!(file.exists(), "a file already the tool's is left alone");
        uninstall(&new, false).unwrap();
    }

    #[test]
    fn status_reports_an_install_in_progress_from_any_process() {
        crate::recipe::store::test_root();
        let r = r6("installing-test");
        let p = paths::tool_paths(&r.name).unwrap();
        std::fs::create_dir_all(&p.data).unwrap();
        let marker = p.data.join(INSTALLING_FILE);
        let mark = |pid: u32| std::fs::write(&marker, serde_json::json!({ "phase": "downloading", "downloaded": 10, "total": 100, "pid": pid, "updatedAt": 1 }).to_string()).unwrap();
        assert!(status(&r).installing.is_none());
        mark(std::process::id());
        let st = status(&r);
        assert_eq!((st.installing.as_ref().unwrap()["phase"].as_str(), st.installing.as_ref().unwrap()["total"].as_u64()), (Some("downloading"), Some(100)));
        mark(u32::MAX - 7);
        assert!(status(&r).installing.is_none(), "a crashed install is not one");
        let _ = std::fs::remove_dir_all(&p.root);
    }

    #[test]
    fn slskd_6_renders_https_off_and_incomplete_inside_downloads_unless_chosen() {
        let r = recipe::fixtures::recipe("slskd@6");
        let mut ctx = Ctx::empty(Platform { os: "darwin", arch: "arm64" });
        ctx.data = "/d".into();
        ctx.ports = [("web".to_string(), 5030u16), ("https".to_string(), 5031u16), ("listen".to_string(), 50300u16)].into();
        ctx.config.insert("downloadsDir".into(), Value::String("/m/Soulseek".into()));
        ctx.config.insert("httpsEnabled".into(), Value::Bool(false));
        let y = render_files(&r, &ctx).unwrap().remove(0).contents;
        assert!(y.contains("  incomplete: \"/m/Soulseek/.incomplete\"\n"), "{y}");
        assert!(y.contains("  https:\n    disabled: true\n    port: 5031\n"), "{y}");

        ctx.config.insert("incompleteDir".into(), Value::String("/fast/partial".into()));
        ctx.config.insert("httpsEnabled".into(), Value::Bool(true));
        let y = render_files(&r, &ctx).unwrap().remove(0).contents;
        assert!(y.contains("  incomplete: \"/fast/partial\"\n"), "{y}");
        assert!(y.contains("  https:\n    disabled: false\n    port: 5031\n"), "{y}");
    }

    /// Something on a free local port that answers every request 401, like
    /// a slskd that is not Roadie's.
    fn foreign_on_free_port() -> u16 {
        let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for mut c in l.incoming().flatten() {
                use std::io::{Read, Write};
                let mut buf = [0u8; 2048];
                let _ = c.read(&mut buf);
                let _ = c.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            }
        });
        port
    }

    #[test]
    fn another_copy_is_found_before_install_on_its_port_and_for_a_singleton_anywhere() {
        crate::recipe::store::test_root();
        let port = foreign_on_free_port();
        let free = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port();
        let mut r = r6("other-instance-test");
        r.ports.get_mut("web").unwrap().default = port;

        let o = other_instance(&r, &Default::default()).expect("a copy that rejects Roadie's key on the port it would use");
        assert!(o.blocks_start && o.url.ends_with(&format!(":{port}")) && o.message.contains("will not start"), "{o:?}");

        // Choosing another port does not dodge a singleton: it still blocks.
        let elsewhere: std::collections::BTreeMap<String, u16> = [("web".to_string(), free)].into();
        assert!(other_instance(&r, &elsewhere).is_some_and(|o| o.blocks_start));
        // Without singleton, another port is a way out: nothing to warn about.
        r.singleton = false;
        assert!(other_instance(&r, &elsewhere).is_none());
        let o = other_instance(&r, &Default::default()).unwrap();
        assert!(!o.blocks_start && o.message.contains("choose another port"), "{o:?}");

        // Nothing there: nothing to say.
        r.ports.get_mut("web").unwrap().default = free;
        r.singleton = true;
        assert!(other_instance(&r, &Default::default()).is_none());
        assert!(dry_run(&r).unwrap().other_instance.is_none());
    }
}
