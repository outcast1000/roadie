//! The local API: what other apps and AI assistants talk to.
//!
//! Bound to `127.0.0.1` on a fixed port (47630, falling back through 47639)
//! so a client that cannot read files can still find it. Two tiers:
//!
//! - **Public read** (no token): health, tool status, and a consumer's
//!   connection details once the user approved that consumer in Roadie.
//! - **Bearer** (`Authorization: Bearer <token>`, token in the 0600 discovery
//!   file `roadie-api.json`): start/stop/configure, recipe authoring, and
//!   *requests* — install/uninstall never happen on an API call; they queue
//!   a prompt the user answers in the window.
//!
//! No CORS ever (`OPTIONS` → 405, no `Access-Control-*`), and the `Host`
//! header must be loopback, so a web page cannot use a visitor's browser as a
//! proxy into it. The token is compared through SHA-256 digests.

use crate::recipe::{self, store, Recipe};
use crate::{actions, consent, events, owner, paths, requests, service, tools};
use axum::{
    body::Bytes,
    extract::{Path as AxumPath, Query, Request, State},
    http::{header, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const DISCOVERY_FILE: &str = "roadie-api.json";
pub const DEFAULT_PORT: u16 = 47630;
pub const PORT_RANGE: u16 = 10;
pub const API_VERSION: u32 = 1;

pub const SCHEMA_MD: &str = include_str!("../../../recipes/SCHEMA.md");

#[derive(Clone)]
pub struct ApiState {
    pub token: Arc<String>,
    pub version: String,
    /// Changes with the executable; the window replaces a service whose
    /// build differs from its own.
    pub build_id: String,
}

// --- Token + discovery ---

fn read_discovery_token(dir: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(dir.join(DISCOVERY_FILE)).ok()?;
    let json: Value = serde_json::from_str(&contents).ok()?;
    let token = json.get("token")?.as_str()?;
    (token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit())).then(|| token.to_string())
}

fn write_discovery_file(dir: &Path, port: u16, token: &str, version: &str) -> Result<(), String> {
    let contents = serde_json::to_string_pretty(&json!({
        "app": "roadie",
        "version": version,
        "apiVersion": API_VERSION,
        "port": port,
        "pid": std::process::id(),
        "token": token,
        "startedAt": chrono::Utc::now().to_rfc3339(),
    }))
    .map_err(|e| e.to_string())?;
    paths::write_atomic(&dir.join(DISCOVERY_FILE), contents.as_bytes(), true)
}

pub fn discovery_path(dir: &Path) -> PathBuf {
    dir.join(DISCOVERY_FILE)
}

fn read_discovery(dir: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(dir.join(DISCOVERY_FILE)).ok()?).ok()
}

fn client() -> Option<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder().user_agent("Roadie").timeout(std::time::Duration::from_millis(1500)).build().ok()
}

/// Is a service answering for this data dir? Its health, with `port` added.
pub fn probe(data_root: &Path) -> Option<Value> {
    let disc = read_discovery(data_root)?;
    let port = disc.get("port")?.as_u64()? as u16;
    let mut health: Value = client()?.get(format!("http://127.0.0.1:{port}/v1/health")).send().ok()?.json().ok()?;
    if health.get("app")?.as_str()? != "roadie" {
        return None;
    }
    health.as_object_mut()?.insert("port".into(), Value::from(port));
    Some(health)
}

/// A bearer POST from another process on this machine (the window's
/// handoff before an owner token exists).
pub fn bearer_post(data_root: &Path, path: &str) -> Result<Value, String> {
    let disc = read_discovery(data_root).ok_or("no discovery file")?;
    let port = disc.get("port").and_then(|p| p.as_u64()).ok_or("no port in discovery file")?;
    let token = disc.get("token").and_then(|t| t.as_str()).ok_or("no token in discovery file")?;
    let resp = client()
        .ok_or("http client")?
        .post(format!("http://127.0.0.1:{port}{path}"))
        .bearer_auth(token)
        .send()
        .map_err(|e| recipe::httpsteps::err_chain(&e))?;
    let status = resp.status();
    let body: Value = resp.json().unwrap_or(Value::Null);
    if status.is_success() {
        Ok(body)
    } else {
        Err(body.get("error").and_then(|e| e.as_str()).map(str::to_string).unwrap_or_else(|| format!("HTTP {status}")))
    }
}

/// Start the server on the first free port of the range and write the
/// discovery file. Returns the port. Must be called inside a tokio runtime.
pub fn start(data_root: &Path, version: String, build_id: String) -> Result<u16, String> {
    let token = read_discovery_token(data_root).map(Ok).unwrap_or_else(|| paths::random_hex(32))?;
    let mut bound = None;
    for port in DEFAULT_PORT..DEFAULT_PORT + PORT_RANGE {
        if let Ok(l) = std::net::TcpListener::bind(("127.0.0.1", port)) {
            bound = Some((port, l));
            break;
        }
    }
    let (port, std_listener) = bound.ok_or_else(|| format!("no free port in {DEFAULT_PORT}–{}", DEFAULT_PORT + PORT_RANGE - 1))?;
    std_listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let state = ApiState { token: Arc::new(token.clone()), version: version.clone(), build_id };
    let router = build_router(state);
    tokio::spawn(async move {
        let listener = match tokio::net::TcpListener::from_std(std_listener) {
            Ok(l) => l,
            Err(e) => {
                log::error!("API listener handoff failed: {e}");
                return;
            }
        };
        if let Err(e) = axum::serve(listener, router).await {
            log::error!("API server error: {e}");
        }
    });
    write_discovery_file(data_root, port, &token, &version)?;
    log::info!("local API listening on 127.0.0.1:{port}");
    Ok(port)
}

// --- Router ---

fn err(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

fn err_with(status: StatusCode, message: impl Into<String>, extra: Value) -> Response {
    let mut body = json!({ "error": message.into() });
    if let (Some(b), Some(e)) = (body.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            b.insert(k.clone(), v.clone());
        }
    }
    (status, Json(body)).into_response()
}

fn requested_by(req: &Request) -> String {
    req.headers()
        .get("x-roadie-client")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.chars().take(60).collect())
        .filter(|s: &String| !s.trim().is_empty())
        .unwrap_or_else(|| "a local API client".to_string())
}

pub fn build_router(state: ApiState) -> Router {
    let public = Router::new()
        .route("/v1/health", get(health))
        .route("/v1/tools", get(list_tools))
        .route("/v1/tools/{name}", get(get_tool).delete(uninstall_tool))
        .route("/v1/tools/{name}/connection", get(get_connection));

    let bearer = Router::new()
        .route("/v1/tools/{name}/start", post(start_tool))
        .route("/v1/tools/{name}/stop", post(stop_tool))
        .route("/v1/tools/{name}/restart", post(restart_tool))
        .route("/v1/tools/{name}/update", post(update_tool))
        .route("/v1/tools/{name}/autostart", post(set_autostart))
        .route("/v1/tools/{name}/config", axum::routing::patch(patch_config))
        .route("/v1/tools/{name}/install", post(install_tool))
        .route("/v1/tools/{name}/check-updates", post(check_updates_tool))
        .route("/v1/tools/{name}/logs", get(tool_logs))
        .route("/v1/events", get(list_events))
        .route("/v1/shutdown", post(shutdown))
        .route("/v1/requests", get(list_requests))
        .route("/v1/requests/{id}", get(get_request))
        .route("/v1/recipes", get(list_recipes))
        .route("/v1/recipes/schema", get(recipe_schema))
        .route("/v1/recipes/validate", post(validate_recipe))
        .route("/v1/recipes/{name}", get(get_recipe).put(put_recipe).delete(delete_recipe))
        .route("/v1/recipes/{name}/dryrun", post(dryrun_recipe))
        .route("/v1/consumers", get(list_consumers).post(register_consumer))
        .route("/v1/consumers/{id}", delete(revoke_consumer))
        .layer(middleware::from_fn_with_state(state.clone(), require_bearer));

    // Owner tier: the user's own clicks, relayed by the window. Only a
    // process that proved it runs the Roadie binary holds an owner token
    // (`owner.rs`), so these are the routes no other local program can call.
    let owner = Router::new()
        .route("/v1/owner/requests/{id}/decide", post(owner_decide))
        .route("/v1/owner/recipes/{name}/trust", post(owner_trust))
        .route("/v1/owner/tools/{name}/install", post(owner_install))
        .route("/v1/owner/tools/{name}", delete(owner_uninstall))
        .route("/v1/owner/intent", post(owner_intent))
        .route("/v1/owner/settings", get(owner_get_settings).put(owner_put_settings))
        .layer(middleware::from_fn(require_owner));

    Router::new()
        .merge(public)
        .merge(bearer)
        .merge(owner)
        .layer(middleware::from_fn(host_and_method_guard))
        .with_state(state)
}

async fn require_owner(req: Request, next: Next) -> Response {
    let ok = req.headers().get(owner::HEADER).and_then(|h| h.to_str().ok()).is_some_and(owner::is_owner);
    if !ok {
        return err(StatusCode::FORBIDDEN, "owner routes are for Roadie's own window; connect to the owner channel first");
    }
    next.run(req).await
}

/// Reject non-loopback `Host` headers (DNS rebinding) and any preflight.
async fn host_and_method_guard(req: Request, next: Next) -> Response {
    if req.method() == Method::OPTIONS {
        return err(StatusCode::METHOD_NOT_ALLOWED, "no CORS");
    }
    let host_ok = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(|h| {
            let h = h.trim_end_matches('.');
            let name = h.rsplit_once(':').map(|(n, _)| n).unwrap_or(h);
            matches!(name, "127.0.0.1" | "localhost" | "[::1]")
        })
        .unwrap_or(false);
    if !host_ok {
        return err(StatusCode::BAD_REQUEST, "Host must be 127.0.0.1 or localhost");
    }
    next.run(req).await
}

async fn require_bearer(State(state): State<ApiState>, req: Request, next: Next) -> Response {
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(str::trim);
    let ok = presented.is_some_and(|t| Sha256::digest(t.as_bytes()) == Sha256::digest(state.token.as_bytes()));
    if !ok {
        return err(StatusCode::UNAUTHORIZED, "missing or invalid bearer token; read it from roadie-api.json");
    }
    next.run(req).await
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tokio::task::spawn_blocking(f).await.map_err(|e| format!("task failed: {e}"))?
}

fn trusted(name: &str) -> Result<Recipe, Response> {
    store::get_trusted(name).map_err(|e| {
        if e.starts_with("unknown") {
            err(StatusCode::NOT_FOUND, e)
        } else {
            err(StatusCode::CONFLICT, e)
        }
    })
}

fn public_status(s: tools::ToolStatus, stored: &store::Stored) -> Value {
    let mut v = serde_json::to_value(s).unwrap_or_default();
    if let Some(o) = v.as_object_mut() {
        o.insert("origin".into(), serde_json::to_value(stored.origin).unwrap_or_default());
        o.insert("trusted".into(), Value::Bool(stored.trusted()));
        o.remove("pid");
    }
    v
}

// --- Public ---

async fn health(State(state): State<ApiState>) -> Response {
    Json(json!({
        "app": "roadie", "version": state.version, "apiVersion": API_VERSION,
        "buildId": state.build_id, "pid": std::process::id(), "role": "service",
        "windowConnected": owner::owners_connected() > 0,
    }))
    .into_response()
}

async fn list_tools() -> Response {
    match blocking(|| {
        Ok(store::list()
            .into_iter()
            .map(|s| {
                let st = tools::status(&s.recipe);
                public_status(st, &s)
            })
            .collect::<Vec<_>>())
    })
    .await
    {
        Ok(list) => Json(list).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn get_tool(AxumPath(name): AxumPath<String>) -> Response {
    let Some(stored) = store::get(&name) else { return err(StatusCode::NOT_FOUND, format!("unknown tool: {name}")) };
    match blocking(move || Ok(public_status(tools::status(&stored.recipe), &stored))).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

/// Connection details. Public callers must name an approved consumer; a
/// bearer holder gets the internal key.
async fn get_connection(State(state): State<ApiState>, AxumPath(name): AxumPath<String>, Query(q): Query<HashMap<String, String>>, req: Request) -> Response {
    let recipe = match trusted(&name) {
        Ok(r) => r,
        Err(r) => return r,
    };
    let policy = recipe.connection.as_ref().map(|c| c.policy).unwrap_or(recipe::ConnectionPolicy::None);
    if policy == recipe::ConnectionPolicy::None {
        return err(StatusCode::NOT_FOUND, format!("{} exposes no connection", recipe.display_name));
    }
    let bearer_ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .is_some_and(|t| Sha256::digest(t.trim().as_bytes()) == Sha256::digest(state.token.as_bytes()));
    let consumer = q.get("consumer").cloned();
    let result = blocking(move || {
        let st = tools::status(&recipe);
        let url = st.url.clone().ok_or("tool has no connection URL")?;
        if let Some(id) = consumer {
            if !consent::is_valid_id(&id) {
                return Err("bad consumer id".to_string());
            }
            match consent::grant(&id, &recipe.name) {
                Some(g) => Ok(json!({ "url": url, "apiKey": if g.key.is_empty() { Value::Null } else { Value::String(g.key) }, "policy": policy, "running": st.running, "healthy": st.healthy })),
                None => {
                    // Unknown consumer ids are not auto-registered: a name is
                    // not a credential. Known ones queue a consent prompt.
                    if consent::get(&id).is_some() {
                        requests::create(requests::RequestKind::Connect { consumer: id.clone(), tool: recipe.name.clone(), return_url: None }, &consent::get(&id).map(|r| r.display_name).unwrap_or(id.clone()));
                        Err("consent-required".to_string())
                    } else {
                        Err("unknown-consumer".to_string())
                    }
                }
            }
        } else if bearer_ok {
            let p = paths::tool_paths(&recipe.name)?;
            let s = tools::state::load(&p.data);
            let key = s.secrets.get("internalKey").cloned();
            Ok(json!({ "url": url, "apiKey": key, "policy": policy, "running": st.running, "healthy": st.healthy }))
        } else {
            Err("consumer-required".to_string())
        }
    })
    .await;
    match result {
        Ok(v) => Json(v).into_response(),
        Err(e) if e == "consent-required" => err_with(StatusCode::FORBIDDEN, "the user has not approved this consumer for this tool yet — ask them to approve it in Roadie", json!({ "reason": "consent-required" })),
        Err(e) if e == "unknown-consumer" => err_with(StatusCode::FORBIDDEN, "unknown consumer; register it via POST /v1/consumers (bearer) first", json!({ "reason": "unknown-consumer" })),
        Err(e) if e == "consumer-required" => err(StatusCode::UNAUTHORIZED, "pass ?consumer=<id> or a bearer token"),
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

// --- Bearer: tool control ---

async fn tool_action(name: String, f: impl FnOnce(&Recipe) -> Result<tools::ToolStatus, String> + Send + 'static) -> Response {
    let recipe = match trusted(&name) {
        Ok(r) => r,
        Err(r) => return r,
    };
    let stored = store::get(&name).expect("trusted implies stored");
    let result = blocking(move || f(&recipe)).await;
    events::tool_changed(&name);
    match result {
        Ok(st) => Json(public_status(st, &stored)).into_response(),
        Err(e) => err(StatusCode::CONFLICT, e),
    }
}

async fn start_tool(AxumPath(name): AxumPath<String>) -> Response {
    tool_action(name, |r| tools::start(r, "api")).await
}
async fn stop_tool(AxumPath(name): AxumPath<String>) -> Response {
    tool_action(name, tools::stop).await
}
async fn restart_tool(AxumPath(name): AxumPath<String>) -> Response {
    tool_action(name, tools::restart).await
}
/// Fetch and stage the latest release; applies when idle. Allowed without
/// a prompt because the tool is already installed by the user's choice.
async fn update_tool(AxumPath(name): AxumPath<String>) -> Response {
    tool_action(name, |r| {
        let st = tools::status(r);
        if !st.installed {
            return Err(format!("{} is not installed", r.display_name));
        }
        tools::check_updates(r)?;
        tools::install(r, &mut |_, _, _| {})
    })
    .await
}
async fn check_updates_tool(AxumPath(name): AxumPath<String>) -> Response {
    tool_action(name, tools::check_updates).await
}

/// Long-poll the event log: `?since=<seq>&wait=<sec>` (wait ≤ 30, default
/// 25). Answers `{ events: [...], latest: <seq> }` as soon as anything newer
/// than `since` exists, or empty when the wait passes.
async fn list_events(Query(q): Query<HashMap<String, String>>) -> Response {
    let since = q.get("since").and_then(|s| s.parse().ok()).unwrap_or_else(events::latest_seq);
    let wait = q.get("wait").and_then(|w| w.parse::<u64>().ok()).unwrap_or(25).min(30);
    let found = if wait == 0 { events::since(since) } else { events::wait_since(since, std::time::Duration::from_secs(wait)).await };
    Json(json!({ "events": found, "latest": events::latest_seq() })).into_response()
}

/// Stop the service. Bearer, not owner, because the window calls it before
/// it can hold an owner token (replacing a stale build); the worst a local
/// program can do with it is what it could do with `kill`.
async fn shutdown() -> Response {
    service::request_shutdown();
    Json(json!({ "stopping": true })).into_response()
}

// --- Owner ---

async fn owner_decide(AxumPath(id): AxumPath<String>, body: Bytes) -> Response {
    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let approve = v.get("approve").and_then(|a| a.as_bool()).unwrap_or(false);
    let answers = v.get("answers").and_then(|a| a.as_object()).cloned();
    match blocking(move || actions::decide(&id, approve, answers)).await {
        Ok(r) => Json(r).into_response(),
        Err(e) => err(StatusCode::CONFLICT, e),
    }
}

async fn owner_trust(AxumPath(name): AxumPath<String>) -> Response {
    match blocking(move || actions::trust(&name)).await {
        Ok(s) => Json(s).into_response(),
        Err(e) => err(StatusCode::CONFLICT, e),
    }
}

/// The user's Install click on a card: immediate, with the decisions typed
/// into the prompt. Progress goes to the event log.
async fn owner_install(AxumPath(name): AxumPath<String>, body: Bytes) -> Response {
    let decisions = match serde_json::from_slice::<Value>(&body) {
        Ok(Value::Object(m)) => m.get("config").and_then(|c| c.as_object()).cloned().unwrap_or(m),
        Ok(Value::Null) | Err(_) if body.is_empty() => Map::new(),
        _ => return err(StatusCode::BAD_REQUEST, "body must be a JSON object of decisions"),
    };
    let stored = match store::get(&name) {
        Some(s) => s,
        None => return err(StatusCode::NOT_FOUND, format!("unknown tool: {name}")),
    };
    match blocking(move || actions::install_now(&name, &decisions)).await {
        Ok(st) => Json(public_status(st, &stored)).into_response(),
        Err(e) => err(StatusCode::CONFLICT, e),
    }
}

async fn owner_uninstall(AxumPath(name): AxumPath<String>, Query(q): Query<HashMap<String, String>>) -> Response {
    let keep_data = q.get("keepData").map(|v| v == "true" || v == "1").unwrap_or(false);
    match blocking(move || actions::uninstall_now(&name, keep_data)).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err(StatusCode::CONFLICT, e),
    }
}

async fn owner_intent(body: Bytes) -> Response {
    let url = serde_json::from_slice::<Value>(&body).ok().and_then(|v| v.get("url").and_then(|u| u.as_str()).map(str::to_string));
    let Some(url) = url else { return err(StatusCode::BAD_REQUEST, "body must be { \"url\": \"roadie://…\" }") };
    match blocking(move || actions::intent(&url)).await {
        Ok(i) => Json(i).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

async fn owner_get_settings() -> Response {
    Json(actions::load_settings()).into_response()
}

async fn owner_put_settings(body: Bytes) -> Response {
    let settings: actions::Settings = match serde_json::from_slice(&body) {
        Ok(s) => s,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("settings: {e}")),
    };
    match blocking(move || actions::save_settings(&settings)).await {
        Ok(s) => Json(s).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn set_autostart(AxumPath(name): AxumPath<String>, body: Bytes) -> Response {
    let enabled = serde_json::from_slice::<Value>(&body).ok().and_then(|v| v.get("enabled").and_then(|e| e.as_bool()));
    let Some(enabled) = enabled else { return err(StatusCode::BAD_REQUEST, "body must be {\"enabled\": true|false}") };
    tool_action(name, move |r| tools::set_autostart(r, enabled)).await
}
async fn patch_config(AxumPath(name): AxumPath<String>, body: Bytes) -> Response {
    let patch = match serde_json::from_slice::<Value>(&body) {
        Ok(Value::Object(m)) => m.get("patch").and_then(|p| p.as_object()).cloned().unwrap_or(m),
        _ => return err(StatusCode::BAD_REQUEST, "body must be a JSON object of config values"),
    };
    tool_action(name, move |r| tools::configure(r, &patch)).await
}
async fn tool_logs(AxumPath(name): AxumPath<String>, Query(q): Query<HashMap<String, String>>) -> Response {
    let recipe = match trusted(&name) {
        Ok(r) => r,
        Err(r) => return r,
    };
    let lines = q.get("lines").and_then(|l| l.parse().ok()).unwrap_or(100usize).min(2000);
    match blocking(move || tools::log_tail(&recipe, lines)).await {
        Ok(text) => Json(json!({ "lines": text.lines().collect::<Vec<_>>() })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

// --- Bearer: requests ---

/// `POST /v1/tools/{name}/install` with an optional body `{ "config": {…} }`
/// (or the bare object): the caller's decisions for the recipe's
/// `askOnInstall` fields. They are validated now, shown to the user in the
/// approval prompt, and applied when the user approves. The reply lists
/// every decision the recipe asks for and whether it is settled.
async fn install_tool(AxumPath(name): AxumPath<String>, req: Request) -> Response {
    let by = requested_by(&req);
    let recipe = match trusted(&name) {
        Ok(r) => r,
        Err(r) => return r,
    };
    let body = match axum::body::to_bytes(req.into_body(), 1 << 20).await {
        Ok(b) => b,
        Err(e) => return err(StatusCode::BAD_REQUEST, e.to_string()),
    };
    let values: Map<String, Value> = if body.is_empty() {
        Map::new()
    } else {
        match serde_json::from_slice::<Value>(&body) {
            Ok(Value::Object(m)) => m.get("config").and_then(|c| c.as_object()).cloned().unwrap_or(m),
            _ => return err(StatusCode::BAD_REQUEST, "body must be a JSON object: { \"config\": { \"<key>\": <value> } }"),
        }
    };
    let mut config_only = values.clone();
    if let Err(e) = tools::install_options(&recipe, &mut config_only) {
        return err(StatusCode::UNPROCESSABLE_ENTITY, format!("config: {e}"));
    }
    if let Err(e) = tools::state::validate_patch(&recipe, &config_only) {
        return err(StatusCode::UNPROCESSABLE_ENTITY, format!("config: {e}"));
    }
    let current = tools::status(&recipe).config;
    let mut decisions: Vec<Value> = recipe
        .install_fields()
        .iter()
        .map(|f| {
            let settled = values.contains_key(&f.key)
                || current.get(&f.key).is_some_and(|v| !v.is_null() && v.as_str() != Some(""))
                || current.get(&format!("has_{}", f.key)) == Some(&Value::Bool(true));
            json!({ "key": f.key, "label": f.label, "kind": f.kind, "required": f.required, "help": f.help, "settled": settled })
        })
        .collect();
    for (key, label, offer) in [
        ("startNow", "Start now, right after installing", recipe.start_after_install),
        ("autostart", "Start at login", recipe.autostart),
    ] {
        if let Some(o) = offer {
            decisions.push(json!({
                "key": key, "label": label, "kind": "bool", "required": false, "default": o.default,
                "settled": true, "value": values.get(key).and_then(|v| v.as_bool()).unwrap_or(o.default),
            }));
        }
    }
    let r = requests::create(requests::install_kind(&recipe, values), &by);
    crate::scheme::focus_if_possible();
    (
        StatusCode::ACCEPTED,
        Json(json!({
            "requestId": r.id, "status": r.status, "decisions": decisions,
            "hint": "the user must approve this in Roadie and can change or fill in any decision there; poll GET /v1/requests/{id}"
        })),
    )
        .into_response()
}

async fn uninstall_tool(State(state): State<ApiState>, AxumPath(name): AxumPath<String>, Query(q): Query<HashMap<String, String>>, req: Request) -> Response {
    // DELETE lives on the public router path but still needs the token.
    let ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .is_some_and(|t| Sha256::digest(t.trim().as_bytes()) == Sha256::digest(state.token.as_bytes()));
    if !ok {
        return err(StatusCode::UNAUTHORIZED, "missing or invalid bearer token");
    }
    let by = requested_by(&req);
    if let Err(r) = trusted(&name) {
        return r;
    }
    let keep_data = q.get("keepData").map(|v| v == "true" || v == "1").unwrap_or(false);
    let r = requests::create(requests::RequestKind::Uninstall { tool: name, keep_data }, &by);
    crate::scheme::focus_if_possible();
    (StatusCode::ACCEPTED, Json(json!({ "requestId": r.id, "status": r.status }))).into_response()
}

async fn list_requests() -> Response {
    Json(requests::pending()).into_response()
}

async fn get_request(AxumPath(id): AxumPath<String>) -> Response {
    match requests::get(&id) {
        Some(r) => Json(r).into_response(),
        None => err(StatusCode::NOT_FOUND, "unknown request"),
    }
}

// --- Bearer: recipes ---

async fn list_recipes(Query(q): Query<HashMap<String, String>>) -> Response {
    if q.get("full").is_some_and(|f| f == "true" || f == "1") {
        return Json(store::list()).into_response();
    }
    let list: Vec<Value> = store::list()
        .into_iter()
        .map(|s| {
            json!({
                "name": s.recipe.name, "displayName": s.recipe.display_name, "kind": s.recipe.kind,
                "author": s.recipe.author, "revision": s.recipe.revision, "platforms": s.recipe.platforms,
                "summary": s.recipe.summary, "origin": s.origin, "trusted": s.trusted(),
                "submittedBy": s.submitted_by, "supported": s.recipe.supported_on(&recipe::Platform::current()),
            })
        })
        .collect();
    Json(list).into_response()
}

async fn recipe_schema() -> Response {
    Json(json!({
        "recipeVersion": recipe::RECIPE_VERSION,
        "platform": recipe::Platform::current().key(),
        "platforms": recipe::PLATFORMS,
        "schema": SCHEMA_MD,
        "example": serde_json::from_str::<Value>(recipe::BUILTIN[0].1).unwrap_or_default(),
    }))
    .into_response()
}

fn recipe_body(body: &Bytes) -> Result<Value, Response> {
    let v: Value = serde_json::from_slice(body).map_err(|e| err(StatusCode::BAD_REQUEST, format!("body is not JSON: {e}")))?;
    Ok(v.get("recipe").cloned().unwrap_or(v))
}

async fn validate_recipe(body: Bytes) -> Response {
    let v = match recipe_body(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    match recipe::from_value(v) {
        Ok(r) => Json(json!({ "ok": true, "name": r.name, "supported": r.supported_on(&recipe::Platform::current()), "errors": [] })).into_response(),
        Err(errors) => Json(json!({ "ok": false, "errors": errors })).into_response(),
    }
}

async fn get_recipe(AxumPath(name): AxumPath<String>) -> Response {
    match store::get(&name) {
        Some(s) => Json(s).into_response(),
        None => err(StatusCode::NOT_FOUND, format!("unknown recipe: {name}")),
    }
}

async fn put_recipe(AxumPath(name): AxumPath<String>, req: Request) -> Response {
    let by = requested_by(&req);
    let body = match axum::body::to_bytes(req.into_body(), 4 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => return err(StatusCode::BAD_REQUEST, e.to_string()),
    };
    let v = match recipe_body(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let recipe = match recipe::from_value(v) {
        Ok(r) => r,
        Err(errors) => return (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({ "error": "recipe is invalid", "errors": errors }))).into_response(),
    };
    if recipe.name != name {
        return err(StatusCode::BAD_REQUEST, format!("URL names {name} but the recipe is {}", recipe.name));
    }
    match blocking(move || Ok(store::put_draft(recipe, Some(by)))).await {
        Ok(Ok(stored)) => {
            events::emit("recipe-changed", json!({ "name": stored.recipe.name, "origin": stored.origin }));
            crate::scheme::focus_if_possible();
            (StatusCode::CREATED, Json(json!({ "name": stored.recipe.name, "origin": stored.origin, "trusted": false,
                "hint": "saved as a draft; ask the user to review and Trust it in Roadie before install" }))).into_response()
        }
        Ok(Err(store::PutError::Invalid(errors))) => (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({ "error": "recipe is invalid", "errors": errors }))).into_response(),
        Ok(Err(store::PutError::Conflict(m))) => err(StatusCode::CONFLICT, m),
        Ok(Err(store::PutError::Io(m))) | Err(m) => err(StatusCode::INTERNAL_SERVER_ERROR, m),
    }
}

async fn delete_recipe(AxumPath(name): AxumPath<String>) -> Response {
    match blocking(move || {
        if let Some(s) = store::get(&name) {
            if tools::status(&s.recipe).installed {
                return Err(format!("{} is installed; uninstall it first", s.recipe.display_name));
            }
        }
        store::delete(&name)?;
        Ok(name)
    })
    .await
    {
        Ok(name) => {
            events::emit("recipe-changed", json!({ "name": name, "deleted": true }));
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => err(StatusCode::CONFLICT, e),
    }
}

/// Resolve the release and render the files without downloading or writing.
/// Body may carry an inline `{recipe}` to dry-run something unsaved.
async fn dryrun_recipe(AxumPath(name): AxumPath<String>, body: Bytes) -> Response {
    let inline = if body.is_empty() { None } else { Some(recipe_body(&body)) };
    let recipe = match inline {
        Some(Ok(v)) => match recipe::from_value(v) {
            Ok(r) => r,
            Err(errors) => return (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({ "error": "recipe is invalid", "errors": errors }))).into_response(),
        },
        Some(Err(r)) => return r,
        None => match store::get(&name) {
            Some(s) => s.recipe,
            None => return err(StatusCode::NOT_FOUND, format!("unknown recipe: {name}")),
        },
    };
    match blocking(move || tools::dry_run(&recipe)).await {
        Ok(d) => Json(d).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

// --- Bearer: consumers ---

async fn list_consumers() -> Response {
    Json(consent::list()).into_response()
}

async fn register_consumer(body: Bytes) -> Response {
    let v: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("body is not JSON: {e}")),
    };
    let id = v.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
    let display = v.get("displayName").and_then(|d| d.as_str()).unwrap_or("").to_string();
    let prefix = v.get("returnPrefix").and_then(|p| p.as_str()).map(str::to_string);
    match blocking(move || consent::register(&id, &display, prefix.as_deref())).await {
        Ok(c) => (StatusCode::CREATED, Json(c)).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

async fn revoke_consumer(AxumPath(id): AxumPath<String>, Query(q): Query<HashMap<String, String>>) -> Response {
    let tool = q.get("tool").cloned();
    match blocking(move || {
        let affected: Vec<String> = match &tool {
            Some(t) => vec![t.clone()],
            None => consent::get(&id).map(|r| r.tools.keys().cloned().collect()).unwrap_or_default(),
        };
        consent::revoke(&id, tool.as_deref())?;
        for t in &affected {
            if let Ok(r) = store::get_trusted(t) {
                let _ = tools::refresh_consumers(&r);
                events::tool_changed(t);
            }
        }
        Ok(())
    })
    .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

// --- Public helper for other modules ---

/// `PATCH /config` etc. work on a `Map`; keep the type visible for commands.
pub type ConfigPatch = Map<String, Value>;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    const TOKEN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn setup() -> Router {
        let root = std::env::temp_dir().join(format!("roadie-api-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&root);
        paths::init(root);
        store::load_all();
        build_router(ApiState { token: Arc::new(TOKEN.into()), version: "0.0.0-test".into(), build_id: "test-build".into() })
    }

    fn owner_req(method: &str, path: &str, owner_token: Option<&str>, body: Option<&str>) -> HttpRequest<Body> {
        let mut r = req(method, path, None, body);
        if let Some(t) = owner_token {
            r.headers_mut().insert(owner::HEADER, t.parse().unwrap());
        }
        r
    }

    fn req(method: &str, path: &str, token: Option<&str>, body: Option<&str>) -> HttpRequest<Body> {
        let mut b = HttpRequest::builder().method(method).uri(path).header("Host", "127.0.0.1:47630");
        if let Some(t) = token {
            b = b.header("Authorization", format!("Bearer {t}"));
        }
        if body.is_some() {
            b = b.header("Content-Type", "application/json");
        }
        b.body(body.map(|s| Body::from(s.to_string())).unwrap_or_else(Body::empty)).unwrap()
    }

    async fn json_of(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    }

    #[tokio::test]
    async fn public_reads_work_without_a_token() {
        let app = setup();
        let resp = app.clone().oneshot(req("GET", "/v1/health", None, None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = json_of(resp).await;
        assert_eq!(v["app"], "roadie");
        assert!(resp_headers_have_no_cors(&app).await);

        let resp = app.clone().oneshot(req("GET", "/v1/tools", None, None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let list = json_of(resp).await;
        assert!(list.as_array().unwrap().iter().any(|t| t["name"] == "slskd" && t["trusted"] == true && t.get("pid").is_none()));

        let resp = app.clone().oneshot(req("GET", "/v1/tools/nope", None, None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    async fn resp_headers_have_no_cors(app: &Router) -> bool {
        let resp = app.clone().oneshot(req("GET", "/v1/health", None, None)).await.unwrap();
        !resp.headers().keys().any(|k| k.as_str().starts_with("access-control"))
    }

    #[tokio::test]
    async fn host_check_and_options_are_enforced() {
        let app = setup();
        let bad = HttpRequest::builder().method("GET").uri("/v1/health").header("Host", "evil.test").body(Body::empty()).unwrap();
        assert_eq!(app.clone().oneshot(bad).await.unwrap().status(), StatusCode::BAD_REQUEST);
        let opt = req("OPTIONS", "/v1/tools", None, None);
        assert_eq!(app.clone().oneshot(opt).await.unwrap().status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn bearer_routes_need_the_right_token() {
        let app = setup();
        let resp = app.clone().oneshot(req("POST", "/v1/tools/slskd/stop", None, None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let resp = app.clone().oneshot(req("POST", "/v1/tools/slskd/stop", Some("bbbb"), None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let resp = app.clone().oneshot(req("DELETE", "/v1/tools/slskd", None, None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "DELETE on the public path still needs the token");
        let resp = app.clone().oneshot(req("GET", "/v1/recipes/schema", Some(TOKEN), None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = json_of(resp).await;
        assert!(v["schema"].as_str().unwrap().contains("recipeVersion"));
        assert_eq!(v["example"]["name"], "slskd");
    }

    #[tokio::test]
    async fn install_is_a_request_not_an_action() {
        let app = setup();
        let resp = app.clone().oneshot(req("POST", "/v1/tools/slskd/install", Some(TOKEN), None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let v = json_of(resp).await;
        let id = v["requestId"].as_str().unwrap().to_string();
        assert_eq!(v["status"], "pending");
        let resp = app.clone().oneshot(req("GET", &format!("/v1/requests/{id}"), Some(TOKEN), None)).await.unwrap();
        let r = json_of(resp).await;
        assert_eq!(r["kind"], "install");
        assert_eq!(r["tool"], "slskd");
        assert_eq!(r["status"], "pending");
    }

    #[tokio::test]
    async fn owner_routes_need_a_channel_token_and_decide_a_request() {
        let app = setup();
        // A bearer token is not enough: same-user programs hold one.
        let mut r = req("POST", "/v1/owner/recipes/slskd/trust", Some(TOKEN), None);
        r.headers_mut().insert(owner::HEADER, "not-a-token".parse().unwrap());
        assert_eq!(app.clone().oneshot(r).await.unwrap().status(), StatusCode::FORBIDDEN);
        assert_eq!(app.clone().oneshot(owner_req("GET", "/v1/owner/settings", None, None)).await.unwrap().status(), StatusCode::FORBIDDEN);

        let owner_token = owner::register_for_test();
        let resp = app.clone().oneshot(req("POST", "/v1/tools/slskd/install", Some(TOKEN), None)).await.unwrap();
        let id = json_of(resp).await["requestId"].as_str().unwrap().to_string();
        let resp = app
            .clone()
            .oneshot(owner_req("POST", &format!("/v1/owner/requests/{id}/decide"), Some(&owner_token), Some(r#"{"approve":false}"#)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(json_of(resp).await["status"], "declined");

        let resp = app.clone().oneshot(owner_req("GET", "/v1/owner/settings", Some(&owner_token), None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(json_of(resp).await["autoUpdateTools"], true);

        let resp = app.clone().oneshot(req("GET", "/v1/recipes?full=true", Some(TOKEN), None)).await.unwrap();
        let v = json_of(resp).await;
        assert!(v[0]["recipe"]["recipeVersion"] == 1 && v[0]["origin"] == "builtin", "full listing returns stored recipes: {v}");
    }

    #[tokio::test]
    async fn events_long_poll_returns_new_events() {
        let app = setup();
        let resp = app.clone().oneshot(req("GET", "/v1/events?wait=0", Some(TOKEN), None)).await.unwrap();
        let latest = json_of(resp).await["latest"].as_u64().unwrap();
        events::emit("test-event", json!({ "x": 1 }));
        let resp = app.clone().oneshot(req("GET", &format!("/v1/events?since={latest}&wait=5"), Some(TOKEN), None)).await.unwrap();
        let v = json_of(resp).await;
        assert!(v["events"].as_array().unwrap().iter().any(|e| e["name"] == "test-event"), "{v}");
        assert!(v["latest"].as_u64().unwrap() > latest);
        let resp = app.clone().oneshot(req("GET", "/v1/health", None, None)).await.unwrap();
        let h = json_of(resp).await;
        assert_eq!(h["buildId"], "test-build");
        assert_eq!(h["role"], "service");
    }

    #[tokio::test]
    async fn install_body_carries_decisions_without_echoing_secrets() {
        let app = setup();
        let body = r#"{"config":{"soulseekUsername":"bj","soulseekPassword":"hunter2"}}"#;
        let resp = app.clone().oneshot(req("POST", "/v1/tools/slskd/install", Some(TOKEN), Some(body))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let v = json_of(resp).await;
        let id = v["requestId"].as_str().unwrap().to_string();
        let decisions = v["decisions"].as_array().unwrap();
        assert!(decisions.iter().any(|d| d["key"] == "soulseekUsername" && d["settled"] == true), "{v}");
        assert!(decisions.iter().any(|d| d["key"] == "downloadsDir"), "askOnInstall fields are listed: {v}");
        assert!(decisions.iter().any(|d| d["key"] == "startNow" && d["default"] == true), "engine choices are listed: {v}");
        assert!(decisions.iter().any(|d| d["key"] == "autostart"), "{v}");
        assert!(serde_json::to_string(&v).unwrap().find("hunter2").is_none(), "secret leaked: {v}");

        let resp = app.clone().oneshot(req("GET", &format!("/v1/requests/{id}"), Some(TOKEN), None)).await.unwrap();
        let r = json_of(resp).await;
        assert_eq!(r["config"]["soulseekUsername"], "bj");
        assert_eq!(r["secretKeys"][0], "soulseekPassword");
        assert!(serde_json::to_string(&r).unwrap().find("hunter2").is_none(), "secret leaked: {r}");
        requests::set_status(&id, requests::RequestStatus::Declined, None);

        let resp = app.clone().oneshot(req("POST", "/v1/tools/slskd/install", Some(TOKEN), Some(r#"{"config":{"nope":1}}"#))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(json_of(resp).await["error"].as_str().unwrap().contains("nope"));
        let resp = app.clone().oneshot(req("POST", "/v1/tools/slskd/install", Some(TOKEN), Some("[1]"))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let body = r#"{"config":{"startNow":false,"autostart":true}}"#;
        let resp = app.clone().oneshot(req("POST", "/v1/tools/slskd/install", Some(TOKEN), Some(body))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let v = json_of(resp).await;
        assert!(v["decisions"].as_array().unwrap().iter().any(|d| d["key"] == "startNow" && d["value"] == false), "{v}");
        requests::set_status(v["requestId"].as_str().unwrap(), requests::RequestStatus::Declined, None);
        let resp = app.clone().oneshot(req("POST", "/v1/tools/yt-dlp/install", Some(TOKEN), Some(r#"{"config":{"autostart":true}}"#))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY, "a cli tool offers no autostart");
    }

    #[tokio::test]
    async fn connection_needs_consent_and_config_never_echoes_secrets() {
        let app = setup();
        let resp = app.clone().oneshot(req("GET", "/v1/tools/slskd/connection?consumer=viboplr", None, None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert_eq!(json_of(resp).await["reason"], "consent-required");
        let resp = app.clone().oneshot(req("GET", "/v1/tools/slskd/connection?consumer=stranger", None, None)).await.unwrap();
        assert_eq!(json_of(resp).await["reason"], "unknown-consumer");
        let resp = app.clone().oneshot(req("GET", "/v1/tools/slskd/connection", None, None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let resp = app
            .clone()
            .oneshot(req("PATCH", "/v1/tools/slskd/config", Some(TOKEN), Some(r#"{"soulseekPassword":"hunter2","soulseekUsername":"bj"}"#)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = json_of(resp).await;
        assert_eq!(v["config"]["soulseekUsername"], "bj");
        assert_eq!(v["config"]["has_soulseekPassword"], true);
        assert!(serde_json::to_string(&v).unwrap().find("hunter2").is_none(), "secret leaked: {v}");

        let resp = app.clone().oneshot(req("PATCH", "/v1/tools/slskd/config", Some(TOKEN), Some(r#"{"nope":1}"#))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn recipes_validate_draft_and_refuse_builtin_names() {
        let app = setup();
        let bad = r#"{"recipeVersion":1,"name":"demo","displayName":"Demo","author":"Test","revision":1,"platforms":["darwin-arm64"],"kind":"cli","source":{"kind":"githubRelease","repo":"x/y","assets":{"darwin-arm64":"d"}},"archive":"bare","version":{"args":["--version"],"regex":"nocapture"}}"#;
        let resp = app.clone().oneshot(req("POST", "/v1/recipes/validate", Some(TOKEN), Some(bad))).await.unwrap();
        let v = json_of(resp).await;
        assert_eq!(v["ok"], false);
        assert_eq!(v["errors"][0]["pointer"], "/version/regex");

        let good = bad.replace("nocapture", "(\\\\d+)");
        let resp = app.clone().oneshot(req("PUT", "/v1/recipes/demo", Some(TOKEN), Some(&good))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED, "{}", json_of(resp).await);
        let resp = app.clone().oneshot(req("GET", "/v1/tools/demo", None, None)).await.unwrap();
        let t = json_of(resp).await;
        assert_eq!(t["trusted"], false);
        assert_eq!(t["origin"], "draft");
        let resp = app.clone().oneshot(req("POST", "/v1/tools/demo/install", Some(TOKEN), None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT, "drafts are not installable");

        let clash = good.replace("\"name\":\"demo\"", "\"name\":\"slskd\"");
        let resp = app.clone().oneshot(req("PUT", "/v1/recipes/slskd", Some(TOKEN), Some(&clash))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);

        let resp = app.clone().oneshot(req("DELETE", "/v1/recipes/demo", Some(TOKEN), None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }
}
