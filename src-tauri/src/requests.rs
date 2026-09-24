//! Things a local client asked for that only the user may grant: installing
//! or removing a tool, and letting a consumer connect to one. The API
//! answers `202 {requestId}`; Roadie's window shows the prompt; the user's
//! click resolves it; the client polls `GET /v1/requests/{id}`.
//!
//! Requests live in memory only — an unanswered prompt does not survive a
//! restart, which is the right default for "someone asked to install X".

use crate::events;
use crate::recipe::{FieldKind, Recipe};
use serde::Serialize;
use serde_json::{Map, Value};
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", rename_all_fields = "camelCase", tag = "kind")]
pub enum RequestKind {
    /// `config` holds the caller's non-secret decisions (echoed to the
    /// window and `GET /v1/requests/{id}`); `secrets` holds password fields
    /// and is never serialized — `secret_keys` says which were supplied.
    Install {
        tool: String,
        #[serde(skip_serializing_if = "Map::is_empty")]
        config: Map<String, Value>,
        #[serde(skip)]
        secrets: Map<String, Value>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        secret_keys: Vec<String>,
    },
    Uninstall { tool: String, keep_data: bool },
    /// A consumer wants this tool's connection details.
    Connect {
        consumer: String,
        tool: String,
        /// Deep link to open once approved/declined (GUI consumers).
        #[serde(skip_serializing_if = "Option::is_none")]
        return_url: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RequestStatus {
    Pending,
    Approved,
    Declined,
    Done,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub phase: String,
    pub downloaded: u64,
    pub total: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub id: String,
    #[serde(flatten)]
    pub kind: RequestKind,
    /// Who asked: a consumer's display name, "API client", "MCP client"…
    pub requested_by: String,
    pub status: RequestStatus,
    pub created_at: u64,
    pub error: Option<String>,
    pub progress: Option<Progress>,
}

/// Build an install request, routing password fields out of the public
/// `config` so a request can be listed without leaking them. The values
/// must already have passed `state::validate_patch`.
pub fn install_kind(recipe: &Recipe, values: Map<String, Value>) -> RequestKind {
    let mut config = Map::new();
    let mut secrets = Map::new();
    for (k, v) in values {
        match recipe.config_field(&k).map(|f| f.kind) {
            Some(FieldKind::Password) => {
                secrets.insert(k, v);
            }
            _ => {
                config.insert(k, v);
            }
        }
    }
    let secret_keys = secrets.keys().cloned().collect();
    RequestKind::Install { tool: recipe.name.clone(), config, secrets, secret_keys }
}

fn store() -> &'static Mutex<Vec<Request>> {
    static S: OnceLock<Mutex<Vec<Request>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(Vec::new()))
}

fn changed(r: &Request) {
    events::emit("request-changed", serde_json::to_value(r).unwrap_or_default());
}

/// Create a request, or return the identical pending one (a client that
/// retries must not stack prompts).
pub fn create(kind: RequestKind, requested_by: &str) -> Request {
    let mut all = store().lock().unwrap();
    if let Some(existing) = all.iter().find(|r| r.kind == kind && r.status == RequestStatus::Pending) {
        return existing.clone();
    }
    let r = Request {
        id: crate::paths::random_hex(8).unwrap_or_else(|_| format!("{}", crate::paths::now_secs())),
        kind,
        requested_by: requested_by.to_string(),
        status: RequestStatus::Pending,
        created_at: crate::paths::now_secs(),
        error: None,
        progress: None,
    };
    all.push(r.clone());
    // Keep the list bounded: drop finished requests older than the last 50.
    if all.len() > 100 {
        let keep_from = all.len() - 50;
        let mut i = 0;
        all.retain(|r| {
            i += 1;
            r.status == RequestStatus::Pending || i > keep_from
        });
    }
    drop(all);
    changed(&r);
    r
}

pub fn get(id: &str) -> Option<Request> {
    store().lock().unwrap().iter().find(|r| r.id == id).cloned()
}

pub fn pending() -> Vec<Request> {
    store().lock().unwrap().iter().filter(|r| r.status == RequestStatus::Pending).cloned().collect()
}

pub fn set_status(id: &str, status: RequestStatus, error: Option<String>) -> Option<Request> {
    let updated = {
        let mut all = store().lock().unwrap();
        let r = all.iter_mut().find(|r| r.id == id)?;
        r.status = status;
        r.error = error;
        if status != RequestStatus::Approved {
            r.progress = None;
        }
        r.clone()
    };
    changed(&updated);
    Some(updated)
}

pub fn set_progress(id: &str, phase: &str, downloaded: u64, total: Option<u64>) {
    let updated = {
        let mut all = store().lock().unwrap();
        let Some(r) = all.iter_mut().find(|r| r.id == id) else { return };
        r.progress = Some(Progress { phase: phase.to_string(), downloaded, total });
        r.clone()
    };
    changed(&updated);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_pending_requests_collapse_and_status_moves() {
        let install = || RequestKind::Install { tool: "t-collapse".into(), config: Map::new(), secrets: Map::new(), secret_keys: vec![] };
        let a = create(install(), "test");
        let b = create(install(), "test");
        assert_eq!(a.id, b.id);
        set_status(&a.id, RequestStatus::Declined, None);
        let c = create(install(), "test");
        assert_ne!(a.id, c.id, "a decided request does not absorb a new one");
        assert!(pending().iter().any(|r| r.id == c.id));
        set_status(&c.id, RequestStatus::Approved, None);
        set_progress(&c.id, "downloading", 10, Some(100));
        assert_eq!(get(&c.id).unwrap().progress.unwrap().downloaded, 10);
        set_status(&c.id, RequestStatus::Failed, Some("boom".into()));
        assert_eq!(get(&c.id).unwrap().error.as_deref(), Some("boom"));
    }

    #[test]
    fn install_decisions_split_secrets_out_of_the_public_shape() {
        let recipe = crate::recipe::load_builtin().remove(0);
        let mut values = Map::new();
        values.insert("soulseekUsername".into(), Value::String("bj".into()));
        values.insert("soulseekPassword".into(), Value::String("hunter2".into()));
        let kind = install_kind(&recipe, values);
        let RequestKind::Install { config, secrets, secret_keys, .. } = &kind else { panic!() };
        assert_eq!(config["soulseekUsername"], "bj");
        assert_eq!(secrets["soulseekPassword"], "hunter2");
        assert_eq!(secret_keys, &vec!["soulseekPassword".to_string()]);
        let r = create(kind, "test");
        let wire = serde_json::to_string(&r).unwrap();
        assert!(wire.contains("\"soulseekUsername\":\"bj\""), "{wire}");
        assert!(wire.contains("\"secretKeys\":[\"soulseekPassword\"]"), "{wire}");
        assert!(!wire.contains("hunter2"), "secret leaked into the request wire shape: {wire}");
    }
}
