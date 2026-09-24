//! What the user's clicks do, in one place, independent of any window: the
//! service's owner routes call these, and so does anything else that is
//! allowed to act *as the user* (nothing else today). API handlers for
//! other clients never call `decide`/`trust`/`install_now` — they queue a
//! request or save a draft.

use crate::recipe::{self, store};
use crate::{consent, events, paths, requests, scheme, tools};
use serde_json::{Map, Value};

/// Install progress → the event log (the window's bar) and, for a request,
/// into the request so the asking client sees it too.
pub fn progress_reporter(name: String, request_id: Option<String>) -> impl FnMut(tools::install::Phase, u64, Option<u64>) {
    move |phase, downloaded, total| {
        let phase_s = serde_json::to_value(phase).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
        events::emit("tool-install-progress", serde_json::json!({ "name": name, "phase": phase, "downloaded": downloaded, "total": total }));
        if let Some(id) = &request_id {
            requests::set_progress(id, &phase_s, downloaded, total);
        }
    }
}

/// Apply the install-time decisions (the recipe's `askOnInstall` fields and
/// the engine's own `startNow`/`autostart`), then install. Config goes first
/// so the first rendered files already carry it; if the download then fails
/// the answers are kept for a retry.
pub fn install_with(recipe: &recipe::Recipe, decisions: &Map<String, Value>, progress: &mut impl FnMut(tools::install::Phase, u64, Option<u64>)) -> Result<(), String> {
    let mut config = decisions.clone();
    let options = tools::install_options(recipe, &mut config)?;
    if !config.is_empty() {
        tools::configure(recipe, &config)?;
    }
    tools::install(recipe, progress)?;
    if recipe.kind == recipe::Kind::Daemon {
        if options.autostart {
            tools::set_autostart(recipe, true)?;
        }
        if options.start_now {
            events::tool_changed(&recipe.name);
            tools::start(recipe, "install")?;
        }
    }
    Ok(())
}

/// The user clicked Install on a card: install right away.
pub fn install_now(name: &str, decisions: &Map<String, Value>) -> Result<tools::ToolStatus, String> {
    let recipe = store::get_trusted(name)?;
    let mut progress = progress_reporter(name.to_string(), None);
    let result = install_with(&recipe, decisions, &mut progress);
    events::tool_changed(name);
    result.map(|()| tools::status(&recipe))
}

/// The user clicked Remove on a card.
pub fn uninstall_now(name: &str, keep_data: bool) -> Result<(), String> {
    let recipe = store::get_trusted(name)?;
    let result = tools::uninstall(&recipe, keep_data);
    events::tool_changed(name);
    result
}

/// The user answered a prompt. Approving runs the action here, reporting
/// progress into the request so the API client sees it. For an install,
/// `answers` are the values typed or changed in the prompt; they win over
/// what the client asked for.
pub fn decide(id: &str, approve: bool, answers: Option<Map<String, Value>>) -> Result<requests::Request, String> {
    let r = requests::get(id).ok_or("unknown request")?;
    if r.status != requests::RequestStatus::Pending {
        return Ok(r);
    }
    if !approve {
        if let requests::RequestKind::Connect { return_url: Some(url), tool, .. } = &r.kind {
            open_return(url, "declined", tool);
        }
        return requests::set_status(id, requests::RequestStatus::Declined, None).ok_or("gone".into());
    }
    requests::set_status(id, requests::RequestStatus::Approved, None);
    let outcome = match r.kind.clone() {
        requests::RequestKind::Install { tool, config, secrets, .. } => {
            let recipe = store::get_trusted(&tool)?;
            let mut decisions = config;
            decisions.extend(secrets);
            decisions.extend(answers.unwrap_or_default());
            let mut progress = progress_reporter(tool.clone(), Some(id.to_string()));
            let out = install_with(&recipe, &decisions, &mut progress);
            events::tool_changed(&tool);
            out
        }
        requests::RequestKind::Uninstall { tool, keep_data } => {
            let recipe = store::get_trusted(&tool)?;
            let out = tools::uninstall(&recipe, keep_data);
            events::tool_changed(&tool);
            out
        }
        requests::RequestKind::Connect { consumer, tool, return_url } => {
            let recipe = store::get_trusted(&tool)?;
            consent::approve(&consumer, &recipe)?;
            // Re-render the config so the new key lands (restart when idle).
            if tools::status(&recipe).installed {
                let _ = tools::refresh_consumers(&recipe);
            }
            events::tool_changed(&tool);
            if let Some(url) = return_url {
                open_return(&url, "connected", &tool);
            }
            Ok(())
        }
    };
    match outcome {
        Ok(()) => requests::set_status(id, requests::RequestStatus::Done, None),
        Err(e) => requests::set_status(id, requests::RequestStatus::Failed, Some(e)),
    }
    .ok_or("gone".into())
}

/// The user read a draft and clicked Trust.
pub fn trust(name: &str) -> Result<store::Stored, String> {
    let r = store::trust(name)?;
    events::emit("recipe-changed", serde_json::json!({ "name": r.recipe.name, "origin": r.origin }));
    events::tool_changed(&r.recipe.name);
    Ok(r)
}

/// A `roadie://` link reached the window; the service records the intent
/// (a Connect request for a known consumer) and tells the window what to
/// focus.
pub fn intent(url: &str) -> Result<scheme::Intent, String> {
    let intent = scheme::parse(url).map_err(|e| {
        events::emit("intent-error", serde_json::json!({ "url": url, "error": e }));
        e
    })?;
    let known = store::get(&intent.tool).is_some();
    if let (Some(consumer), true) = (&intent.consumer, known) {
        let display = consent::get(consumer).map(|r| r.display_name).unwrap_or_else(|| consumer.clone());
        requests::create(
            requests::RequestKind::Connect { consumer: consumer.clone(), tool: intent.tool.clone(), return_url: intent.return_url.clone() },
            &display,
        );
    }
    events::emit("intent", serde_json::json!({ "verb": intent.verb, "tool": intent.tool, "known": known }));
    Ok(intent)
}

/// Open a consumer's return link with the outcome. The service has no
/// webview; it asks the OS directly, and also emits `open-url` so a
/// connected window can do it if the OS handoff fails.
fn open_return(base: &str, status: &str, tool: &str) {
    let url = scheme::return_link(base, status, tool);
    if let Err(e) = open_url(&url) {
        log::warn!("could not open return link {url}: {e}");
        events::emit("return-link-failed", serde_json::json!({ "url": url, "tool": tool }));
    }
}

/// Hand a URL to the OS's default handler (no crate: `open`, `xdg-open`,
/// `rundll32`). Only custom-scheme and http(s) URLs are accepted.
pub fn open_url(url: &str) -> Result<(), String> {
    let ok = url.split_once("://").is_some_and(|(scheme, _)| !scheme.is_empty() && scheme.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.'));
    if !ok {
        return Err(format!("not a URL: {url}"));
    }
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("/usr/bin/open");
        c.arg(url);
        c
    };
    #[cfg(windows)]
    let mut cmd = {
        let mut c = std::process::Command::new("rundll32");
        c.args(["url.dll,FileProtocolHandler", url]);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    let status = cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().map_err(|e| format!("open {url}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("open {url}: exit {status}"))
    }
}

// --- Settings ---

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub auto_update_tools: bool,
    /// On: the service is a per-user login item and keeps running when the
    /// window closes, so the API, the tools' "start at login" and the daily
    /// updates work without a window. Off: Roadie behaves like a plain app —
    /// no login item, and the service exits shortly after the window
    /// disconnects. Daemons are independent processes either way.
    pub run_in_background: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Settings { auto_update_tools: true, run_in_background: true }
    }
}

fn settings_path() -> Option<std::path::PathBuf> {
    paths::data_root().ok().map(|r| r.join("settings.json"))
}
pub fn load_settings() -> Settings {
    settings_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}
pub fn save_settings(settings: &Settings) -> Result<Settings, String> {
    let p = settings_path().ok_or("data root not initialized")?;
    paths::write_atomic(&p, serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?.as_bytes(), false)?;
    // The login item follows the setting right away.
    if let Ok(root) = paths::data_root() {
        if settings.run_in_background {
            tools::autostart::enable_service(root)?;
        } else {
            tools::autostart::disable_service()?;
        }
    }
    events::emit("settings-changed", serde_json::to_value(settings).unwrap_or_default());
    Ok(settings.clone())
}
pub fn auto_update_enabled() -> bool {
    load_settings().auto_update_tools
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_url_refuses_non_urls() {
        assert!(open_url("not a url").is_err());
        assert!(open_url("/etc/passwd").is_err());
        assert!(open_url("; rm -rf /").is_err());
    }
}
