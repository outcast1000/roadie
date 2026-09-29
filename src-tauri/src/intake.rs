//! What a client's ask becomes, shared by both releases: the desktop
//! service's API handlers and the standalone CLI call these, so the two
//! refuse the same things for the same reasons and build the same requests.
//! An install, upgrade or uninstall ask is checked here and turned into a
//! `RequestKind` for the user to approve. The API maps a `Refusal` to an
//! HTTP status; the CLI maps it to a message and an exit code.

use crate::recipe::store::{self, Change, RecipeSource};
use crate::recipe::{self, Recipe};
use crate::requests::{self, RequestKind};
use crate::{consent, paths, prompt, tools};
use serde_json::{json, Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// The ask is malformed or names the wrong thing (HTTP 400).
    BadRequest,
    /// No such tool (404).
    NotFound,
    /// Not possible in the current state: a draft, not installed (409).
    Conflict,
    /// The recipe or a value is invalid (422).
    Invalid,
}

#[derive(Debug, Clone)]
pub struct Refusal {
    pub kind: Refused,
    pub message: String,
    /// Extra fields for the API's error body (`errors`, `missing`).
    pub extra: Value,
}

impl Refusal {
    fn new(kind: Refused, message: impl Into<String>) -> Self {
        Refusal { kind, message: message.into(), extra: Value::Null }
    }
    fn with(mut self, extra: Value) -> Self {
        self.extra = extra;
        self
    }
}

/// The trusted recipe named `name`: unknown is `NotFound`, a draft `Conflict`.
pub fn trusted(name: &str) -> Result<Recipe, Refusal> {
    store::get_trusted(name).map_err(|e| Refusal::new(if e.starts_with("unknown") { Refused::NotFound } else { Refused::Conflict }, e))
}

/// A recipe a client sent with an install or update: valid, named like the
/// tool it is for, and compared with the stored one. `None` as the change
/// means it is exactly the trusted recipe, so nothing needs reviewing. A
/// recipe identical to the catalog's counts as the catalog's, so recipe
/// updates keep coming from there.
pub fn brought_recipe(name: &str, v: Value) -> Result<(Recipe, Option<Change>, RecipeSource), Refusal> {
    let recipe = recipe::from_value(v).map_err(|errors| {
        Refusal::new(Refused::Invalid, "the recipe you sent is invalid; each error names a JSON pointer inside /recipe").with(json!({ "errors": errors }))
    })?;
    if recipe.name != name {
        return Err(Refusal::new(Refused::BadRequest, format!("the ask names {name} but /recipe/name is {}; use the same name", recipe.name)));
    }
    let change = store::compare(&recipe);
    let source = if store::catalog_entry(name).is_some_and(|l| store::same(&l.recipe, &recipe)) { RecipeSource::Catalog } else { RecipeSource::User };
    Ok((recipe, change, source))
}

/// The recipe an ask that brought none installs: the trusted one, or the
/// catalog's newer revision of it, or the catalog's when Roadie has none.
/// The last two ride in the request for review, exactly as a brought recipe.
fn own_or_catalog(name: &str) -> Result<(Recipe, Option<Change>, RecipeSource), Refusal> {
    if let Some(s) = store::get(name).filter(|s| s.origin == store::Origin::Catalog) {
        return Ok((s.recipe, Some(Change::New), RecipeSource::Catalog));
    }
    let recipe = trusted(name)?;
    Ok(match store::recipe_update(name) {
        Some(u) => (u.recipe, Some(Change::ChangesTrusted), RecipeSource::Catalog),
        None => (recipe, None, RecipeSource::User),
    })
}

/// An install ask: the caller's decisions (`askOnInstall` values and the
/// engine's `startNow`/`autostart`), a consumer to grant with the same
/// click, and optionally the recipe the caller ships.
#[derive(Debug, Clone, Default)]
pub struct InstallAsk {
    pub values: Map<String, Value>,
    pub consumer: Option<String>,
    pub recipe: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct InstallPlan {
    pub kind: RequestKind,
    /// Another copy already running here (`tools::other_instance`): a
    /// warning for the caller; the prompt shows it too.
    pub other_instance: Option<tools::OtherInstance>,
    /// Every decision the recipe asks for and whether it is settled.
    pub decisions: Vec<Value>,
    pub recipe_change: Option<Change>,
}

/// Check an install ask and build its request. The consumer must be
/// registered (the CLI registers the caller first; the API never
/// auto-registers). Where nothing on screen can ask for values, a missing
/// required one is refused here (`prompt::refuse_missing`).
pub fn install(name: &str, ask: InstallAsk) -> Result<InstallPlan, Refusal> {
    let InstallAsk { values, consumer, recipe: brought } = ask;
    let (recipe, proposed, source) = match brought {
        Some(v) => brought_recipe(name, v)?,
        None => own_or_catalog(name)?,
    };
    if let Some(c) = &consumer {
        if consent::get(c).is_none() {
            return Err(Refusal::new(Refused::BadRequest, format!("unknown consumer `{c}`; register it first")));
        }
        if recipe.connection.as_ref().map(|c| c.policy).unwrap_or(recipe::ConnectionPolicy::None) == recipe::ConnectionPolicy::None {
            return Err(Refusal::new(Refused::BadRequest, format!("{} exposes no connection; drop the consumer", recipe.display_name)));
        }
    }
    let mut config_only = values.clone();
    tools::install_options(&recipe, &mut config_only).map_err(|e| Refusal::new(Refused::Invalid, format!("config: {e}")))?;
    tools::state::validate_patch(&recipe, &config_only).map_err(|e| Refusal::new(Refused::Invalid, format!("config: {e}")))?;
    let current = tools::status(&recipe).config;
    if let Some((message, missing)) = prompt::refuse_missing(&recipe, &values, &current, prompt::surface()) {
        return Err(Refusal::new(Refused::Invalid, message).with(json!({ "missing": missing })));
    }
    let secrets_missing = missing_secrets(&recipe, &values);
    if !secrets_missing.is_empty() && prompt::surface() != prompt::Surface::Window {
        let example = secrets_missing.iter().map(|k| format!("--set {k}=…")).collect::<Vec<_>>().join(" ");
        return Err(Refusal::new(Refused::Invalid, format!("{} needs {} and generates none: pass it with the request (CLI: `roadie tool install {} {example}`)", recipe.display_name, secrets_missing.join(", "), recipe.name))
            .with(json!({ "missing": secrets_missing })));
    }
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
    // The engine's own decisions a recipe offers: where it installs, its
    // ports, the secrets an app may choose. Each shows its default.
    let st = tools::status(&recipe);
    decisions.push(json!({ "key": "installDir", "label": "Install folder", "kind": "path", "required": false, "settled": true,
        "value": values.get("installDir").cloned().unwrap_or_else(|| Value::String(st.versions_dir.clone())) }));
    for (name, def) in recipe.ports.iter().filter(|(_, d)| d.ask_on_install) {
        let key = format!("{}{name}", recipe::PORT_DECISION);
        decisions.push(json!({ "key": key, "label": def.label, "kind": "port", "required": false, "settled": true, "default": def.default,
            "value": values.get(&key).cloned().unwrap_or(json!(def.default)) }));
    }
    for s in recipe.secrets.iter().filter(|s| s.ask_on_install) {
        let key = format!("{}{}", recipe::SECRET_DECISION, s.key);
        // The value never echoes back; `given` says whether the app chose one.
        decisions.push(json!({ "key": key, "label": s.label, "kind": "password", "required": false, "settled": true, "minLen": s.min_len(), "given": values.contains_key(&key) }));
    }
    for (key, label, offer) in [("startNow", "Start now, right after installing", recipe.start_after_install), ("autostart", "Start at login", recipe.autostart)] {
        if let Some(o) = offer {
            decisions.push(json!({
                "key": key, "label": label, "kind": "bool", "required": false, "default": o.default,
                "settled": true, "value": values.get(key).and_then(|v| v.as_bool()).unwrap_or(o.default),
            }));
        }
    }
    let chosen = chosen_ports(&values);
    let other_instance = tools::other_instance(&recipe, &chosen);
    Ok(InstallPlan { kind: requests::install_kind(&recipe, values, consumer, proposed, source), decisions, recipe_change: proposed, other_instance })
}

/// The `ports.<name>` an ask chose.
pub fn chosen_ports(values: &Map<String, Value>) -> std::collections::BTreeMap<String, u16> {
    values
        .iter()
        .filter_map(|(k, v)| {
            let name = k.strip_prefix(recipe::PORT_DECISION)?;
            let n = v.as_u64().or_else(|| v.as_str().and_then(|t| t.trim().parse().ok()))?;
            u16::try_from(n).ok().map(|n| (name.to_string(), n))
        })
        .collect()
}

pub enum UpdatePlan {
    /// The caller brought a different recipe: the user reviews and trusts
    /// it, and approving updates the tool.
    Review { kind: RequestKind, change: Change },
    /// Update now with the trusted recipe (`update_now`).
    Now(Box<Recipe>),
}

/// An upgrade ask, optionally with the caller's recipe. Without one, a
/// pending recipe update from the catalog is what gets reviewed; approving
/// it trusts the new revision and updates the tool.
pub fn update(name: &str, brought: Option<Value>) -> Result<UpdatePlan, Refusal> {
    let proposal = match brought {
        Some(v) => Some(brought_recipe(name, v)?),
        None => store::recipe_update(name).map(|u| (u.recipe, Some(Change::ChangesTrusted), RecipeSource::Catalog)),
    };
    if let Some((recipe, Some(change), source)) = proposal {
        let installed = store::get_trusted(name).map(|current| tools::status(&current).installed).unwrap_or(false);
        if !installed {
            return Err(Refusal::new(Refused::Conflict, format!("{} is not installed; install it with this recipe instead", recipe.display_name)));
        }
        let kind = RequestKind::ReplaceRecipe { tool: name.to_string(), recipe: Box::new(recipe), recipe_change: change, recipe_source: Some(source) };
        return Ok(UpdatePlan::Review { kind, change });
    }
    Ok(UpdatePlan::Now(Box::new(trusted(name)?)))
}

/// Look for a newer release and install it (staged when a daemon is busy).
pub fn update_now(recipe: &Recipe, progress: tools::Progress) -> Result<tools::ToolStatus, String> {
    if !tools::status(recipe).installed {
        return Err(format!("{} is not installed", recipe.display_name));
    }
    tools::check_updates(recipe)?;
    tools::install(recipe, progress)
}

pub fn uninstall(name: &str, keep_data: bool) -> Result<RequestKind, Refusal> {
    trusted(name)?;
    Ok(RequestKind::Uninstall { tool: name.to_string(), keep_data })
}

/// A tool's status as clients see it: no pid, plus where its recipe came
/// from (`origin`, and for a user recipe `source`), whether it is trusted,
/// whether the catalog offers it (`available`), a pending `recipeUpdate`
/// and `delisted`.
pub fn public_status(s: tools::ToolStatus, stored: &store::Stored) -> Value {
    let name = stored.recipe.name.as_str();
    let mut v = serde_json::to_value(s).unwrap_or_default();
    if let Some(o) = v.as_object_mut() {
        o.insert("origin".into(), serde_json::to_value(stored.origin).unwrap_or_default());
        o.insert("trusted".into(), Value::Bool(stored.trusted()));
        o.insert("source".into(), serde_json::to_value(stored.source).unwrap_or_default());
        o.insert("available".into(), Value::Bool(store::catalog_entry(name).is_some()));
        o.insert("recipeUpdate".into(), store::recipe_update(name).map(|u| serde_json::to_value(u).unwrap_or_default()).unwrap_or(Value::Null));
        o.insert("delisted".into(), Value::Bool(store::delisted(name)));
        o.remove("pid");
    }
    v
}

/// Every value an install of `recipe` can be given, in one shape, so a
/// client can show them to its user before it asks to install: the recipe's
/// config fields and configuration entries, its ports and secrets, and the
/// engine's own decisions (`installDir`, `startNow`, `autostart`). Each is
/// `required` or not; `default` is expanded for this computer, `generated`
/// says Roadie makes the value when none is given, and `value` is what an
/// installed tool has now (never a secret: `set` says whether it has one).
/// `askOnInstall` marks what Roadie's own prompt asks the user.
pub fn options(recipe: &Recipe) -> Value {
    let st = tools::status(recipe);
    let p = paths::tool_paths(&recipe.name).ok();
    let state = p.as_ref().map(|p| tools::state::load(&p.data)).unwrap_or_default();
    let defaults = p.as_ref().map(|p| tools::state::preview(recipe, &p.data, &recipe::Platform::current())).unwrap_or_default();
    let installed = st.installed;
    let mut out = Vec::new();
    for f in recipe.fields() {
        let mut o = json!({
            "key": f.key, "label": f.label, "help": f.help, "kind": f.kind, "required": f.required, "secret": f.secret,
            "default": if f.secret { Value::Null } else { defaults.config.get(&f.key).cloned().or(f.default.clone()).unwrap_or(Value::Null) },
            "generated": false, "askOnInstall": f.ask_on_install, "settable": st.configurable || !installed,
        });
        if installed {
            o["value"] = if f.secret { Value::Null } else { state.config.get(&f.key).cloned().unwrap_or(Value::Null) };
            o["set"] = json!(if f.secret { state.secrets.contains_key(&f.key) } else { state.config.contains_key(&f.key) });
        }
        out.push(o);
    }
    for (name, def) in &recipe.ports {
        let mut o = json!({
            "key": format!("{}{name}", recipe::PORT_DECISION), "label": def.label.clone().unwrap_or_else(|| format!("{name} port")), "kind": "port",
            "required": false, "secret": false, "default": def.default, "generated": false, "askOnInstall": def.ask_on_install,
            "help": if def.pick { "Roadie picks the next free port when this one is taken, unless you choose it." } else { "Used as is." },
            "settable": !installed,
        });
        if installed {
            o["value"] = json!(state.ports.get(name));
        }
        out.push(o);
    }
    for s in &recipe.secrets {
        let mut o = json!({
            "key": format!("{}{}", recipe::SECRET_DECISION, s.key), "label": s.label.clone().unwrap_or_else(|| s.key.clone()), "kind": "secret",
            "required": s.required(), "secret": true, "default": Value::Null, "generated": !s.required(), "minLen": s.min_len(),
            "askOnInstall": s.ask_on_install, "settable": !installed,
        });
        if installed {
            o["set"] = json!(state.secrets.contains_key(&s.key));
        }
        out.push(o);
    }
    out.push(json!({ "key": "installDir", "label": "Install folder", "kind": "path", "required": false, "secret": false, "default": st.versions_dir,
        "generated": false, "askOnInstall": true, "settable": !installed, "help": "A new or empty folder; uninstalling removes it." }));
    for (key, label, offer) in [("startNow", "Start right after installing", recipe.start_after_install), ("autostart", "Start at login", recipe.autostart)] {
        if let Some(o) = offer {
            out.push(json!({ "key": key, "label": label, "kind": "bool", "required": false, "secret": false, "default": o.default,
                "generated": false, "askOnInstall": o.ask_on_install, "settable": true }));
        }
    }
    json!({ "tool": recipe.name, "revision": recipe.revision, "installed": installed, "options": out,
        "otherInstance": tools::other_instance(recipe, &Default::default()) })
}

/// A required secret (no `generate`) that neither the ask nor the tool's
/// state supplies.
fn missing_secrets(recipe: &Recipe, values: &Map<String, Value>) -> Vec<String> {
    let st = paths::tool_paths(&recipe.name).map(|p| tools::state::load(&p.data)).unwrap_or_default();
    recipe
        .secrets
        .iter()
        .filter(|s| s.required() && !st.secrets.contains_key(&s.key))
        .map(|s| format!("{}{}", recipe::SECRET_DECISION, s.key))
        .filter(|k| !values.get(k).is_some_and(|v| v.as_str().is_some_and(|t| !t.trim().is_empty())))
        .collect()
}

/// The recipe catalog on GitHub, where recipes are submitted.
pub const CATALOG_REPO: &str = "outcast1000/roadie-recipes";
/// Beyond this a prefilled "new file" link is not attempted and the file
/// goes on the clipboard instead. GitHub documents no limit for `value`;
/// 8 KB keeps well inside what browsers and GitHub accept for a URL.
pub const PREFILL_LIMIT: usize = 8 * 1024;

fn url_encode(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// What a person needs to propose `name` to the recipe catalog: the file,
/// where it goes, and a GitHub link that opens it under their own account.
/// Roadie submits nothing and holds no GitHub credential; the user (or an
/// assistant with its own GitHub access) opens the pull request. Only a
/// trusted, valid recipe qualifies, and a change to a catalog recipe must
/// raise its revision past the catalog's.
pub fn submission(name: &str) -> Result<Value, Refusal> {
    let stored = store::get(name).ok_or_else(|| Refusal::new(Refused::NotFound, format!("unknown recipe: {name}")))?;
    match stored.origin {
        store::Origin::Draft => return Err(Refusal::new(Refused::Conflict, format!("{name} is a draft: the user must review and Trust it in Roadie before it can be submitted"))),
        store::Origin::Catalog => return Err(Refusal::new(Refused::Conflict, format!("{name} is the catalog's own recipe, unchanged; there is nothing to submit"))),
        _ => {}
    }
    let recipe = stored.recipe;
    let errors = recipe::validate(&recipe);
    if !errors.is_empty() {
        return Err(Refusal::new(Refused::Invalid, format!("{name} does not validate; fix it before submitting")).with(json!({ "errors": errors })));
    }
    let listed = store::catalog_entry(name);
    if let Some(l) = &listed {
        if store::same(&l.recipe, &recipe) {
            return Err(Refusal::new(Refused::Conflict, format!("{name} is identical to the catalog's revision {}; there is nothing to submit", l.recipe.revision)));
        }
        if recipe.revision <= l.recipe.revision {
            return Err(Refusal::new(Refused::Invalid, format!("/revision is {} but the catalog has {}; raise it above {} so Roadie offers the change", recipe.revision, l.recipe.revision, l.recipe.revision)));
        }
    }
    let path = format!("recipes/{name}.json");
    let file = serde_json::to_string_pretty(&recipe).map_err(|e| Refusal::new(Refused::Invalid, e.to_string()))? + "\n";
    let is_update = listed.is_some();
    let (submit_url, clipboard) = if is_update {
        // GitHub cannot prefill an edit: the file goes on the clipboard.
        (format!("https://github.com/{CATALOG_REPO}/edit/main/{path}"), true)
    } else {
        let base = format!("https://github.com/{CATALOG_REPO}/new/main?filename={}", url_encode(&path));
        let full = format!("{base}&value={}", url_encode(&file));
        if full.len() <= PREFILL_LIMIT { (full, false) } else { (base, true) }
    };
    let how = if clipboard {
        format!("Open submitUrl, paste the file {}, and propose the change: GitHub forks the catalog under your account and opens a pull request.", if is_update { "over the current one" } else { "as its contents" })
    } else {
        "Open submitUrl: GitHub shows the new file filled in; propose it, and GitHub forks the catalog under your account and opens a pull request.".to_string()
    };
    Ok(json!({
        "name": name,
        "repo": CATALOG_REPO,
        "path": path,
        "recipe": recipe,
        "file": file,
        "isUpdate": is_update,
        "revision": recipe.revision,
        "catalogRevision": listed.map(|l| l.recipe.revision),
        "submitUrl": submit_url,
        "clipboardFallback": clipboard,
        "instructions": how,
    }))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnError {
    NotInstalled,
    /// A known consumer the user has not approved for this tool yet.
    ConsentRequired,
    UnknownConsumer,
    Other(String),
}

/// Connection details for an approved consumer: the URL and *its* key.
pub fn consumer_connection(recipe: &Recipe, consumer: &str) -> Result<Value, ConnError> {
    let policy = recipe.connection.as_ref().map(|c| c.policy).unwrap_or(recipe::ConnectionPolicy::None);
    if policy == recipe::ConnectionPolicy::None {
        return Err(ConnError::Other(format!("{} exposes no connection", recipe.display_name)));
    }
    if !consent::is_valid_id(consumer) {
        return Err(ConnError::Other("bad consumer id".into()));
    }
    let st = tools::status(recipe);
    let url = st.url.clone().ok_or_else(|| ConnError::Other("tool has no connection URL".into()))?;
    match consent::grant(consumer, &recipe.name) {
        Some(_) if !st.installed => Err(ConnError::NotInstalled),
        Some(g) => {
            // A shared key: every approved app gets the tool's one key.
            let key = match policy {
                recipe::ConnectionPolicy::SharedKey => shared_key(recipe)?,
                _ => g.key,
            };
            let mut out = json!({ "url": url, "apiKey": if key.is_empty() { Value::Null } else { Value::String(key) }, "policy": policy, "running": st.running, "healthy": st.healthy });
            with_web_login(recipe, &mut out);
            Ok(out)
        }
        None if consent::get(consumer).is_some() => Err(ConnError::ConsentRequired),
        None => Err(ConnError::UnknownConsumer),
    }
}

/// Connection details with the tool's own internal key (the API's bearer
/// holder: the owner's scripts).
pub fn owner_connection(recipe: &Recipe) -> Result<Value, ConnError> {
    let policy = recipe.connection.as_ref().map(|c| c.policy).unwrap_or(recipe::ConnectionPolicy::None);
    if policy == recipe::ConnectionPolicy::None {
        return Err(ConnError::Other(format!("{} exposes no connection", recipe.display_name)));
    }
    let st = tools::status(recipe);
    let url = st.url.clone().ok_or_else(|| ConnError::Other("tool has no connection URL".into()))?;
    if !st.installed {
        return Err(ConnError::NotInstalled);
    }
    let key = match recipe.connection.as_ref().and_then(|c| c.key.clone()) {
        Some(_) => Some(shared_key(recipe)?),
        None => {
            let p = paths::tool_paths(&recipe.name).map_err(ConnError::Other)?;
            tools::state::load(&p.data).secrets.get("internalKey").cloned()
        }
    };
    let mut out = json!({ "url": url, "apiKey": key, "policy": policy, "running": st.running, "healthy": st.healthy });
    with_web_login(recipe, &mut out);
    Ok(out)
}

/// The secret a `sharedKey` connection hands out (`connection.key`).
fn shared_key(recipe: &Recipe) -> Result<String, ConnError> {
    let name = recipe.connection.as_ref().and_then(|c| c.key.clone()).ok_or_else(|| ConnError::Other(format!("{} names no shared key", recipe.display_name)))?;
    let p = paths::tool_paths(&recipe.name).map_err(ConnError::Other)?;
    tools::state::load(&p.data).secrets.get(&name).cloned().ok_or_else(|| ConnError::Other(format!("{} has no {name} yet", recipe.display_name)))
}

/// The secrets a recipe lets the user choose (`askOnInstall`), for the
/// owner's own screen ("Show key"): the user may see and copy them.
pub fn owner_secrets(name: &str) -> Result<Value, Refusal> {
    let recipe = trusted(name)?;
    let p = paths::tool_paths(name).map_err(|e| Refusal::new(Refused::Conflict, e))?;
    let st = tools::state::load(&p.data);
    let out: Map<String, Value> = recipe
        .secrets
        .iter()
        .filter(|s| s.ask_on_install)
        .filter_map(|s| st.secrets.get(&s.key).map(|v| (s.key.clone(), json!({ "label": s.label, "value": v }))))
        .collect();
    if out.is_empty() {
        return Err(Refusal::new(Refused::NotFound, format!("{} has no secret to show yet", recipe.display_name)));
    }
    Ok(Value::Object(out))
}

/// Add `webLogin` to a connection answer when the recipe declares one. The
/// same holders already get an API key that can do what the web page does,
/// so the login grants nothing more — it lets a person open the page. A
/// login that fails to expand is logged and left out: the connection is
/// still good without it.
fn with_web_login(recipe: &Recipe, out: &mut Value) {
    match tools::web_login(recipe) {
        Ok(Some(login)) => {
            if let Some(o) = out.as_object_mut() {
                o.insert("webLogin".into(), login);
            }
        }
        Ok(None) => {}
        Err(e) => log::error!("{}: web login did not expand: {e}", recipe.name),
    }
}

/// The web login for the owner's own screen (the window's "Show login").
pub fn owner_web_login(name: &str) -> Result<Value, Refusal> {
    let recipe = trusted(name)?;
    if !tools::status(&recipe).installed {
        return Err(Refusal::new(Refused::Conflict, format!("{} is not installed", recipe.display_name)));
    }
    match tools::web_login(&recipe) {
        Ok(Some(v)) => Ok(v),
        Ok(None) => Err(Refusal::new(Refused::NotFound, format!("{} declares no web login", recipe.display_name))),
        Err(e) => Err(Refusal::new(Refused::Conflict, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recipe::{catalog, fixtures};

    fn named(base: &str, name: &str, revision: u32) -> Recipe {
        let mut r = fixtures::recipe(base);
        r.name = name.into();
        r.revision = revision;
        r
    }

    /// Make `recipe` look installed at `version` without downloading it.
    fn fake_install(recipe: &Recipe, version: &str) {
        let p = paths::tool_paths(&recipe.name).unwrap();
        let bin = tools::install::binary_path(recipe, &p, version);
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
        tools::install::set_current(&p, version).unwrap();
    }

    #[test]
    fn a_catalog_recipe_is_installed_as_a_brought_one() {
        let _s = catalog::tests::serial();
        let r = named("yt-dlp", "intake-cat", 1);
        catalog::tests::publish(std::slice::from_ref(&r));

        let plan = install("intake-cat", InstallAsk::default()).unwrap();
        assert_eq!(plan.recipe_change, Some(Change::New), "the catalog's recipe rides in the request for review");
        let RequestKind::Install { recipe: Some(p), recipe_source, .. } = &plan.kind else { panic!("{:?}", plan.kind) };
        assert_eq!((p.name.as_str(), *recipe_source), ("intake-cat", Some(RecipeSource::Catalog)));

        // An app's own recipe wins over the catalog's; the catalog's own
        // file, brought by an app, still counts as the catalog's.
        let mut theirs = r.clone();
        theirs.summary = "an app's own".into();
        let plan = install("intake-cat", InstallAsk { recipe: Some(serde_json::to_value(&theirs).unwrap()), ..Default::default() }).unwrap();
        let RequestKind::Install { recipe: Some(p), recipe_source, .. } = &plan.kind else { panic!() };
        assert_eq!((p.summary.as_str(), *recipe_source), ("an app's own", Some(RecipeSource::User)));
        let plan = install("intake-cat", InstallAsk { recipe: Some(serde_json::to_value(&r).unwrap()), ..Default::default() }).unwrap();
        assert!(matches!(plan.kind, RequestKind::Install { recipe_source: Some(RecipeSource::Catalog), .. }));

        let st = public_status(tools::status(&r), &store::get("intake-cat").unwrap());
        assert_eq!((st["installed"].as_bool(), st["available"].as_bool(), st["origin"].as_str(), st["trusted"].as_bool()), (Some(false), Some(true), Some("catalog"), Some(false)), "{st}");

        // Approved: trusted from the catalog; the same recipe needs no review.
        store::put_trusted(r.clone(), RecipeSource::Catalog).unwrap();
        let plan = install("intake-cat", InstallAsk::default()).unwrap();
        assert_eq!(plan.recipe_change, None, "identical to the trusted one: nothing to review");
        let _ = store::delete("intake-cat");
    }

    #[test]
    fn a_catalog_revision_is_a_recipe_update_that_asks() {
        let _s = catalog::tests::serial();
        let v1 = named("yt-dlp", "intake-upd", 1);
        catalog::tests::publish(std::slice::from_ref(&v1));
        store::put_trusted(v1.clone(), RecipeSource::Catalog).unwrap();
        fake_install(&v1, "2026.01.01");
        assert!(store::recipe_update("intake-upd").is_none());

        let mut v2 = v1.clone();
        v2.revision = 2;
        v2.notes = Some("new notes".into());
        catalog::tests::publish(&[v2.clone()]);
        let u = store::recipe_update("intake-upd").expect("a higher revision is offered");
        assert_eq!((u.revision, u.changed_keys.clone()), (2, vec!["revision".to_string(), "notes".to_string()]));
        let st = public_status(tools::status(&v1), &store::get("intake-upd").unwrap());
        assert_eq!(st["recipeUpdate"]["revision"], 2, "{st}");
        assert_eq!(store::get_trusted("intake-upd").unwrap().revision, 1, "never applied silently");

        match update("intake-upd", None).unwrap() {
            UpdatePlan::Review { kind: RequestKind::ReplaceRecipe { recipe, recipe_change, recipe_source, .. }, .. } => {
                assert_eq!((recipe.revision, recipe_change, recipe_source), (2, Change::ChangesTrusted, Some(RecipeSource::Catalog)));
            }
            _ => panic!("an upgrade reviews the recipe update"),
        }
        let plan = install("intake-upd", InstallAsk::default()).unwrap();
        assert_eq!(plan.recipe_change, Some(Change::ChangesTrusted), "a reinstall reviews it too");

        // A user recipe of their own gets no catalog updates.
        store::put_trusted(v1.clone(), RecipeSource::User).unwrap();
        assert!(store::recipe_update("intake-upd").is_none());

        // Gone from the catalog: delisted, still trusted.
        store::put_trusted(v1.clone(), RecipeSource::Catalog).unwrap();
        catalog::tests::publish(&[]);
        assert!(store::delisted("intake-upd"));
        let st = public_status(tools::status(&v1), &store::get("intake-upd").unwrap());
        assert_eq!((st["delisted"].as_bool(), st["trusted"].as_bool(), st["recipeUpdate"].is_null()), (Some(true), Some(true), true), "{st}");
        let _ = tools::uninstall(&v1, false);
        let _ = store::delete("intake-upd");
    }

    #[test]
    fn a_submission_needs_a_reviewed_recipe_and_a_higher_revision() {
        let _s = catalog::tests::serial();
        let r = named("yt-dlp", "intake-sub", 3);
        catalog::tests::publish(std::slice::from_ref(&r));
        assert_eq!(submission("intake-sub").unwrap_err().kind, Refused::Conflict, "the catalog's own recipe");
        store::put_trusted(r.clone(), RecipeSource::Catalog).unwrap();
        assert!(submission("intake-sub").unwrap_err().message.contains("identical"));

        let mut mine = r.clone();
        mine.notes = Some("better".into());
        store::put_trusted(mine.clone(), RecipeSource::User).unwrap();
        let e = submission("intake-sub").unwrap_err();
        assert!(e.kind == Refused::Invalid && e.message.contains("/revision") && e.message.contains("above 3"), "{e:?}");
        mine.revision = 4;
        store::put_trusted(mine.clone(), RecipeSource::User).unwrap();
        let v = submission("intake-sub").unwrap();
        assert_eq!((v["isUpdate"].as_bool(), v["path"].as_str(), v["clipboardFallback"].as_bool()), (Some(true), Some("recipes/intake-sub.json"), Some(true)));
        assert_eq!(v["submitUrl"], "https://github.com/outcast1000/roadie-recipes/edit/main/recipes/intake-sub.json");

        // A new tool: a prefilled link when it fits, the clipboard when not.
        catalog::tests::publish(&[]);
        let v = submission("intake-sub").unwrap();
        let url = v["submitUrl"].as_str().unwrap();
        assert_eq!(v["isUpdate"], false);
        assert!(url.starts_with("https://github.com/outcast1000/roadie-recipes/new/main?filename=recipes%2Fintake-sub.json&value=%7B"), "{url}");
        assert!(url.len() <= PREFILL_LIMIT && v["clipboardFallback"] == false);
        let file: Value = serde_json::from_str(v["file"].as_str().unwrap()).unwrap();
        assert_eq!(file["name"], "intake-sub", "the file is the recipe");
        let mut big = mine.clone();
        big.notes = Some("x".repeat(PREFILL_LIMIT));
        store::put_trusted(big, RecipeSource::User).unwrap();
        let v = submission("intake-sub").unwrap();
        assert_eq!((v["clipboardFallback"].as_bool(), v["submitUrl"].as_str().unwrap().contains("value=")), (Some(true), false));

        let mut draft = mine;
        draft.name = "intake-sub-draft".into();
        store::put_draft(draft, None).unwrap();
        assert!(submission("intake-sub-draft").unwrap_err().message.contains("draft"));
        let _ = store::delete("intake-sub-draft");
        let _ = store::delete("intake-sub");
    }

    #[test]
    fn a_catalog_only_status_answers_with_its_config() {
        let _s = catalog::tests::serial();
        let r = named("slskd", "intake-daemon", 5);
        catalog::tests::publish(std::slice::from_ref(&r));
        let s = store::get("intake-daemon").unwrap();
        let st = public_status(tools::status(&s.recipe), &s);
        assert!(st["config"].get("shares.directories").is_some(), "apps read configuration entries from status before installing: {st}");
        assert_eq!(st["available"], true);
    }

    #[test]
    fn every_approved_app_gets_the_one_shared_key() {
        let _s = catalog::tests::serial();
        crate::recipe::store::test_root();
        let mut r = fixtures::recipe("slskd@6");
        r.name = "intake-shared".into();
        store::put_trusted(r.clone(), RecipeSource::User).unwrap();
        let p = paths::tool_paths(&r.name).unwrap();
        let o = tools::InstallOptions { secrets: [("internalKey".to_string(), "the-one-key-for-all-apps".to_string())].into(), ..Default::default() };
        tools::apply_install_choices(&r, &o).unwrap();
        fake_install(&r, "1.0.0");
        for app in ["shared-app-a", "shared-app-b"] {
            let _ = consent::register(app, app, None);
            assert_eq!(consumer_connection(&r, app), Err(ConnError::ConsentRequired), "each app is approved once");
            consent::approve(app, &r).unwrap();
            let c = consumer_connection(&r, app).unwrap();
            assert_eq!(c["apiKey"], "the-one-key-for-all-apps", "{c}");
        }
        assert_eq!(owner_secrets("intake-shared").unwrap()["internalKey"]["value"], "the-one-key-for-all-apps", "the user can see it");

        // An app's chosen key rides in the request's private half only.
        let mut v = Map::new();
        v.insert("secrets.internalKey".into(), json!("x".repeat(20)));
        let kind = requests::install_kind(&r, v, None, None, RecipeSource::User);
        let shown = serde_json::to_string(&kind).unwrap();
        assert!(!shown.contains(&"x".repeat(20)) && shown.contains("secrets.internalKey"), "{shown}");
        let _ = tools::uninstall(&r, false);
        let _ = std::fs::remove_dir_all(p.root);
        let _ = store::delete("intake-shared");
    }

    #[test]
    fn options_list_every_value_an_install_takes_with_defaults_for_this_computer() {
        crate::recipe::store::test_root();
        let mut r = fixtures::recipe("slskd@6");
        r.name = "intake-options".into();
        let v = options(&r);
        let opts = v["options"].as_array().unwrap();
        let by = |k: &str| opts.iter().find(|o| o["key"] == k).unwrap_or_else(|| panic!("no option {k}: {v}")).clone();
        let keys: Vec<&str> = opts.iter().map(|o| o["key"].as_str().unwrap()).collect();
        assert_eq!(
            keys,
            vec![
                "soulseekUsername", "soulseekPassword", "downloadsDir", "incompleteDir", "shareDownloads", "httpsEnabled", "shares.directories",
                "ports.https", "ports.listen", "ports.web", "secrets.internalKey", "installDir", "startNow", "autostart"
            ]
        );
        let dl = by("downloadsDir");
        assert!(dl["default"].as_str().is_some_and(|d| !d.contains('{') && d.ends_with("Soulseek")), "defaults are expanded: {dl}");
        assert_eq!((by("ports.web")["default"].as_u64(), by("ports.web")["required"].as_bool()), (Some(5030), Some(false)));
        let key = by("secrets.internalKey");
        assert_eq!((key["generated"].as_bool(), key["required"].as_bool(), key["default"].is_null()), (Some(true), Some(false), true), "a key Roadie makes, never shown: {key}");
        assert!(by("installDir")["default"].as_str().is_some_and(|d| d.ends_with("versions")));
        assert_eq!(by("startNow")["default"], true);
        assert_eq!((by("soulseekUsername")["required"].as_bool(), by("soulseekPassword")["required"].as_bool()), (Some(true), Some(true)), "slskd needs an account");
        assert_eq!((by("httpsEnabled")["default"].as_bool(), by("ports.https")["default"].as_u64()), (Some(false), Some(5031)), "HTTPS off by default");
        assert!(by("incompleteDir")["default"].is_null() && by("incompleteDir")["help"].as_str().unwrap().contains(".incomplete"), "blank follows the downloads folder");
        assert!(!v.to_string().contains("hex48"), "nothing about secrets leaks beyond their shape");

        // A secret the recipe cannot generate is required.
        r.secrets[0].generate = None;
        let v = options(&r);
        let key = v["options"].as_array().unwrap().iter().find(|o| o["key"] == "secrets.internalKey").unwrap().clone();
        assert_eq!((key["required"].as_bool(), key["generated"].as_bool()), (Some(true), Some(false)));
        assert_eq!(missing_secrets(&r, &Map::new()), vec!["secrets.internalKey"]);
        let mut given = Map::new();
        given.insert("secrets.internalKey".into(), json!("k".repeat(20)));
        assert!(missing_secrets(&r, &given).is_empty());
        let e = tools::require_secrets(&r).unwrap_err();
        assert!(e.contains("cannot generate"), "{e}");
    }
}
