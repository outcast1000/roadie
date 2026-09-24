//! Every recipe Roadie knows, with where it came from. Built-ins are compiled
//! in; user recipes live in `recipes/<name>.json`; anything that arrived
//! through the API is a **draft** (`recipes/<name>.draft.json`) until the user
//! reads it in the review screen and clicks Trust — only trusted recipes can
//! be installed. Names are unique across all three.
//!
//! A client may also *bring* a recipe with an install or update request
//! (`compare` says how it differs from what is stored). It is not saved
//! anywhere until the user approves that request, which trusts it here
//! (`put_trusted`). Such a user recipe may replace a built-in of the same
//! name, until Roadie ships a built-in with a higher `revision`.

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
    let builtins: Vec<Stored> = super::load_builtin()
        .into_iter()
        .map(|recipe| Stored { recipe, origin: Origin::Builtin, submitted_by: None })
        .collect();
    let mut files = Vec::new();
    if let Ok(dir) = paths::recipes_dir() {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            let mut paths_found: Vec<_> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
            paths_found.sort();
            for path in paths_found {
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
                    Ok(recipe) if recipe.name == stem => files.push(Stored { recipe, origin, submitted_by }),
                    Ok(r) => log::warn!("recipe file {} is named {} inside; skipped", path.display(), r.name),
                    Err(e) => log::warn!("recipe file {} is invalid: {e:?}", path.display()),
                }
            }
        }
    }
    *store().write().unwrap() = merge(builtins, files);
}

/// Names are unique. A trusted user recipe replaces the built-in it was
/// approved over, unless the built-in has since moved to a higher
/// `revision`; any other clash keeps the first one and warns.
fn merge(builtins: Vec<Stored>, files: Vec<Stored>) -> Vec<Stored> {
    let mut all = builtins;
    for f in files {
        if f.origin == Origin::User {
            if let Some(i) = all.iter().position(|s| s.recipe.name == f.recipe.name && s.origin == Origin::Builtin) {
                if all[i].recipe.revision > f.recipe.revision {
                    log::info!("built-in {} revision {} is newer than the user recipe's {}; using the built-in", f.recipe.name, all[i].recipe.revision, f.recipe.revision);
                } else {
                    all[i] = f;
                }
                continue;
            }
        }
        if all.iter().any(|s| s.recipe.name == f.recipe.name) {
            log::warn!("recipe {} ({:?}) shadows an existing one; skipped", f.recipe.name, f.origin);
            continue;
        }
        all.push(f);
    }
    all
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

/// How a recipe a client brought differs from the one stored under its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Change {
    /// Roadie has no recipe of that name.
    New,
    /// It would replace the built-in recipe.
    ReplacesBuiltin,
    /// It would replace a recipe the user trusted earlier.
    ChangesTrusted,
    /// Only an unreviewed draft has that name.
    ReplacesDraft,
}

/// `None` when `recipe` is exactly the trusted one: nothing to review.
pub fn compare(recipe: &Recipe) -> Option<Change> {
    match get(&recipe.name) {
        None => Some(Change::New),
        Some(s) if s.trusted() && same(&s.recipe, recipe) => None,
        Some(s) => Some(match s.origin {
            Origin::Builtin => Change::ReplacesBuiltin,
            Origin::User => Change::ChangesTrusted,
            Origin::Draft => Change::ReplacesDraft,
        }),
    }
}

/// Equal as recipes: compared as parsed values, so key order and
/// whitespace in the client's file do not count as a change.
pub fn same(a: &Recipe, b: &Recipe) -> bool {
    serde_json::to_value(a).ok() == serde_json::to_value(b).ok()
}

/// Top-level recipe keys whose values differ, for "what changed" lines.
pub fn changed_keys(old: &Recipe, new: &Recipe) -> Vec<String> {
    let (Ok(serde_json::Value::Object(a)), Ok(serde_json::Value::Object(b))) = (serde_json::to_value(old), serde_json::to_value(new)) else {
        return vec![];
    };
    let mut keys: Vec<String> = a.keys().chain(b.keys()).filter(|k| a.get(*k) != b.get(*k)).cloned().collect();
    keys.dedup();
    let mut seen = std::collections::HashSet::new();
    keys.retain(|k| seen.insert(k.clone()));
    keys
}

/// The user approved a request that brought this recipe: save it as a
/// trusted user recipe, replacing a draft, a user recipe or the built-in of
/// the same name. Only `actions` calls this, from an approval.
pub fn put_trusted(recipe: Recipe) -> Result<Stored, PutError> {
    let errors = super::validate(&recipe);
    if !errors.is_empty() {
        return Err(PutError::Invalid(errors));
    }
    let dir = paths::recipes_dir().map_err(PutError::Io)?;
    let text = serde_json::to_string_pretty(&recipe).map_err(|e| PutError::Io(e.to_string()))?;
    paths::write_atomic(&dir.join(format!("{}.json", recipe.name)), text.as_bytes(), false).map_err(PutError::Io)?;
    let _ = std::fs::remove_file(dir.join(format!("{}.draft.json", recipe.name)));
    let stored = Stored { recipe, origin: Origin::User, submitted_by: None };
    let mut all = store().write().unwrap();
    match all.iter().position(|s| s.recipe.name == stored.recipe.name) {
        Some(i) => all[i] = stored.clone(),
        None => all.push(stored.clone()),
    }
    Ok(stored)
}

/// Remove a user or draft recipe (built-ins cannot be removed). A user
/// recipe that replaced a built-in gives the name back to the built-in.
pub fn delete(name: &str) -> Result<(), String> {
    let stored = get(name).ok_or_else(|| format!("unknown recipe: {name}"))?;
    if stored.origin == Origin::Builtin {
        return Err("built-in recipes cannot be deleted".into());
    }
    let dir = paths::recipes_dir()?;
    let _ = std::fs::remove_file(dir.join(format!("{name}.json")));
    let _ = std::fs::remove_file(dir.join(format!("{name}.draft.json")));
    let mut all = store().write().unwrap();
    all.retain(|s| s.recipe.name != name);
    if let Some(builtin) = super::load_builtin().into_iter().find(|r| r.name == name) {
        all.push(Stored { recipe: builtin, origin: Origin::Builtin, submitted_by: None });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored(recipe: Recipe, origin: Origin) -> Stored {
        Stored { recipe, origin, submitted_by: None }
    }

    #[test]
    fn a_user_recipe_replaces_its_builtin_until_the_builtin_moves_past_it() {
        let builtin = super::super::load_builtin().into_iter().find(|r| r.name == "ffmpeg").unwrap();
        let mut theirs = builtin.clone();
        theirs.summary = "a client's ffmpeg".into();
        let merged = merge(vec![stored(builtin.clone(), Origin::Builtin)], vec![stored(theirs.clone(), Origin::User)]);
        assert_eq!((merged.len(), merged[0].origin, merged[0].recipe.summary.as_str()), (1, Origin::User, "a client's ffmpeg"));

        let mut older = theirs.clone();
        older.revision = builtin.revision - 1;
        let merged = merge(vec![stored(builtin.clone(), Origin::Builtin)], vec![stored(older, Origin::User)]);
        assert_eq!(merged[0].origin, Origin::Builtin, "a newer built-in wins");

        let merged = merge(vec![stored(builtin.clone(), Origin::Builtin)], vec![stored(theirs, Origin::Draft)]);
        assert_eq!((merged.len(), merged[0].origin), (1, Origin::Builtin), "a draft never replaces anything");
    }

    #[test]
    fn a_brought_recipe_is_compared_then_trusted() {
        let root = std::env::temp_dir().join(format!("roadie-store-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        paths::init(root);
        let builtin = super::super::load_builtin().into_iter().find(|r| r.name == "ffmpeg").unwrap();
        if get("ffmpeg").is_none() {
            load_all();
        }
        assert_eq!(compare(&get("ffmpeg").unwrap().recipe), None, "the stored recipe itself needs no review");

        let mut theirs = builtin.clone();
        theirs.name = "store-test-brought".into();
        assert_eq!(compare(&theirs), Some(Change::New));
        put_trusted(theirs.clone()).unwrap();
        assert_eq!(get("store-test-brought").unwrap().origin, Origin::User);
        assert_eq!(compare(&theirs), None, "once trusted, the same file is not a change");

        let mut changed = theirs.clone();
        changed.summary = "changed".into();
        assert_eq!(compare(&changed), Some(Change::ChangesTrusted));
        assert_eq!(changed_keys(&theirs, &changed), vec!["summary".to_string()]);
        delete("store-test-brought").unwrap();
        assert!(get("store-test-brought").is_none());
    }
}
