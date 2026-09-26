//! What a client's ask becomes, shared by both releases: the desktop
//! service's API handlers and the standalone CLI call these, so the two
//! refuse the same things for the same reasons and build the same requests.
//! An install, upgrade or uninstall ask is checked here and turned into a
//! `RequestKind` for the user to approve. The API maps a `Refusal` to an
//! HTTP status; the CLI maps it to a message and an exit code.

use crate::recipe::store::{self, Change};
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
/// means it is exactly the trusted recipe, so nothing needs reviewing.
pub fn brought_recipe(name: &str, v: Value) -> Result<(Recipe, Option<Change>), Refusal> {
    let recipe = recipe::from_value(v).map_err(|errors| {
        Refusal::new(Refused::Invalid, "the recipe you sent is invalid; each error names a JSON pointer inside /recipe").with(json!({ "errors": errors }))
    })?;
    if recipe.name != name {
        return Err(Refusal::new(Refused::BadRequest, format!("the ask names {name} but /recipe/name is {}; use the same name", recipe.name)));
    }
    let change = store::compare(&recipe);
    Ok((recipe, change))
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
    let (recipe, proposed) = match brought {
        Some(v) => brought_recipe(name, v)?,
        None => (trusted(name)?, None),
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
    for (key, label, offer) in [("startNow", "Start now, right after installing", recipe.start_after_install), ("autostart", "Start at login", recipe.autostart)] {
        if let Some(o) = offer {
            decisions.push(json!({
                "key": key, "label": label, "kind": "bool", "required": false, "default": o.default,
                "settled": true, "value": values.get(key).and_then(|v| v.as_bool()).unwrap_or(o.default),
            }));
        }
    }
    Ok(InstallPlan { kind: requests::install_kind(&recipe, values, consumer, proposed), decisions, recipe_change: proposed })
}

pub enum UpdatePlan {
    /// The caller brought a different recipe: the user reviews and trusts
    /// it, and approving updates the tool.
    Review { kind: RequestKind, change: Change },
    /// Update now with the trusted recipe (`update_now`).
    Now(Box<Recipe>),
}

/// An upgrade ask, optionally with the caller's recipe.
pub fn update(name: &str, brought: Option<Value>) -> Result<UpdatePlan, Refusal> {
    if let Some(v) = brought {
        let (recipe, change) = brought_recipe(name, v)?;
        if let Some(change) = change {
            let installed = store::get_trusted(name).map(|current| tools::status(&current).installed).unwrap_or(false);
            if !installed {
                return Err(Refusal::new(Refused::Conflict, format!("{} is not installed; install it with this recipe instead", recipe.display_name)));
            }
            return Ok(UpdatePlan::Review { kind: RequestKind::ReplaceRecipe { tool: name.to_string(), recipe: Box::new(recipe), recipe_change: change }, change });
        }
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
/// from and whether it is trusted.
pub fn public_status(s: tools::ToolStatus, stored: &store::Stored) -> Value {
    let mut v = serde_json::to_value(s).unwrap_or_default();
    if let Some(o) = v.as_object_mut() {
        o.insert("origin".into(), serde_json::to_value(stored.origin).unwrap_or_default());
        o.insert("trusted".into(), Value::Bool(stored.trusted()));
        o.remove("pid");
    }
    v
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
            let mut out = json!({ "url": url, "apiKey": if g.key.is_empty() { Value::Null } else { Value::String(g.key) }, "policy": policy, "running": st.running, "healthy": st.healthy });
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
    let p = paths::tool_paths(&recipe.name).map_err(ConnError::Other)?;
    let key = tools::state::load(&p.data).secrets.get("internalKey").cloned();
    let mut out = json!({ "url": url, "apiKey": key, "policy": policy, "running": st.running, "healthy": st.healthy });
    with_web_login(recipe, &mut out);
    Ok(out)
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
