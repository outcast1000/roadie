//! Who may connect to which tool. A *consumer* is an app (Viboplr, a script,
//! an assistant) identified by a short id. The user approves a consumer for
//! a tool once, in Roadie's window; Roadie then mints that consumer its own
//! key, renders it into the tool's config, and serves it on request.
//! Revoking drops the key from the config at the next restart.
//!
//! `consumers.json`, 0600: keys live here and nowhere else.

use crate::paths;
use crate::recipe::template::Consumer;
use crate::recipe::{ConnectionPolicy, Recipe};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Mutex;

pub const FILE: &str = "consumers.json";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Grant {
    pub key: String,
    pub approved_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ConsumerRecord {
    pub display_name: String,
    /// Deep links back to this consumer must start with this (never http).
    pub return_prefix: Option<String>,
    pub registered_at: u64,
    pub tools: BTreeMap<String, Grant>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Consumers {
    pub consumers: BTreeMap<String, ConsumerRecord>,
}

/// Consumers Roadie knows without registration.
pub const BUILTIN: &[(&str, &str, &str)] = &[("viboplr", "Viboplr", "viboplr://plugin/")];

fn file_lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}
use std::sync::OnceLock;

pub fn is_valid_id(id: &str) -> bool {
    let b = id.as_bytes();
    !b.is_empty() && b.len() <= 32 && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

pub fn load() -> Consumers {
    let Ok(root) = paths::data_root() else { return Consumers::default() };
    let mut c: Consumers = std::fs::read_to_string(root.join(FILE))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    for (id, display, prefix) in BUILTIN {
        c.consumers.entry(id.to_string()).or_insert_with(|| ConsumerRecord {
            display_name: display.to_string(),
            return_prefix: Some(prefix.to_string()),
            registered_at: 0,
            tools: BTreeMap::new(),
        });
    }
    c
}

fn save(c: &Consumers) -> Result<(), String> {
    let root = paths::data_root()?;
    let text = serde_json::to_string_pretty(c).map_err(|e| e.to_string())?;
    paths::write_atomic(&root.join(FILE), text.as_bytes(), true)
}

/// Public view: no keys.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsumerPublic {
    pub id: String,
    pub display_name: String,
    pub return_prefix: Option<String>,
    pub builtin: bool,
    pub tools: Vec<String>,
}

pub fn list() -> Vec<ConsumerPublic> {
    load()
        .consumers
        .into_iter()
        .map(|(id, r)| ConsumerPublic {
            builtin: BUILTIN.iter().any(|(b, _, _)| *b == id),
            id,
            display_name: r.display_name,
            return_prefix: r.return_prefix,
            tools: r.tools.keys().cloned().collect(),
        })
        .collect()
}

pub fn get(id: &str) -> Option<ConsumerRecord> {
    load().consumers.remove(id)
}

pub fn register(id: &str, display_name: &str, return_prefix: Option<&str>) -> Result<ConsumerPublic, String> {
    if !is_valid_id(id) {
        return Err("consumer id must match ^[a-z0-9-]{1,32}$".into());
    }
    if let Some(p) = return_prefix {
        if p.starts_with("http://") || p.starts_with("https://") || !p.contains("://") {
            return Err("returnPrefix must be a custom URL scheme such as myapp://".into());
        }
    }
    let _g = file_lock().lock().unwrap();
    let mut c = load();
    let rec = c.consumers.entry(id.to_string()).or_insert_with(|| ConsumerRecord {
        display_name: String::new(),
        return_prefix: None,
        registered_at: paths::now_secs(),
        tools: BTreeMap::new(),
    });
    if !display_name.trim().is_empty() {
        rec.display_name = display_name.trim().to_string();
    }
    if return_prefix.is_some() {
        rec.return_prefix = return_prefix.map(str::to_string);
    }
    let out = ConsumerPublic {
        id: id.to_string(),
        display_name: rec.display_name.clone(),
        return_prefix: rec.return_prefix.clone(),
        builtin: BUILTIN.iter().any(|(b, _, _)| *b == id),
        tools: rec.tools.keys().cloned().collect(),
    };
    save(&c)?;
    Ok(out)
}

/// Approved consumers for a tool, as the template engine wants them.
pub fn consumers_for(tool: &str) -> Vec<Consumer> {
    load()
        .consumers
        .into_iter()
        .filter_map(|(id, r)| r.tools.get(tool).map(|g| Consumer { id, key: g.key.clone() }))
        .collect()
}

pub fn is_approved(consumer: &str, tool: &str) -> bool {
    load().consumers.get(consumer).is_some_and(|r| r.tools.contains_key(tool))
}

pub fn grant(consumer: &str, tool: &str) -> Option<Grant> {
    load().consumers.get(consumer)?.tools.get(tool).cloned()
}

/// Approve `consumer` for `tool`, minting a key sized for the recipe's
/// policy. Idempotent: an existing grant keeps its key.
pub fn approve(consumer: &str, recipe: &Recipe) -> Result<Grant, String> {
    let policy = recipe.connection.as_ref().map(|c| c.policy).unwrap_or(ConnectionPolicy::None);
    if policy == ConnectionPolicy::None {
        return Err(format!("{} exposes no connection to share", recipe.display_name));
    }
    let _g = file_lock().lock().unwrap();
    let mut c = load();
    let rec = c
        .consumers
        .get_mut(consumer)
        .ok_or_else(|| format!("unknown consumer {consumer}; register it first"))?;
    if let Some(g) = rec.tools.get(&recipe.name) {
        return Ok(g.clone());
    }
    let key = if policy == ConnectionPolicy::PerConsumerKey {
        let want = 48usize;
        let (lo, hi) = recipe
            .connection
            .as_ref()
            .map(|c| (c.min_len.unwrap_or(16), c.max_len.unwrap_or(255)))
            .unwrap_or((16, 255));
        paths::random_hex(want.clamp(lo, hi) / 2)?
    } else {
        String::new()
    };
    let g = Grant { key, approved_at: paths::now_secs() };
    rec.tools.insert(recipe.name.clone(), g.clone());
    save(&c)?;
    Ok(g)
}

pub fn revoke(consumer: &str, tool: Option<&str>) -> Result<(), String> {
    let _g = file_lock().lock().unwrap();
    let mut c = load();
    match tool {
        Some(t) => {
            if let Some(r) = c.consumers.get_mut(consumer) {
                r.tools.remove(t);
            }
        }
        None => {
            if BUILTIN.iter().any(|(b, _, _)| *b == consumer) {
                // Built-ins stay known; only their grants go.
                if let Some(r) = c.consumers.get_mut(consumer) {
                    r.tools.clear();
                }
            } else {
                c.consumers.remove(consumer);
            }
        }
    }
    save(&c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_prefixes_are_checked() {
        assert!(is_valid_id("viboplr") && is_valid_id("my-app2"));
        assert!(!is_valid_id("MyApp") && !is_valid_id("") && !is_valid_id("a b"));
        // register() needs a data root; exercised in the API tests.
    }
}
