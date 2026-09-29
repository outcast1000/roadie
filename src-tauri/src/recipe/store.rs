//! Every recipe Roadie knows, with where it came from. User recipes live in
//! `recipes/<name>.json`; anything that arrived through the API is a
//! **draft** (`recipes/<name>.draft.json`) until the user reads it in the
//! review screen and clicks Trust; the recipe catalog (`catalog.rs`) offers
//! the rest. Only user recipes are trusted, so only they can be installed.
//! Names are unique: a user recipe or a draft owns its name, and a catalog
//! entry of the same name is not listed.
//!
//! A client may also *bring* a recipe with an install or update request
//! (`compare` says how it differs from what is stored), and a catalog recipe
//! is installed the same way. It is not saved anywhere until the user
//! approves that request, which trusts it here (`put_trusted`), recording
//! where it came from: a catalog recipe's file is an envelope
//! `{"source": "catalog", "recipe": …}`; a plain file is the user's own.

use super::{catalog, Recipe, ValidationError};
use crate::paths;
use serde::Serialize;
use std::sync::{OnceLock, RwLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// No longer occurs (Roadie ships no recipes); kept so the wire value
    /// stays reserved.
    Builtin,
    User,
    Draft,
    /// Offered by the recipe catalog, not trusted: installing it asks.
    Catalog,
}

/// Where a trusted user recipe came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RecipeSource {
    /// Written by the user or an app they approved.
    #[default]
    User,
    /// Approved from the recipe catalog; recipe updates come from there.
    Catalog,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Stored {
    pub recipe: Recipe,
    pub origin: Origin,
    /// Who submitted a draft (an API consumer's display name), for the review.
    pub submitted_by: Option<String>,
    pub source: RecipeSource,
}

impl Stored {
    pub fn trusted(&self) -> bool {
        matches!(self.origin, Origin::User | Origin::Builtin)
    }
    fn new(recipe: Recipe, origin: Origin) -> Self {
        let source = if origin == Origin::Catalog { RecipeSource::Catalog } else { RecipeSource::User };
        Stored { recipe, origin, submitted_by: None, source }
    }
}

struct All {
    stored: Vec<Stored>,
    /// The catalog as last loaded, including entries a user recipe shadows
    /// (for recipe updates and for giving a name back on delete).
    catalog: Vec<catalog::Listed>,
    /// Every name the cached index lists (`None`: never fetched).
    index_names: Option<Vec<String>>,
}

fn store() -> &'static RwLock<All> {
    static S: OnceLock<RwLock<All>> = OnceLock::new();
    S.get_or_init(|| RwLock::new(All { stored: Vec::new(), catalog: Vec::new(), index_names: None }))
}

/// What is on disk (after `legacy::migrate`), then the cached catalog. Invalid files are skipped with
/// a warning rather than failing startup. Does not fetch. Reads under the
/// write lock, so a concurrent `put_trusted` is never overwritten by a view
/// read before it.
pub fn load_all() {
    let mut guard = store().write().unwrap();
    if let Ok(root) = paths::data_root() {
        super::legacy::migrate(root);
    }
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
                let envelope = split_envelope(&text);
                match super::parse(&envelope.recipe) {
                    Ok(recipe) if recipe.name == stem => {
                        files.push(Stored { recipe, origin, submitted_by: envelope.submitted_by, source: if origin == Origin::User { envelope.source } else { RecipeSource::User } })
                    }
                    Ok(r) => log::warn!("recipe file {} is named {} inside; skipped", path.display(), r.name),
                    Err(e) => log::warn!("recipe file {} is invalid: {e:?}", path.display()),
                }
            }
        }
    }
    let catalog = catalog::load();
    let stored = merge(files, &catalog);
    *guard = All { stored, catalog, index_names: catalog::index_names() };
}

/// Fetch the catalog now and reload. The cache survives a failure.
pub fn refresh_catalog() -> Result<usize, String> {
    let r = catalog::refresh();
    load_all();
    r
}

/// Fetch the catalog if the cache is stale, and reload when it did.
pub fn refresh_catalog_if_stale() {
    if catalog::refresh_if_stale() {
        load_all();
    }
}

/// Names are unique: a user recipe beats a draft of its name (approving a
/// brought recipe removes the draft, so both exist only by hand), and
/// either beats a catalog entry.
fn merge(files: Vec<Stored>, catalog: &[catalog::Listed]) -> Vec<Stored> {
    let mut all: Vec<Stored> = Vec::new();
    for f in files {
        match all.iter().position(|s| s.recipe.name == f.recipe.name) {
            Some(i) if all[i].origin == Origin::Draft && f.origin == Origin::User => all[i] = f,
            Some(_) => log::warn!("recipe {} ({:?}) shadows an existing one; skipped", f.recipe.name, f.origin),
            None => all.push(f),
        }
    }
    for l in catalog {
        if !all.iter().any(|s| s.recipe.name == l.recipe.name) {
            all.push(Stored::new(l.recipe.clone(), Origin::Catalog));
        }
    }
    all
}

struct Envelope {
    recipe: String,
    submitted_by: Option<String>,
    source: RecipeSource,
}

/// Drafts are wrapped in `{"submittedBy": …, "recipe": {…}}` so the review
/// can say who wrote them, and catalog recipes in `{"source": "catalog",
/// "recipe": {…}}`; plain recipe files are accepted too.
fn split_envelope(text: &str) -> Envelope {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
        if let Some(recipe) = v.get("recipe") {
            return Envelope {
                recipe: recipe.to_string(),
                submitted_by: v.get("submittedBy").and_then(|b| b.as_str()).map(str::to_string),
                source: if v.get("source").and_then(|s| s.as_str()) == Some("catalog") { RecipeSource::Catalog } else { RecipeSource::User },
            };
        }
    }
    Envelope { recipe: text.to_string(), submitted_by: None, source: RecipeSource::User }
}

pub fn list() -> Vec<Stored> {
    store().read().unwrap().stored.clone()
}

pub fn get(name: &str) -> Option<Stored> {
    store().read().unwrap().stored.iter().find(|s| s.recipe.name == name).cloned()
}

/// The catalog's entry for `name`, even when a user recipe shadows it.
pub fn catalog_entry(name: &str) -> Option<catalog::Listed> {
    store().read().unwrap().catalog.iter().find(|l| l.recipe.name == name).cloned()
}

/// A newer revision of a trusted catalog recipe, waiting for the user's
/// review. It is never applied without it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecipeUpdate {
    pub revision: u32,
    /// Top-level recipe keys that change.
    pub changed_keys: Vec<String>,
    #[serde(skip)]
    pub recipe: Recipe,
}

/// The catalog's newer revision of `name`, when the user's recipe came from
/// the catalog and the catalog has moved past it.
pub fn recipe_update(name: &str) -> Option<RecipeUpdate> {
    let s = get(name).filter(|s| s.origin == Origin::User && s.source == RecipeSource::Catalog)?;
    let l = catalog_entry(name).filter(|l| l.recipe.revision > s.recipe.revision)?;
    Some(RecipeUpdate { revision: l.recipe.revision, changed_keys: changed_keys(&s.recipe, &l.recipe), recipe: l.recipe })
}

/// A trusted catalog recipe the catalog no longer lists. It keeps working;
/// it just gets no recipe updates.
pub fn delisted(name: &str) -> bool {
    let all = store().read().unwrap();
    let from_catalog = all.stored.iter().any(|s| s.recipe.name == name && s.origin == Origin::User && s.source == RecipeSource::Catalog);
    from_catalog && all.index_names.as_ref().is_some_and(|names| !names.iter().any(|n| n == name))
}

/// Only trusted recipes are eligible for install/start.
pub fn get_trusted(name: &str) -> Result<Recipe, String> {
    match get(name) {
        Some(s) if s.trusted() => Ok(s.recipe),
        Some(s) if s.origin == Origin::Catalog => Err(format!("recipe {name} comes from the recipe catalog — install it to review it")),
        Some(_) => Err(format!("recipe {name} is a draft — review and trust it in Roadie first")),
        None => Err(format!("unknown tool: {name}")),
    }
}

#[derive(Debug)]
pub enum PutError {
    Invalid(Vec<ValidationError>),
    /// A trusted user recipe already owns the name.
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
        if existing.trusted() {
            return Err(PutError::Conflict(format!("a trusted recipe named {} already exists", recipe.name)));
        }
    }
    let dir = paths::recipes_dir().map_err(PutError::Io)?;
    let envelope = serde_json::json!({ "submittedBy": submitted_by, "recipe": recipe });
    let text = serde_json::to_string_pretty(&envelope).map_err(|e| PutError::Io(e.to_string()))?;
    paths::write_atomic(&dir.join(format!("{}.draft.json", recipe.name)), text.as_bytes(), false).map_err(PutError::Io)?;
    let stored = Stored { recipe, origin: Origin::Draft, submitted_by, source: RecipeSource::User };
    let all = &mut store().write().unwrap().stored;
    all.retain(|s| s.recipe.name != stored.recipe.name);
    all.push(stored.clone());
    Ok(stored)
}

/// The user read the draft, or a catalog recipe, in the review screen and
/// accepts it: it becomes a user recipe (a catalog one remembers its source,
/// so recipe updates keep coming from the catalog).
pub fn trust(name: &str) -> Result<Stored, String> {
    let stored = get(name).ok_or_else(|| format!("unknown recipe: {name}"))?;
    match stored.origin {
        Origin::Draft => {}
        Origin::Catalog => {
            return put_trusted(stored.recipe, RecipeSource::Catalog).map_err(|e| match e {
                PutError::Invalid(errors) => format!("recipe {name} is invalid: {errors:?}"),
                PutError::Conflict(m) | PutError::Io(m) => m,
            })
        }
        _ => return Ok(stored),
    }
    let dir = paths::recipes_dir()?;
    let text = serde_json::to_string_pretty(&stored.recipe).map_err(|e| e.to_string())?;
    paths::write_atomic(&dir.join(format!("{name}.json")), text.as_bytes(), false)?;
    let _ = std::fs::remove_file(dir.join(format!("{name}.draft.json")));
    let trusted = Stored { origin: Origin::User, ..stored };
    let all = &mut store().write().unwrap().stored;
    all.retain(|s| s.recipe.name != name);
    all.push(trusted.clone());
    Ok(trusted)
}

/// How a recipe a client brought differs from the one stored under its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Change {
    /// Roadie has no trusted recipe of that name (the catalog may offer it).
    New,
    /// No longer occurs (Roadie ships no recipes); kept so the wire value
    /// stays reserved.
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
            Origin::Catalog => Change::New,
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
/// trusted user recipe from `source`, replacing a draft, a user recipe or
/// the catalog entry of the same name. Only `actions` calls this, from an
/// approval.
pub fn put_trusted(recipe: Recipe, source: RecipeSource) -> Result<Stored, PutError> {
    let errors = super::validate(&recipe);
    if !errors.is_empty() {
        return Err(PutError::Invalid(errors));
    }
    let dir = paths::recipes_dir().map_err(PutError::Io)?;
    let text = match source {
        RecipeSource::User => serde_json::to_string_pretty(&recipe),
        RecipeSource::Catalog => serde_json::to_string_pretty(&serde_json::json!({ "source": "catalog", "recipe": recipe })),
    }
    .map_err(|e| PutError::Io(e.to_string()))?;
    paths::write_atomic(&dir.join(format!("{}.json", recipe.name)), text.as_bytes(), false).map_err(PutError::Io)?;
    let _ = std::fs::remove_file(dir.join(format!("{}.draft.json", recipe.name)));
    let stored = Stored { recipe, origin: Origin::User, submitted_by: None, source };
    let all = &mut store().write().unwrap().stored;
    match all.iter().position(|s| s.recipe.name == stored.recipe.name) {
        Some(i) => all[i] = stored.clone(),
        None => all.push(stored.clone()),
    }
    Ok(stored)
}

/// Remove a user or draft recipe. A catalog entry has nothing on disk to
/// remove; a user recipe that shadowed one gives the name back to it.
pub fn delete(name: &str) -> Result<(), String> {
    let stored = get(name).ok_or_else(|| format!("unknown recipe: {name}"))?;
    if !matches!(stored.origin, Origin::User | Origin::Draft) {
        return Err(format!("recipe {name} comes from the recipe catalog; there is nothing to delete"));
    }
    let dir = paths::recipes_dir()?;
    let _ = std::fs::remove_file(dir.join(format!("{name}.json")));
    let _ = std::fs::remove_file(dir.join(format!("{name}.draft.json")));
    let mut guard = store().write().unwrap();
    let All { stored: all, catalog, .. } = &mut *guard;
    all.retain(|s| s.recipe.name != name);
    if let Some(l) = catalog.iter().find(|l| l.recipe.name == name) {
        all.push(Stored::new(l.recipe.clone(), Origin::Catalog));
    }
    Ok(())
}

/// Tests: point the process-wide data root at a temp dir (the first call
/// wins; every test module shares it).
#[cfg(test)]
pub fn test_root() {
    let root = std::env::temp_dir().join(format!("roadie-store-test-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    paths::init(root);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recipe::fixtures;

    fn stored(recipe: Recipe, origin: Origin) -> Stored {
        Stored::new(recipe, origin)
    }

    fn listed(recipe: Recipe) -> catalog::Listed {
        let entry = catalog::Entry {
            name: recipe.name.clone(),
            summary: String::new(),
            kind: "cli".into(),
            revision: recipe.revision,
            platforms: vec![],
            min_roadie: "0.0.0".into(),
            path: format!("recipes/{}.json", recipe.name),
            sha256: String::new(),
        };
        catalog::Listed { entry, recipe }
    }

    #[test]
    fn a_catalog_entry_is_listed_untrusted_unless_a_file_owns_its_name() {
        let ffmpeg = fixtures::recipe("ffmpeg");
        let ytdlp = fixtures::recipe("yt-dlp");
        let mut mine = ffmpeg.clone();
        mine.summary = "my ffmpeg".into();
        let merged = merge(vec![stored(mine, Origin::User)], &[listed(ffmpeg.clone()), listed(ytdlp.clone())]);
        let by = |n: &str| merged.iter().find(|s| s.recipe.name == n).unwrap();
        assert_eq!((merged.len(), by("ffmpeg").origin, by("ffmpeg").recipe.summary.as_str()), (2, Origin::User, "my ffmpeg"), "a user recipe shadows the catalog");
        assert_eq!(by("yt-dlp").origin, Origin::Catalog);
        assert!(!by("yt-dlp").trusted(), "a catalog recipe is never trusted by arriving");

        let merged = merge(vec![stored(ytdlp.clone(), Origin::Draft)], &[listed(ytdlp.clone())]);
        assert_eq!((merged.len(), merged[0].origin), (1, Origin::Draft), "a draft owns its name too");
        let merged = merge(vec![stored(ytdlp.clone(), Origin::Draft), stored(ytdlp, Origin::User)], &[]);
        assert_eq!((merged.len(), merged[0].origin), (1, Origin::User), "a user recipe beats a draft");
    }

    #[test]
    fn envelopes_carry_who_and_where_from() {
        let e = split_envelope(r#"{"source":"catalog","recipe":{"name":"x"}}"#);
        assert_eq!((e.source, e.submitted_by, e.recipe.as_str()), (RecipeSource::Catalog, None, r#"{"name":"x"}"#));
        let e = split_envelope(r#"{"submittedBy":"An App","recipe":{"name":"x"}}"#);
        assert_eq!((e.source, e.submitted_by.as_deref()), (RecipeSource::User, Some("An App")));
        let e = split_envelope(r#"{"name":"x"}"#);
        assert_eq!((e.source, e.recipe.as_str()), (RecipeSource::User, r#"{"name":"x"}"#), "a plain file is the user's own");
    }

    #[test]
    fn a_brought_recipe_is_compared_then_trusted() {
        test_root();
        let mut theirs = fixtures::recipe("ffmpeg");
        theirs.name = "store-test-brought".into();
        assert_eq!(compare(&theirs), Some(Change::New));
        put_trusted(theirs.clone(), RecipeSource::User).unwrap();
        assert_eq!(get("store-test-brought").unwrap().origin, Origin::User);
        assert_eq!(compare(&theirs), None, "once trusted, the same file is not a change");

        let mut changed = theirs.clone();
        changed.summary = "changed".into();
        assert_eq!(compare(&changed), Some(Change::ChangesTrusted));
        assert_eq!(changed_keys(&theirs, &changed), vec!["summary".to_string()]);
        delete("store-test-brought").unwrap();
        assert!(get("store-test-brought").is_none());
    }

    #[test]
    fn a_catalog_recipe_is_installed_as_brought_and_given_back_on_delete() {
        let _s = catalog::tests::serial();
        test_root();
        let mut r = fixtures::recipe("yt-dlp");
        r.name = "store-test-cat".into();
        let bytes = serde_json::to_vec_pretty(&r).unwrap();
        let entry = catalog::Entry { sha256: crate::tools::install::sha256_hex(&bytes), ..listed(r.clone()).entry };
        let index = serde_json::to_vec(&serde_json::json!({ "indexVersion": 1, "recipes": [entry] })).unwrap();
        let fetch = |url: &str| match url.strip_prefix(catalog::BASE_URL) {
            Some("index.json") => Ok(index.clone()),
            Some("recipes/store-test-cat.json") => Ok(bytes.clone()),
            _ => Err(format!("fetch {url}: HTTP 404")),
        };
        catalog::refresh_with(&fetch).unwrap();
        load_all();
        let s = get("store-test-cat").unwrap();
        assert_eq!((s.origin, s.trusted()), (Origin::Catalog, false));
        let err = get_trusted("store-test-cat").unwrap_err();
        assert!(err.contains("recipe catalog") && err.contains("install it"), "{err}");
        assert_eq!(compare(&r), Some(Change::New), "a catalog recipe is reviewed like a brought one");
        assert!(delete("store-test-cat").is_err(), "nothing on disk to delete");

        let t = trust("store-test-cat").unwrap();
        assert_eq!((t.origin, t.source), (Origin::User, RecipeSource::Catalog), "trusting it in the review screen keeps its source");
        load_all();
        let s = get("store-test-cat").unwrap();
        assert_eq!((s.origin, s.source), (Origin::User, RecipeSource::Catalog), "the envelope's source survives a reload");
        assert_eq!(compare(&r), None);
        assert!(catalog_entry("store-test-cat").is_some(), "the shadowed entry stays known");

        delete("store-test-cat").unwrap();
        assert_eq!(get("store-test-cat").map(|s| s.origin), Some(Origin::Catalog), "the name goes back to the catalog");
        catalog::refresh_with(&|_: &str| Ok(br#"{"indexVersion":1,"recipes":[]}"#.to_vec())).unwrap();
        load_all();
        assert!(get("store-test-cat").is_none());
    }
}
