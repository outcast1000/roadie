//! `roadie://` deep links — the handoff for GUI apps that cannot read the
//! discovery file (a webview plugin can open a URL and fetch loopback HTTP,
//! nothing more).
//!
//!   roadie://install/<tool>?consumer=<id>&return=<url-encoded>
//!   roadie://connect/<tool>?consumer=<id>&return=<url-encoded>
//!   roadie://open/<tool>
//!
//! `install`/`connect` with a known consumer queue a **Connect request** the
//! user approves in the window; approval mints the key and opens `return`
//! with `?status=connected&tool=<name>` (no key in the URL — the consumer
//! then fetches `/v1/tools/<name>/connection?consumer=<id>`). Nothing is
//! installed by a link: the window focuses the tool's row and the user
//! clicks Install.

use crate::{client, consent, events, recipe};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verb {
    Install,
    Connect,
    Open,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Intent {
    pub verb: Verb,
    pub tool: String,
    pub consumer: Option<String>,
    pub return_url: Option<String>,
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() + 0 && i + 2 <= bytes.len() - 1 || (bytes[i] == b'%' && i + 2 < bytes.len()) => {
                match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse and validate. The consumer must be known and the return URL must
/// start with that consumer's registered prefix (never http).
pub fn parse(url: &str) -> Result<Intent, String> {
    let rest = url.strip_prefix("roadie://").ok_or("not a roadie:// link")?;
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let mut segs = path.trim_end_matches('/').split('/');
    let verb = match segs.next() {
        Some("install") => Verb::Install,
        Some("connect") => Verb::Connect,
        Some("open") => Verb::Open,
        other => return Err(format!("unknown verb {other:?}; use install, connect or open")),
    };
    let tool = segs.next().map(percent_decode).filter(|t| recipe::is_valid_name(t)).ok_or("missing or invalid tool name")?;
    if segs.next().is_some() {
        return Err("too many path segments".into());
    }
    let mut consumer = None;
    let mut return_url = None;
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match k {
            "consumer" => consumer = Some(percent_decode(v)),
            "return" => return_url = Some(percent_decode(v)),
            _ => {}
        }
    }
    if verb == Verb::Open {
        return Ok(Intent { verb, tool, consumer: None, return_url: None });
    }
    let Some(id) = consumer.as_deref() else {
        // No consumer: behave like `open`.
        return Ok(Intent { verb: Verb::Open, tool, consumer: None, return_url: None });
    };
    let record = consent::get(id).ok_or_else(|| format!("unknown consumer `{id}`; register it through the API first"))?;
    if let Some(r) = &return_url {
        if r.starts_with("http://") || r.starts_with("https://") {
            return Err("return must be a custom scheme link, not http(s)".into());
        }
        match &record.return_prefix {
            Some(p) if r.starts_with(p.as_str()) => {}
            Some(p) => return Err(format!("return must start with {p}")),
            None => return Err(format!("consumer `{id}` registered no return prefix")),
        }
    }
    Ok(Intent { verb, tool, consumer, return_url })
}

/// A link reached the window: hand it to the service over the owner
/// channel (`actions::intent` parses it, queues a Connect request for a
/// known consumer and emits `intent`, which comes back through the event
/// pump), and bring the window forward.
pub fn handle(app: &tauri::AppHandle, url: &str) {
    let url = url.to_string();
    std::thread::spawn(move || {
        if let Err(e) = client::call("POST", "/v1/owner/intent", Some(serde_json::json!({ "url": url })), true) {
            log::warn!("deep link {url} not accepted: {e}");
            events::emit("intent-error", serde_json::json!({ "url": url, "error": e }));
        }
    });
    focus_window(app);
}

/// Something needs the user: bring a connected window forward, or open one
/// when none is connected (the service runs headless).
pub fn focus_if_possible() {
    events::emit("focus-request", serde_json::Value::Null);
    crate::service::open_window_if_needed();
}

pub fn focus_window(app: &tauri::AppHandle) {
    use tauri::Manager;
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// Build the link a consumer is sent back to.
pub fn return_link(base: &str, status: &str, tool: &str) -> String {
    let sep = if base.contains('?') { '&' } else { '?' };
    format!("{base}{sep}status={status}&tool={tool}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_verbs_and_rejects_bad_shapes() {
        let i = parse("roadie://open/slskd").unwrap();
        assert_eq!(i, Intent { verb: Verb::Open, tool: "slskd".into(), consumer: None, return_url: None });
        assert!(parse("roadie://dance/slskd").unwrap_err().contains("unknown verb"));
        assert!(parse("roadie://open/Bad%20Name").is_err());
        assert!(parse("roadie://open/a/b").is_err());
        assert!(parse("https://example.com").is_err());
        // install without a consumer degrades to open
        assert_eq!(parse("roadie://install/slskd").unwrap().verb, Verb::Open);
    }

    #[test]
    fn return_links_and_decoding() {
        assert_eq!(return_link("viboplr://plugin/slskd/roadie", "connected", "slskd"), "viboplr://plugin/slskd/roadie?status=connected&tool=slskd");
        assert_eq!(return_link("x://a?b=1", "declined", "t"), "x://a?b=1&status=declined&tool=t");
        assert_eq!(percent_decode("viboplr%3A%2F%2Fplugin%2Fslskd+x"), "viboplr://plugin/slskd x");
    }
}
