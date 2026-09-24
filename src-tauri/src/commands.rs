//! Tauri commands for the window. The window is a client of the service:
//! every command here is a relay over the local API (`client.rs`), carrying
//! the bearer token and, for the user's own clicks, the owner token. Only
//! the few commands that need the desktop (open a folder, a URL, the
//! updater) do anything locally.
//!
//! Wire shapes are the API's, which the frontend types already mirror.

use crate::{client, mcp_setup, paths, recipe};
use serde_json::{json, Map, Value};
use tauri::{AppHandle, Manager};

async fn relay(method: &'static str, path: String, body: Option<Value>, owner: bool) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || client::call(method, &path, body, owner))
        .await
        .map_err(|e| format!("task failed: {e}"))?
}

fn enc(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                vec![c]
            } else {
                c.to_string().into_bytes().into_iter().flat_map(|b| format!("%{b:02X}").chars().collect::<Vec<_>>()).collect()
            }
        })
        .collect()
}

// --- Tools ---

#[tauri::command]
pub async fn tool_list() -> Result<Value, String> {
    relay("GET", "/v1/tools".into(), None, false).await
}
#[tauri::command]
pub async fn tool_status(name: String) -> Result<Value, String> {
    relay("GET", format!("/v1/tools/{}", enc(&name)), None, false).await
}
/// The user clicked Install; `config` are the decisions from the prompt.
#[tauri::command]
pub async fn tool_install(name: String, config: Option<Map<String, Value>>) -> Result<Value, String> {
    relay("POST", format!("/v1/owner/tools/{}/install", enc(&name)), Some(json!({ "config": config.unwrap_or_default() })), true).await
}
#[tauri::command]
pub async fn tool_start(name: String) -> Result<Value, String> {
    relay("POST", format!("/v1/tools/{}/start", enc(&name)), None, false).await
}
#[tauri::command]
pub async fn tool_stop(name: String) -> Result<Value, String> {
    relay("POST", format!("/v1/tools/{}/stop", enc(&name)), None, false).await
}
#[tauri::command]
pub async fn tool_restart(name: String) -> Result<Value, String> {
    relay("POST", format!("/v1/tools/{}/restart", enc(&name)), None, false).await
}
#[tauri::command]
pub async fn tool_set_autostart(name: String, enabled: bool) -> Result<Value, String> {
    relay("POST", format!("/v1/tools/{}/autostart", enc(&name)), Some(json!({ "enabled": enabled })), false).await
}
#[tauri::command]
pub async fn tool_configure(name: String, patch: Map<String, Value>) -> Result<Value, String> {
    relay("PATCH", format!("/v1/tools/{}/config", enc(&name)), Some(Value::Object(patch)), false).await
}
#[tauri::command]
pub async fn tool_check_updates(name: String) -> Result<Value, String> {
    relay("POST", format!("/v1/tools/{}/check-updates", enc(&name)), None, false).await
}
#[tauri::command]
pub async fn tool_update(name: String) -> Result<Value, String> {
    relay("POST", format!("/v1/tools/{}/update", enc(&name)), None, false).await
}
#[tauri::command]
pub async fn tool_uninstall(name: String, keep_data: bool) -> Result<(), String> {
    relay("DELETE", format!("/v1/owner/tools/{}?keepData={keep_data}", enc(&name)), None, true).await.map(|_| ())
}
#[tauri::command]
pub async fn tool_logs(name: String, lines: Option<usize>) -> Result<String, String> {
    let v = relay("GET", format!("/v1/tools/{}/logs?lines={}", enc(&name), lines.unwrap_or(60)), None, false).await?;
    Ok(v.get("lines")
        .and_then(|l| l.as_array())
        .map(|a| a.iter().filter_map(|s| s.as_str()).collect::<Vec<_>>().join("\n"))
        .unwrap_or_default())
}
#[tauri::command]
pub async fn tool_open_logs(app: AppHandle, name: String) -> Result<(), String> {
    let p = paths::tool_paths(&name)?;
    std::fs::create_dir_all(&p.logs).map_err(|e| format!("create {}: {e}", p.logs.display()))?;
    use tauri_plugin_opener::OpenerExt;
    app.opener().open_path(p.logs.to_string_lossy().into_owned(), None::<&str>).map_err(|e| e.to_string())
}
#[tauri::command]
pub async fn tool_open_url(app: AppHandle, url: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    app.opener().open_url(url, None::<&str>).map_err(|e| e.to_string())
}

// --- Recipes ---

#[tauri::command]
pub async fn recipe_list() -> Result<Value, String> {
    relay("GET", "/v1/recipes?full=true".into(), None, false).await
}
#[tauri::command]
pub async fn recipe_get(name: String) -> Result<Value, String> {
    relay("GET", format!("/v1/recipes/{}", enc(&name)), None, false).await
}
/// The user read the review screen and clicked Trust.
#[tauri::command]
pub async fn recipe_trust(name: String) -> Result<Value, String> {
    relay("POST", format!("/v1/owner/recipes/{}/trust", enc(&name)), None, true).await
}
#[tauri::command]
pub async fn recipe_delete(name: String) -> Result<(), String> {
    relay("DELETE", format!("/v1/recipes/{}", enc(&name)), None, false).await.map(|_| ())
}
#[tauri::command]
pub async fn recipe_dry_run(name: String) -> Result<Value, String> {
    relay("POST", format!("/v1/recipes/{}/dryrun", enc(&name)), None, false).await
}

// --- Consumers + requests ---

#[tauri::command]
pub async fn consumer_list() -> Result<Value, String> {
    relay("GET", "/v1/consumers".into(), None, false).await
}
#[tauri::command]
pub async fn consumer_revoke(id: String, tool: Option<String>) -> Result<(), String> {
    let q = tool.map(|t| format!("?tool={}", enc(&t))).unwrap_or_default();
    relay("DELETE", format!("/v1/consumers/{}{q}", enc(&id)), None, false).await.map(|_| ())
}
#[tauri::command]
pub async fn request_list() -> Result<Value, String> {
    relay("GET", "/v1/requests".into(), None, false).await
}
/// The user's click on a prompt; `answers` are install decisions typed or
/// changed there. Owner route: only the window can answer a request.
#[tauri::command]
pub async fn request_decide(id: String, approve: bool, answers: Option<Map<String, Value>>) -> Result<Value, String> {
    relay("POST", format!("/v1/owner/requests/{}/decide", enc(&id)), Some(json!({ "approve": approve, "answers": answers })), true).await
}

// --- Settings + about ---

#[tauri::command]
pub async fn settings_get() -> Result<Value, String> {
    relay("GET", "/v1/owner/settings".into(), None, true).await
}
#[tauri::command]
pub async fn settings_set(settings: Value) -> Result<Value, String> {
    relay("PUT", "/v1/owner/settings".into(), Some(settings), true).await
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub version: String,
    pub data_dir: String,
    pub api_port: Option<u16>,
    pub platform: String,
    pub service: Option<ServiceInfo>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceInfo {
    pub version: String,
    pub pid: u64,
    pub owner_channel: bool,
    pub login_item: bool,
}

#[tauri::command]
pub async fn app_info(app: AppHandle) -> Result<AppInfo, String> {
    let data_dir = paths::data_root()?.to_path_buf();
    let conn = client::current();
    Ok(AppInfo {
        version: app.package_info().version.to_string(),
        data_dir: data_dir.to_string_lossy().into_owned(),
        api_port: conn.as_ref().map(|c| c.port),
        platform: recipe::Platform::current().key(),
        service: conn.map(|c| ServiceInfo {
            version: c.service_version,
            pid: c.service_pid,
            owner_channel: c.owner_token.is_some(),
            login_item: crate::tools::autostart::service_enabled(),
        }),
    })
}

/// Reconnect to the service (or start it) — the banner's Retry button.
#[tauri::command]
pub async fn service_reconnect() -> Result<AppInfo, String> {
    let root = paths::data_root()?.to_path_buf();
    tauri::async_runtime::spawn_blocking(move || client::connect(&root)).await.map_err(|e| format!("task failed: {e}"))??;
    Err("reconnected".into()).or_else(|_: String| {
        // Return the fresh info through the same shape the pane reads.
        let root = paths::data_root()?.to_path_buf();
        let conn = client::current();
        Ok(AppInfo {
            version: env!("CARGO_PKG_VERSION").into(),
            data_dir: root.to_string_lossy().into_owned(),
            api_port: conn.as_ref().map(|c| c.port),
            platform: recipe::Platform::current().key(),
            service: conn.map(|c| ServiceInfo { version: c.service_version, pid: c.service_pid, owner_channel: c.owner_token.is_some(), login_item: crate::tools::autostart::service_enabled() }),
        })
    })
}

#[tauri::command]
pub async fn mcp_setup_info(app: AppHandle) -> Result<mcp_setup::McpSetupInfo, String> {
    let resource_dir = app.path().resource_dir().ok();
    tauri::async_runtime::spawn_blocking(move || Ok(mcp_setup::info(resource_dir))).await.map_err(|e| format!("task failed: {e}"))?
}

pub fn handler() -> impl Fn(tauri::ipc::Invoke) -> bool + Send + Sync + 'static {
    tauri::generate_handler![
        tool_list,
        tool_status,
        tool_install,
        tool_start,
        tool_stop,
        tool_restart,
        tool_set_autostart,
        tool_configure,
        tool_check_updates,
        tool_update,
        tool_uninstall,
        tool_logs,
        tool_open_logs,
        tool_open_url,
        recipe_list,
        recipe_get,
        recipe_trust,
        recipe_delete,
        recipe_dry_run,
        consumer_list,
        consumer_revoke,
        request_list,
        request_decide,
        settings_get,
        settings_set,
        app_info,
        service_reconnect,
        mcp_setup_info,
    ]
}
