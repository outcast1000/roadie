//! The window's connection to the service: a blocking HTTP client holding
//! the bearer token (from the discovery file) and the owner token (from the
//! owner channel), plus the event pump that mirrors the service's event log
//! into the webview.

use crate::{api, owner, service};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Connection {
    pub port: u16,
    pub token: String,
    pub owner_token: Option<String>,
    pub service_version: String,
    pub service_pid: u64,
}

static CONN: OnceLock<RwLock<Option<Connection>>> = OnceLock::new();
static DATA_ROOT: OnceLock<PathBuf> = OnceLock::new();
static CLIENT_NAME: OnceLock<RwLock<String>> = OnceLock::new();

/// What this process calls itself in `X-Roadie-Client` (shown to the user
/// as "… asks to install"). The window is "Roadie window"; the CLI defaults
/// to "roadie CLI" and `--as <name>` lets a script name the real app.
pub fn set_client_name(name: &str) {
    let n = name.trim().chars().take(60).collect::<String>();
    if !n.is_empty() {
        *CLIENT_NAME.get_or_init(|| RwLock::new(String::new())).write().unwrap() = n;
    }
}

fn client_name() -> String {
    CLIENT_NAME.get_or_init(|| RwLock::new("Roadie window".into())).read().unwrap().clone()
}

fn slot() -> &'static RwLock<Option<Connection>> {
    CONN.get_or_init(|| RwLock::new(None))
}

pub fn current() -> Option<Connection> {
    slot().read().unwrap().clone()
}

/// Bring the service up (or replace a stale build), read the token, open
/// the owner channel. Idempotent; call again to reconnect.
pub fn connect(data_root: &Path) -> Result<Connection, String> {
    connect_with(data_root, true)
}

/// `owner = false` is for the CLI: a bearer client like any other, which
/// must not count as a window (it would keep the service alive and stop it
/// from opening the real window for a prompt).
pub fn connect_with(data_root: &Path, owner: bool) -> Result<Connection, String> {
    let _ = DATA_ROOT.set(data_root.to_path_buf());
    let port = service::ensure_running(data_root)?;
    let disc: Value = serde_json::from_str(&std::fs::read_to_string(api::discovery_path(data_root)).map_err(|e| format!("read discovery file: {e}"))?)
        .map_err(|e| format!("discovery file: {e}"))?;
    let token = disc.get("token").and_then(|t| t.as_str()).ok_or("discovery file has no token")?.to_string();
    let health = api::probe(data_root).ok_or("service stopped answering")?;
    let owner_token = if !owner {
        None
    } else {
        match owner::connect(data_root) {
            Ok(t) => Some(t),
            Err(e) => {
                log::error!("owner channel: {e} — approvals will be unavailable");
                None
            }
        }
    };
    let conn = Connection {
        port,
        token,
        owner_token,
        service_version: health.get("version").and_then(|v| v.as_str()).unwrap_or("?").to_string(),
        service_pid: health.get("pid").and_then(|p| p.as_u64()).unwrap_or(0),
    };
    *slot().write().unwrap() = Some(conn.clone());
    Ok(conn)
}

fn http() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder().user_agent("Roadie window").timeout(Duration::from_secs(30 * 60)).build().map_err(|e| e.to_string())
}

/// One API call. `owner = true` adds the owner token (owner routes).
/// Non-2xx replies become the service's `error` string.
pub fn call(method: &str, path: &str, body: Option<Value>, owner: bool) -> Result<Value, String> {
    let conn = current().ok_or("not connected to the Roadie service")?;
    let url = format!("http://127.0.0.1:{}{}", conn.port, path);
    let m: reqwest::Method = method.parse().map_err(|_| format!("bad method {method}"))?;
    let mut req = http()?.request(m, &url).bearer_auth(&conn.token).header("X-Roadie-Client", client_name());
    if owner {
        let t = conn.owner_token.as_deref().ok_or("the window is not connected to the owner channel; approvals are unavailable")?;
        req = req.header(owner::HEADER, t);
    }
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = req.send().map_err(|e| {
        let msg = crate::recipe::httpsteps::err_chain(&e);
        // A vanished service: drop the connection so the pump reconnects.
        if e.is_connect() {
            *slot().write().unwrap() = None;
        }
        format!("Roadie service: {msg}")
    })?;
    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    let json: Value = if text.is_empty() { Value::Null } else { serde_json::from_str(&text).unwrap_or(Value::String(text)) };
    if status.is_success() {
        Ok(json)
    } else {
        Err(json.get("error").and_then(|e| e.as_str()).map(str::to_string).unwrap_or_else(|| format!("HTTP {status}")))
    }
}

/// Mirror the service's event log into the webview, forever. Reconnects
/// (restarting the service if needed) when the poll fails.
pub fn run_event_pump(emit: impl Fn(&str, Value) + Send + 'static) {
    std::thread::Builder::new()
        .name("event-pump".into())
        .spawn(move || {
            let mut since: Option<u64> = None;
            let mut announced_down = false;
            loop {
                let Some(conn) = current() else {
                    if !announced_down {
                        emit("service-changed", serde_json::json!({ "connected": false }));
                        announced_down = true;
                    }
                    if let Some(root) = DATA_ROOT.get() {
                        match connect(root) {
                            Ok(c) => {
                                emit("service-changed", serde_json::json!({ "connected": true, "version": c.service_version, "pid": c.service_pid }));
                                announced_down = false;
                                since = None;
                                // Everything may have changed while we were away.
                                emit("tool-status-changed", serde_json::json!({ "name": "*" }));
                                emit("recipe-changed", serde_json::json!({}));
                            }
                            Err(e) => {
                                log::warn!("service reconnect: {e}");
                                std::thread::sleep(Duration::from_secs(3));
                            }
                        }
                    }
                    continue;
                };
                let url = format!("http://127.0.0.1:{}/v1/events?wait=25{}", conn.port, since.map(|s| format!("&since={s}")).unwrap_or_default());
                let resp = http().and_then(|c| c.get(&url).bearer_auth(&conn.token).send().map_err(|e| crate::recipe::httpsteps::err_chain(&e)));
                match resp.and_then(|r| r.json::<Value>().map_err(|e| e.to_string())) {
                    Ok(v) => {
                        if let Some(latest) = v.get("latest").and_then(|l| l.as_u64()) {
                            since = Some(latest);
                        }
                        for ev in v.get("events").and_then(|e| e.as_array()).cloned().unwrap_or_default() {
                            let name = ev.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                            emit(&name, ev.get("payload").cloned().unwrap_or(Value::Null));
                        }
                    }
                    Err(e) => {
                        log::warn!("event poll failed: {e}");
                        *slot().write().unwrap() = None;
                        std::thread::sleep(Duration::from_millis(500));
                    }
                }
            }
        })
        .ok();
}
