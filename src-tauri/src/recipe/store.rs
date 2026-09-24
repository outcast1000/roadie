//! Every recipe Roadie knows, with where it came from. Built-ins are compiled
//! in; user recipes live in `recipes/<name>.json`; anything that arrived
//! through the API is a **draft** (`recipes/<name>.draft.json`) until the user
//! reads it in the review screen and clicks Trust — only trusted recipes can
//! be installed. Names are unique across all three.

use super::{Recipe, ValidationError};
use crate::paths;
use serde::Serialize;
use std::sync::{OnceLock, RwLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    Builtin,
    User,
    Draft,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Stored {
    pub recipe: Recipe,
    pub origin: Origin,
    /// Who submitted a draft (an API consumer's display name), for the review.
    pub submitted_by: Option<String>,
}

impl Stored {
    pub fn trusted(&self) -> bool {
        self.origin != Origin::Draft
    }
}

fn store() -> &'static RwLock<Vec<Stored>> {
    static S: OnceLock<RwLock<Vec<Stored>>> = OnceLock::new();
    S.get_or_init(|| RwLock::new(Vec::new()))
}

/// Built-ins first, then whatever is on disk. Invalid files are skipped with
/// a warning rather than failing startup.
pub fn load_all() {
    let mut all: Vec<Stored> = super::load_builtin()
        .into_iter()
        .map(|recipe| Stored { recipe, origin: Origin::Builtin, submitted_by: None })
        .collect();
    if let Ok(dir) = paths::recipes_dir() {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            let mut files: Vec<_> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
            files.sort();
            for path in files {
                let Some(fname) = path.file_name().and_then(|n| n.to_str()) else { continue };
                let (origin, stem) = if let Some(s) = fname.strip_suffix(".draft.json") {
                    (Origin::Draft, s)
                } else if let Some(s) = fname.strip_suffix(".json") {
                    (Origin::User, s)
                } else {
                    continue;
                };
                let Ok(text) = std::fs::read_to_string(&path) else { continue };
                let (recipe_json, submitted_by) = split_envelope(&text);
                match super::parse(&recipe_json) {
                    Ok(recipe) if recipe.name == stem => {
                        if all.iter().any(|s| s.recipe.name == recipe.name) {
                            log::warn!("recipe {} in {} shadows an existing one; skipped", recipe.name, path.display());
                            continue;
                        }
                        all.push(Stored { recipe, origin, submitted_by });
                    }
                    Ok(r) => log::warn!("recipe file {} is named {} inside; skipped", path.display(), r.name),
                    Err(e) => log::warn!("recipe file {} is invalid: {e:?}", path.display()),
                }
            }
        }
    }
    *store().write().unwrap() = all;
}

/// Drafts are wrapped in `{"submittedBy": …, "recipe": {…}}` so the review
/// can say who wrote them; plain recipe files are accepted too.
fn split_envelope(text: &str) -> (String, Option<String>) {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
        if let (Some(recipe), by) = (v.get("recipe"), v.get("submittedBy")) {
            return (recipe.to_string(), by.and_then(|b| b.as_str()).map(str::to_string));
        }
    }
    (text.to_string(), None)
}

pub fn list() -> Vec<Stored> {
    store().read().unwrap().clone()
}

pub fn get(name: &str) -> Option<Stored> {
    store().read().unwrap().iter().find(|s| s.recipe.name == name).cloned()
}

/// Only trusted recipes are eligible for install/start.
pub fn get_trusted(name: &str) -> Result<Recipe, String> {
    match get(name) {
        Some(s) if s.trusted() => Ok(s.recipe),
        Some(_) => Err(format!("recipe {name} is a draft — review and trust it in Roadie first")),
        None => Err(format!("unknown tool: {name}")),
    }
}

#[derive(Debug)]
pub enum PutError {
    Invalid(Vec<ValidationError>),
    /// A built-in or a trusted user recipe already owns the name.
    Conflict(String),
    Io(String),
}

/// Save an API-submitted recipe as a draft. Re-submitting a draft replaces it.
pub fn put_draft(recipe: Recipe, submitted_by: Option<String>) -> Result<Stored, PutError> {
    let errors = super::validate(&recipe);
    if !errors.is_empty() {
        return Err(PutError::Invalid(errors));
    }
    if let Some(existing) = get(&recipe.name) {
        if existing.origin != Origin::Draft {
            return Err(PutError::Conflict(format!(
                "a {} recipe named {} already exists",
                match existing.origin {
                    Origin::Builtin => "built-in",
                    _ => "trusted",
                },
                recipe.name
            )));
        }
    }
    let dir = paths::recipes_dir().map_err(PutError::Io)?;
    let envelope = serde_json::json!({ "submittedBy": submitted_by, "recipe": recipe });
    let text = serde_json::to_string_pretty(&envelope).map_err(|e| PutError::Io(e.to_string()))?;
    paths::write_atomic(&dir.join(format!("{}.draft.json", recipe.name)), text.as_bytes(), false).map_err(PutError::Io)?;
    let stored = Stored { recipe, origin: Origin::Draft, submitted_by };
    let mut all = store().write().unwrap();
    all.retain(|s| s.recipe.name != stored.recipe.name);
    all.push(stored.clone());
    Ok(stored)
}

/// The user read the draft and accepts it: it becomes a plain user recipe.
pub fn trust(name: &str) -> Result<Stored, String> {
    let stored = get(name).ok_or_else(|| format!("unknown recipe: {name}"))?;
    if stored.origin != Origin::Draft {
        return Ok(stored);
    }
    let dir = paths::recipes_dir()?;
    let text = serde_json::to_string_pretty(&stored.recipe).map_err(|e| e.to_string())?;
    paths::write_atomic(&dir.join(format!("{name}.json")), text.as_bytes(), false)?;
    let _ = std::fs::remove_file(dir.join(format!("{name}.draft.json")));
    let trusted = Stored { origin: Origin::User, ..stored };
    let mut all = store().write().unwrap();
    all.retain(|s| s.recipe.name != name);
    all.push(trusted.clone());
    Ok(trusted)
}

/// Remove a user or draft recipe (built-ins cannot be removed).
pub fn delete(name: &str) -> Result<(), String> {
    let stored = get(name).ok_or_else(|| format!("unknown recipe: {name}"))?;
    if stored.origin == Origin::Builtin {
        return Err("built-in recipes cannot be deleted".into());
    }
    let dir = paths::recipes_dir()?;
    let _ = std::fs::remove_file(dir.join(format!("{name}.json")));
    let _ = std::fs::remove_file(dir.join(format!("{name}.draft.json")));
    store().write().unwrap().retain(|s| s.recipe.name != name);
    Ok(())
}
