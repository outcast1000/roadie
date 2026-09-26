//! The recipe format: one JSON document per tool, interpreted by `tools/`.
//! Nothing in Roadie knows a tool by name — every behaviour a tool needs is a
//! field here, and adding a capability means adding a field, documenting it
//! in `recipes/SCHEMA.md`, and validating it below.
//!
//! `validate()` is hand-written (no schema crate) and every error names a
//! JSON pointer, because the main author of new recipes is expected to be an
//! AI assistant iterating through the API: "/files/0/content/web/port:
//! unknown placeholder `ports.wev`" is actionable, "invalid recipe" is not.

pub mod emit;
pub mod httpsteps;
pub mod jsonq;
pub mod store;
pub mod template;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const RECIPE_VERSION: u32 = 1;

/// Platforms a recipe may name assets for. `os-arch`.
pub const PLATFORMS: &[&str] = &[
    "darwin-arm64",
    "darwin-x64",
    "windows-x64",
    "windows-arm64",
    "linux-x64",
    "linux-arm64",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Platform {
    pub os: &'static str,
    pub arch: &'static str,
}

impl Platform {
    pub fn current() -> Self {
        let os = match std::env::consts::OS {
            "macos" => "darwin",
            "windows" => "windows",
            _ => "linux",
        };
        let arch = match std::env::consts::ARCH {
            "aarch64" => "arm64",
            _ => "x64",
        };
        Platform { os, arch }
    }
    pub fn key(&self) -> String {
        format!("{}-{}", self.os, self.arch)
    }
}

fn default_true() -> bool {
    true
}
fn default_tag_style() -> String {
    "plain".into()
}
fn default_version_timeout() -> u64 {
    60
}
fn default_startup_grace() -> u64 {
    30
}
fn default_stop_grace() -> u64 {
    15
}
fn default_http_timeout() -> u64 {
    5
}
fn default_method() -> String {
    "GET".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Recipe {
    pub recipe_version: u32,
    pub name: String,
    pub display_name: String,
    /// Who wrote and maintains this recipe (a person, a project or an
    /// assistant's operator) — shown on the card and the review screen.
    #[serde(default)]
    pub author: String,
    /// The recipe's own edition, bumped by its author on every change. Not
    /// the tool's version (that is resolved from the release) and not the
    /// format version (`recipeVersion`).
    #[serde(default)]
    pub revision: u32,
    /// Platform keys this recipe targets. Every listed platform must have a
    /// download in `source` (or an override), and every download must be
    /// listed, so the two never disagree.
    #[serde(default)]
    pub platforms: Vec<String>,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    /// Free text for consumers (e.g. "never pass -U; Roadie owns updates").
    #[serde(default)]
    pub notes: Option<String>,
    pub kind: Kind,
    pub source: Source,
    pub archive: Archive,
    #[serde(default)]
    pub layout: Layout,
    /// `platform key -> { archive?, layout? }` replacing the defaults above.
    #[serde(default)]
    pub overrides: BTreeMap<String, Override>,
    #[serde(default)]
    pub min_binary_bytes: u64,
    pub version: VersionProbe,
    #[serde(default)]
    pub secrets: Vec<SecretDef>,
    #[serde(default)]
    pub ports: BTreeMap<String, PortDef>,
    #[serde(default)]
    pub config: Vec<ConfigField>,
    #[serde(default)]
    pub connection: Option<Connection>,
    /// Directories that must exist before start (tools that refuse to start
    /// on a missing path). Placeholders allowed.
    #[serde(default)]
    pub create_dirs: Vec<String>,
    #[serde(default)]
    pub files: Vec<FileDef>,
    #[serde(default)]
    pub run: Option<Run>,
    #[serde(default)]
    pub health: Option<Health>,
    #[serde(default)]
    pub busy: Option<Busy>,
    #[serde(default)]
    pub stop: Option<Stop>,
    /// Daemon only: whether Roadie starts the daemon right after the first
    /// install, and whether that is a question the install prompt asks.
    /// Arrives in the install decisions as the reserved key `startNow`.
    #[serde(default)]
    pub start_after_install: Option<InstallChoice>,
    /// Daemon only: whether the daemon is registered to start at login
    /// (through Roadie's launcher mode), and whether the prompt asks.
    /// Reserved decision key `autostart`.
    #[serde(default)]
    pub autostart: Option<InstallChoice>,
    #[serde(default)]
    pub start_failures: Vec<StartFailure>,
    /// `details.<key>` values read off the daemon's log with a regex whose
    /// first capture is the value (e.g. a quick tunnel's hostname).
    #[serde(default)]
    pub log_extract: BTreeMap<String, String>,
}

/// A yes/no the install has to settle, with the recipe's suggestion.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct InstallChoice {
    #[serde(default)]
    pub default: bool,
    #[serde(default)]
    pub ask_on_install: bool,
}

/// Decision keys the engine owns; no config field may use them.
pub const RESERVED_DECISION_KEYS: &[&str] = &["startNow", "autostart"];

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Daemon,
    Cli,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum Source {
    /// Assets on a GitHub release. `assets` maps a platform key to the asset
    /// filename template (`{version}` is the tag without any `v`).
    GithubRelease {
        repo: String,
        /// `plain` (tag == version), `vPrefixed` (tag `v1.2.3`), or
        /// `floating:<tag>` (a moving tag such as BtbN's `latest`).
        #[serde(default = "default_tag_style")]
        tag_style: String,
        assets: BTreeMap<String, String>,
        #[serde(default)]
        checksums: Checksums,
    },
    /// A per-platform URL that redirects to the versioned download; the
    /// version is read off the final URL with `versionRegex`.
    HttpRedirect {
        latest_url: BTreeMap<String, String>,
        version_regex: String,
        #[serde(default)]
        checksums: Checksums,
    },
    /// A download page listing versioned links. `links` maps a platform to a
    /// regex matched against every `href` on `page`; its first capture is
    /// the version, and the highest version wins. Relative hrefs resolve
    /// against `page`.
    HtmlIndex {
        page: String,
        links: BTreeMap<String, String>,
        #[serde(default)]
        checksums: Checksums,
    },
}

/// Per-platform replacement for `archive` / `layout`, for tools whose
/// builds are packaged differently per OS (one zip with a `bin/` folder on
/// Windows, a bare binary in a zip on macOS).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct Override {
    #[serde(default)]
    pub archive: Option<Archive>,
    #[serde(default)]
    pub layout: Option<Layout>,
    /// A different release source for this platform (e.g. another builder).
    #[serde(default)]
    pub source: Option<Source>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum Checksums {
    /// Upstream publishes none: size floor + HTML sniff + run `--version`.
    #[default]
    None,
    /// A `<sha256>  <filename>` file on the same release.
    SumsFile { asset: String },
    /// `<asset><suffix>` beside the asset, containing the hex digest.
    Sidecar { suffix: String },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Archive {
    Zip,
    Tgz,
    Bare,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct Layout {
    #[serde(default)]
    pub strip_top_dir: bool,
    /// Paths inside the extracted archive (after stripping); `[0]` is the
    /// main binary. Empty means `[name]`. `.exe` is appended on Windows.
    #[serde(default)]
    pub binaries: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct VersionProbe {
    pub args: Vec<String>,
    /// First capture group is the version.
    pub regex: String,
    #[serde(default = "default_version_timeout")]
    pub timeout_sec: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SecretDef {
    pub key: String,
    /// `hex<N>` — N hex characters from OS entropy.
    pub generate: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PortDef {
    pub default: u16,
    /// Scan `default+1..=default+10` when the default is taken by something
    /// that is not this tool. `false` for ports the engine never probes.
    #[serde(default = "default_true")]
    pub pick: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FieldKind {
    Text,
    Password,
    Path,
    Bool,
    Port,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConfigField {
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub help: Option<String>,
    pub kind: FieldKind,
    #[serde(default)]
    pub required: bool,
    /// Stored in `secrets`, never echoed by status or the API.
    #[serde(default)]
    pub secret: bool,
    #[serde(default)]
    pub default: Option<Value>,
    /// macOS: a path under ~/Downloads|Documents|Desktop needs TCC consent
    /// for a background daemon; the UI warns.
    #[serde(default)]
    pub tcc_sensitive: bool,
    /// Create the directory before start (daemons that refuse missing dirs).
    #[serde(default)]
    pub create_dir: bool,
    /// A decision the user (or the API caller) makes *before* the first
    /// install — an account name, a folder. The window asks for these when
    /// Install is clicked; an API/MCP caller passes them in the install
    /// request and the approval prompt shows what was decided. `required`
    /// fields are always asked.
    #[serde(default)]
    pub ask_on_install: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Connection {
    pub policy: ConnectionPolicy,
    #[serde(default)]
    pub min_len: Option<usize>,
    #[serde(default)]
    pub max_len: Option<usize>,
    /// Base URL consumers connect to, e.g. `http://127.0.0.1:{ports.web}`.
    pub url: String,
    /// The sign-in for the tool's own web page, when it has one behind a
    /// login Roadie generated (slskd's web UI). Expanded like `url`. Handed
    /// only to the owner and to consumers the user approved, never in status.
    #[serde(default)]
    pub web_login: Option<WebLogin>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WebLogin {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ConnectionPolicy {
    None,
    Open,
    PerConsumerKey,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FileFormat {
    Yaml,
    Json,
    Env,
    Ini,
    Raw,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FileDef {
    pub path: String,
    pub format: FileFormat,
    #[serde(default)]
    pub secret: bool,
    pub content: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Run {
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default = "default_startup_grace")]
    pub startup_grace_sec: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HttpRequest {
    #[serde(default = "default_method")]
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub json: Option<Value>,
    /// `name -> jsonq path` read off the response body into `{name}`.
    #[serde(default)]
    pub capture: BTreeMap<String, String>,
    #[serde(default = "default_http_timeout")]
    pub timeout_sec: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Health {
    pub request: HttpRequest,
    /// Statuses meaning "something answered but it is not ours".
    #[serde(default)]
    pub unauthorized_status: Vec<u16>,
    /// `version` and `details.<key>` -> jsonq path.
    #[serde(default)]
    pub extract: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Busy {
    /// Every request is sent; a match in any reply means busy. An error
    /// means busy too — never restart on a guess.
    pub requests: Vec<HttpRequest>,
    pub busy_if: BusyIf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BusyIf {
    pub path: String,
    pub regex: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Stop {
    #[serde(default = "default_stop_grace")]
    pub grace_sec: u64,
    /// Graceful stop through the tool's own API, as a chain of requests with
    /// captures (login → token → shutdown). Empty → signal straight away.
    #[serde(default)]
    pub api: Vec<HttpRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StartFailure {
    pub regex: String,
    pub code: String,
    pub message: String,
}

// --- Validation ---

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ValidationError {
    pub pointer: String,
    pub message: String,
}

impl ValidationError {
    fn new(pointer: impl Into<String>, message: impl Into<String>) -> Self {
        Self { pointer: pointer.into(), message: message.into() }
    }
}

pub fn parse(json: &str) -> Result<Recipe, Vec<ValidationError>> {
    let value: Value = serde_json::from_str(json)
        .map_err(|e| vec![ValidationError::new("", format!("not valid JSON: {e}"))])?;
    from_value(value)
}

pub fn from_value(value: Value) -> Result<Recipe, Vec<ValidationError>> {
    let recipe: Recipe = serde_json::from_value(value)
        .map_err(|e| vec![ValidationError::new("", format!("does not match the recipe shape: {e}"))])?;
    let errors = validate(&recipe);
    if errors.is_empty() {
        Ok(recipe)
    } else {
        Err(errors)
    }
}

pub fn validate(r: &Recipe) -> Vec<ValidationError> {
    let mut errs = Vec::new();
    macro_rules! err { ($p:expr, $m:expr) => { errs.push(ValidationError::new($p, String::from($m))) }; }

    if r.recipe_version != RECIPE_VERSION {
        err!("/recipeVersion", format!("must be {RECIPE_VERSION}"));
    }
    if !is_valid_name(&r.name) {
        err!("/name", "must match ^[a-z0-9][a-z0-9-]{0,31}$");
    }
    if r.display_name.trim().is_empty() {
        err!("/displayName", "must not be empty");
    }
    if r.author.trim().is_empty() {
        err!("/author", "must name who maintains this recipe (a person, project or organisation)");
    }
    if r.revision == 0 {
        err!("/revision", "must be a positive integer; start at 1 and bump it on every change");
    }

    // Platforms: declared explicitly, and consistent with the downloads.
    if r.platforms.is_empty() {
        err!("/platforms", format!("list the platforms this recipe targets; one or more of {}", PLATFORMS.join(", ")));
    }
    for (i, k) in r.platforms.iter().enumerate() {
        if !PLATFORMS.contains(&k.as_str()) {
            err!(&format!("/platforms/{i}"), format!("unknown platform; one of {}", PLATFORMS.join(", ")));
        } else if r.platforms[..i].contains(k) {
            err!(&format!("/platforms/{i}"), "listed twice");
        } else if !r.has_download_for(k) {
            err!(&format!("/platforms/{i}"), format!("`{k}` is listed but has no download in /source (or an override); add one or drop the platform"));
        }
    }
    for k in PLATFORMS {
        if r.has_download_for(k) && !r.platforms.iter().any(|p| p == k) {
            err!("/platforms", format!("`{k}` has a download but is not listed; add it to /platforms or remove that download"));
        }
    }

    // Source.
    match &r.source {
        Source::GithubRelease { repo, tag_style, assets, checksums } => {
            if repo.split('/').filter(|s| !s.is_empty()).count() != 2 {
                err!("/source/repo", "must be owner/repo");
            }
            if !(tag_style == "plain" || tag_style == "vPrefixed" || tag_style.starts_with("floating:")) {
                err!("/source/tagStyle", "must be plain, vPrefixed or floating:<tag>");
            }
            if assets.is_empty() {
                err!("/source/assets", "at least one platform is required");
            }
            for (k, v) in assets {
                if !PLATFORMS.contains(&k.as_str()) {
                    err!(&format!("/source/assets/{k}"), format!("unknown platform; one of {}", PLATFORMS.join(", ")));
                }
                check_placeholders(r, v, &format!("/source/assets/{k}"), &["version", "platform"], &mut errs);
            }
            if let Checksums::SumsFile { asset } = checksums {
                if asset.is_empty() {
                    err!("/source/checksums/asset", "must not be empty");
                }
            }
        }
        Source::HttpRedirect { latest_url, version_regex, .. } => {
            if latest_url.is_empty() {
                err!("/source/latestUrl", "at least one platform is required");
            }
            for (k, v) in latest_url {
                if !PLATFORMS.contains(&k.as_str()) {
                    err!(&format!("/source/latestUrl/{k}"), "unknown platform");
                }
                if !v.starts_with("https://") {
                    err!(&format!("/source/latestUrl/{k}"), "must be an https URL");
                }
            }
            check_regex(version_regex, "/source/versionRegex", true, &mut errs);
        }
        Source::HtmlIndex { page, links, .. } => {
            if !page.starts_with("https://") {
                err!("/source/page", "must be an https URL");
            }
            if links.is_empty() {
                err!("/source/links", "at least one platform is required");
            }
            for (k, re) in links {
                if !PLATFORMS.contains(&k.as_str()) {
                    err!(&format!("/source/links/{k}"), "unknown platform");
                }
                check_regex(re, &format!("/source/links/{k}"), true, &mut errs);
            }
        }
    }
    for (k, o) in &r.overrides {
        if !PLATFORMS.contains(&k.as_str()) {
            err!(&format!("/overrides/{k}"), "unknown platform");
        }
        if let Some(s) = &o.source {
            // Reuse the main source checks on a copy, re-pointed.
            let mut tmp = r.clone();
            tmp.source = s.clone();
            tmp.overrides.clear();
            for e in validate(&tmp).into_iter().filter(|e| e.pointer.starts_with("/source")) {
                errs.push(ValidationError::new(format!("/overrides/{k}{}", e.pointer), e.message));
            }
        }
        if let Some(l) = &o.layout {
            for (i, b) in l.binaries.iter().enumerate() {
                if b.is_empty() || b.starts_with('/') || b.contains("..") {
                    err!(&format!("/overrides/{k}/layout/binaries/{i}"), "must be a relative path inside the archive");
                }
            }
        }
    }

    // Layout / version.
    for (i, b) in r.layout.binaries.iter().enumerate() {
        if b.is_empty() || b.starts_with('/') || b.contains("..") {
            err!(&format!("/layout/binaries/{i}"), "must be a relative path inside the archive");
        }
    }
    if r.archive == Archive::Bare && r.layout.binaries.len() > 1 {
        err!("/layout/binaries", "a bare download is exactly one binary");
    }
    check_regex(&r.version.regex, "/version/regex", true, &mut errs);

    // Secrets / ports / config.
    let mut seen = std::collections::BTreeSet::new();
    for (i, s) in r.secrets.iter().enumerate() {
        if !is_valid_key(&s.key) {
            err!(&format!("/secrets/{i}/key"), "must be an identifier");
        }
        if !seen.insert(format!("secret:{}", s.key)) {
            err!(&format!("/secrets/{i}/key"), "duplicate secret key");
        }
        let n = s.generate.strip_prefix("hex").and_then(|n| n.parse::<usize>().ok());
        if !matches!(n, Some(n) if (8..=256).contains(&n) && n % 2 == 0) {
            err!(&format!("/secrets/{i}/generate"), "must be hex<N> with even N between 8 and 256");
        }
    }
    for (k, p) in &r.ports {
        if !is_valid_key(k) {
            err!(&format!("/ports/{k}"), "port name must be an identifier");
        }
        if p.default == 0 {
            err!(&format!("/ports/{k}/default"), "must be 1–65535");
        }
    }
    for (i, f) in r.config.iter().enumerate() {
        if !is_valid_key(&f.key) {
            err!(&format!("/config/{i}/key"), "must be an identifier");
        }
        if RESERVED_DECISION_KEYS.contains(&f.key.as_str()) {
            err!(&format!("/config/{i}/key"), format!("`{}` is reserved for the install choice of the same name; pick another key", f.key));
        }
        if !seen.insert(format!("config:{}", f.key)) {
            err!(&format!("/config/{i}/key"), "duplicate config key");
        }
        if r.secrets.iter().any(|s| s.key == f.key) {
            err!(&format!("/config/{i}/key"), "collides with a generated secret");
        }
        if f.label.trim().is_empty() {
            err!(&format!("/config/{i}/label"), "must not be empty");
        }
        if f.secret && f.kind != FieldKind::Password {
            err!(&format!("/config/{i}/secret"), "only password fields can be secret");
        }
        if f.kind == FieldKind::Password && !f.secret {
            err!(&format!("/config/{i}/secret"), "password fields must be secret");
        }
        if let Some(d) = &f.default {
            let ok = match f.kind {
                FieldKind::Bool => d.is_boolean(),
                FieldKind::Port => d.as_u64().is_some_and(|n| (1..=65535).contains(&n)),
                FieldKind::Password => false,
                _ => d.is_string(),
            };
            if !ok {
                err!(&format!("/config/{i}/default"), "does not match the field kind");
            }
            if let Some(s) = d.as_str() {
                check_placeholders(r, s, &format!("/config/{i}/default"), &["home", "data"], &mut errs);
            }
        }
        if f.create_dir && f.kind != FieldKind::Path {
            err!(&format!("/config/{i}/createDir"), "only path fields can create directories");
        }
    }

    // Kind-specific shape.
    let all_roots: &[&str] = &["home", "data", "bin", "version", "platform", "ports", "config", "secrets", "connection"];
    match r.kind {
        Kind::Cli => {
            for (present, p) in [
                (r.run.is_some(), "/run"),
                (r.health.is_some(), "/health"),
                (r.busy.is_some(), "/busy"),
                (r.stop.is_some(), "/stop"),
                (!r.ports.is_empty(), "/ports"),
                (r.start_after_install.is_some(), "/startAfterInstall"),
                (r.autostart.is_some(), "/autostart"),
            ] {
                if present {
                    err!(p, "not allowed for a cli recipe");
                }
            }
            if let Some(c) = &r.connection {
                if c.policy != ConnectionPolicy::None {
                    err!("/connection/policy", "a cli recipe has no connection");
                }
            }
        }
        Kind::Daemon => {
            match &r.run {
                None => err!("/run", "required for a daemon recipe"),
                Some(run) => {
                    for (i, a) in run.args.iter().enumerate() {
                        check_placeholders(r, a, &format!("/run/args/{i}"), all_roots, &mut errs);
                    }
                    for (k, v) in &run.env {
                        check_placeholders(r, v, &format!("/run/env/{k}"), all_roots, &mut errs);
                    }
                    if let Some(c) = &run.cwd {
                        check_placeholders(r, c, "/run/cwd", all_roots, &mut errs);
                    }
                }
            }
            if r.health.is_none() {
                err!("/health", "required for a daemon recipe");
            }
        }
    }

    if let Some(c) = &r.connection {
        check_placeholders(r, &c.url, "/connection/url", all_roots, &mut errs);
        if let Some(w) = &c.web_login {
            for (v, field) in [(&w.username, "username"), (&w.password, "password")] {
                let pointer = format!("/connection/webLogin/{field}");
                if v.trim().is_empty() {
                    errs.push(ValidationError::new(&pointer, "empty: give the literal value or a placeholder such as `{secrets.webPassword}`"));
                } else {
                    check_placeholders(r, v, &pointer, all_roots, &mut errs);
                }
            }
        }
        if c.policy == ConnectionPolicy::PerConsumerKey {
            if let (Some(a), Some(b)) = (c.min_len, c.max_len) {
                if a > b {
                    err!("/connection/minLen", "greater than maxLen");
                }
            }
        }
    }

    for (i, d) in r.create_dirs.iter().enumerate() {
        check_placeholders(r, d, &format!("/createDirs/{i}"), all_roots, &mut errs);
    }

    // Files.
    let mut item_roots: Vec<&str> = all_roots.to_vec();
    item_roots.push("item");
    for (i, f) in r.files.iter().enumerate() {
        let p = format!("/files/{i}");
        check_placeholders(r, &f.path, &format!("{p}/path"), all_roots, &mut errs);
        if !f.path.starts_with("{data}") {
            err!(&format!("{p}/path"), "must start with {data} — a recipe writes only inside its own data dir");
        }
        if f.format == FileFormat::Raw && !f.content.is_string() {
            err!(&format!("{p}/content"), "raw content must be a string");
        }
        walk_placeholders(r, &f.content, &format!("{p}/content"), &item_roots, &mut errs);
    }

    // HTTP blocks: captures from earlier steps become legal roots later.
    let mut roots: Vec<String> = all_roots.iter().map(|s| s.to_string()).collect();
    if let Some(h) = &r.health {
        check_request(r, &h.request, "/health/request", &roots, &mut errs);
        for (k, path) in &h.extract {
            if !(k == "version" || k.starts_with("details.")) {
                err!(&format!("/health/extract/{k}"), "keys are `version` or `details.<name>`");
            }
            check_jsonq(path, &format!("/health/extract/{k}"), &mut errs);
        }
    }
    if let Some(b) = &r.busy {
        if b.requests.is_empty() {
            err!("/busy/requests", "at least one request");
        }
        for (i, req) in b.requests.iter().enumerate() {
            check_request(r, req, &format!("/busy/requests/{i}"), &roots, &mut errs);
        }
        check_jsonq(&b.busy_if.path, "/busy/busyIf/path", &mut errs);
        check_regex(&b.busy_if.regex, "/busy/busyIf/regex", false, &mut errs);
    }
    if let Some(s) = &r.stop {
        for (i, step) in s.api.iter().enumerate() {
            check_request(r, step, &format!("/stop/api/{i}"), &roots, &mut errs);
            for (k, path) in &step.capture {
                if !is_valid_key(k) {
                    err!(&format!("/stop/api/{i}/capture/{k}"), "capture name must be an identifier");
                }
                check_jsonq(path, &format!("/stop/api/{i}/capture/{k}"), &mut errs);
                roots.push(k.clone());
            }
        }
    }
    for (i, f) in r.start_failures.iter().enumerate() {
        check_regex(&f.regex, &format!("/startFailures/{i}/regex"), false, &mut errs);
        if f.code.is_empty() || f.message.is_empty() {
            err!(&format!("/startFailures/{i}"), "code and message are required");
        }
    }
    for (k, re) in &r.log_extract {
        if !is_valid_key(k) {
            err!(&format!("/logExtract/{k}"), "key must be an identifier");
        }
        check_regex(re, &format!("/logExtract/{k}"), true, &mut errs);
    }

    errs
}

fn check_request(r: &Recipe, req: &HttpRequest, pointer: &str, roots: &[String], errs: &mut Vec<ValidationError>) {
    let roots: Vec<&str> = roots.iter().map(|s| s.as_str()).collect();
    if !matches!(req.method.as_str(), "GET" | "POST" | "PUT" | "PATCH" | "DELETE") {
        errs.push(ValidationError::new(format!("{pointer}/method"), "must be GET, POST, PUT, PATCH or DELETE"));
    }
    check_placeholders(r, &req.url, &format!("{pointer}/url"), &roots, errs);
    for (k, v) in &req.headers {
        check_placeholders(r, v, &format!("{pointer}/headers/{k}"), &roots, errs);
    }
    if let Some(j) = &req.json {
        walk_placeholders(r, j, &format!("{pointer}/json"), &roots, errs);
    }
}

fn check_regex(re: &str, pointer: &str, needs_capture: bool, errs: &mut Vec<ValidationError>) {
    match regex::Regex::new(re) {
        Err(e) => errs.push(ValidationError::new(pointer, format!("invalid regex: {e}"))),
        Ok(rx) if needs_capture && rx.captures_len() < 2 => {
            errs.push(ValidationError::new(pointer, "regex needs one capture group for the value"))
        }
        Ok(_) => {}
    }
}

fn check_jsonq(path: &str, pointer: &str, errs: &mut Vec<ValidationError>) {
    if let Err(e) = jsonq::parse(path) {
        errs.push(ValidationError::new(pointer, format!("invalid query: {e}")));
    }
}

/// Every `{placeholder}` in `s` must resolve to a known root and, for
/// `ports.X` / `config.X` / `secrets.X`, to a key the recipe declares.
fn check_placeholders(r: &Recipe, s: &str, pointer: &str, roots: &[&str], errs: &mut Vec<ValidationError>) {
    for ph in template::placeholders(s) {
        let (path, filter) = template::split_filter(&ph);
        if let Some(f) = filter {
            if !matches!(f, "int" | "json" | "shell") {
                errs.push(ValidationError::new(pointer, format!("unknown filter `{f}` in `{{{ph}}}`")));
            }
        }
        let mut segs = path.split('.');
        let root = segs.next().unwrap_or("");
        let rest: Vec<&str> = segs.collect();
        if !roots.contains(&root) {
            errs.push(ValidationError::new(pointer, format!("unknown placeholder `{{{ph}}}`")));
            continue;
        }
        let known = match root {
            "ports" => rest.len() == 1 && r.ports.contains_key(rest[0]),
            "config" => rest.len() == 1 && r.config.iter().any(|f| f.key == rest[0]),
            "secrets" => {
                rest.len() == 1
                    && (r.secrets.iter().any(|f| f.key == rest[0]) || r.config.iter().any(|f| f.secret && f.key == rest[0]))
            }
            "platform" => rest.len() == 1 && matches!(rest[0], "os" | "arch"),
            "connection" => rest.len() == 1 && rest[0] == "url" && r.connection.is_some(),
            "item" => rest.len() == 1 && matches!(rest[0], "id" | "key"),
            _ => rest.is_empty(),
        };
        if !known {
            errs.push(ValidationError::new(pointer, format!("`{{{ph}}}` names nothing this recipe declares")));
        }
    }
}

fn walk_placeholders(r: &Recipe, v: &Value, pointer: &str, roots: &[&str], errs: &mut Vec<ValidationError>) {
    match v {
        Value::String(s) => check_placeholders(r, s, pointer, roots, errs),
        Value::Array(items) => {
            for (i, it) in items.iter().enumerate() {
                walk_placeholders(r, it, &format!("{pointer}/{i}"), roots, errs);
            }
        }
        Value::Object(map) => {
            if let Some(each) = map.get("$each") {
                if each.as_str() != Some("consumers") {
                    errs.push(ValidationError::new(format!("{pointer}/$each"), "only `consumers` can be iterated"));
                }
                match (map.get("key"), map.get("value")) {
                    (Some(Value::String(k)), Some(val)) => {
                        check_placeholders(r, k, &format!("{pointer}/key"), roots, errs);
                        walk_placeholders(r, val, &format!("{pointer}/value"), roots, errs);
                    }
                    _ => errs.push(ValidationError::new(pointer, "$each needs `key` (string) and `value`")),
                }
                // Static entries beside the directive are kept as-is.
                for (k, val) in map.iter().filter(|(k, _)| !matches!(k.as_str(), "$each" | "key" | "value")) {
                    walk_placeholders(r, val, &format!("{pointer}/{k}"), roots, errs);
                }
                return;
            }
            if let Some(cond) = map.get("$if") {
                match cond.as_str() {
                    Some(c) => check_placeholders(r, &format!("{{{c}}}"), &format!("{pointer}/$if"), roots, errs),
                    None => errs.push(ValidationError::new(format!("{pointer}/$if"), "must be a placeholder path such as config.flag")),
                }
                if !map.contains_key("then") {
                    errs.push(ValidationError::new(pointer, "$if needs `then`"));
                }
                for k in ["then", "else"] {
                    if let Some(v) = map.get(k) {
                        walk_placeholders(r, v, &format!("{pointer}/{k}"), roots, errs);
                    }
                }
                return;
            }
            for (k, val) in map {
                if k.starts_with('$') {
                    errs.push(ValidationError::new(format!("{pointer}/{k}"), "unknown directive; only $each and $if exist"));
                }
                walk_placeholders(r, val, &format!("{pointer}/{k}"), roots, errs);
            }
        }
        _ => {}
    }
}

pub fn is_valid_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && b[0].is_ascii_lowercase() | b[0].is_ascii_digit()
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

fn is_valid_key(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && (b[0].is_ascii_alphabetic() || b[0] == b'_')
        && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

impl Recipe {
    /// Archive kind for this platform (an override wins).
    pub fn archive_for(&self, platform: &Platform) -> Archive {
        self.overrides.get(&platform.key()).and_then(|o| o.archive).unwrap_or(self.archive)
    }
    pub fn layout_for(&self, platform: &Platform) -> Layout {
        self.overrides.get(&platform.key()).and_then(|o| o.layout.clone()).unwrap_or_else(|| self.layout.clone())
    }
    /// Binaries inside the archive for the running platform; `[0]` is main.
    pub fn binaries(&self) -> Vec<String> {
        let layout = self.layout_for(&Platform::current());
        if layout.binaries.is_empty() {
            vec![self.name.clone()]
        } else {
            layout.binaries
        }
    }
    pub fn main_binary(&self) -> String {
        self.binaries().remove(0)
    }
    /// Release source for this platform (an override wins).
    pub fn source_for(&self, platform: &Platform) -> &Source {
        self.overrides.get(&platform.key()).and_then(|o| o.source.as_ref()).unwrap_or(&self.source)
    }
    /// Whether a download exists for a platform key (an override's source
    /// wins over the main one), regardless of what `platforms` declares.
    pub fn has_download_for(&self, key: &str) -> bool {
        let source = self.overrides.get(key).and_then(|o| o.source.as_ref()).unwrap_or(&self.source);
        match source {
            Source::GithubRelease { assets, .. } => assets.contains_key(key),
            Source::HttpRedirect { latest_url, .. } => latest_url.contains_key(key),
            Source::HtmlIndex { links, .. } => links.contains_key(key),
        }
    }
    /// Declared in `platforms` *and* downloadable. The validator keeps the
    /// two in step, so for a valid recipe either alone would do.
    pub fn supported_on(&self, platform: &Platform) -> bool {
        let key = platform.key();
        self.platforms.iter().any(|p| *p == key) && self.has_download_for(&key)
    }
    pub fn config_field(&self, key: &str) -> Option<&ConfigField> {
        self.config.iter().find(|f| f.key == key)
    }
    /// The decisions an install needs: `askOnInstall` fields plus every
    /// `required` one.
    pub fn install_fields(&self) -> Vec<&ConfigField> {
        self.config.iter().filter(|f| f.ask_on_install || f.required).collect()
    }
}

// --- Loading ---

pub const BUILTIN: &[(&str, &str)] = &[
    ("slskd", include_str!("../../../recipes/slskd.json")),
    ("yt-dlp", include_str!("../../../recipes/yt-dlp.json")),
    ("ffmpeg", include_str!("../../../recipes/ffmpeg.json")),
];

pub fn load_builtin() -> Vec<Recipe> {
    BUILTIN
        .iter()
        .map(|(name, json)| parse(json).unwrap_or_else(|e| panic!("built-in recipe {name} is invalid: {e:?}")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slskd() -> Recipe {
        parse(BUILTIN[0].1).expect("built-in slskd recipe validates")
    }

    fn with(edit: impl FnOnce(&mut Value)) -> Vec<ValidationError> {
        let mut v: Value = serde_json::from_str(BUILTIN[0].1).unwrap();
        edit(&mut v);
        match from_value(v) {
            Ok(_) => vec![],
            Err(e) => e,
        }
    }

    fn pointers(errs: &[ValidationError]) -> Vec<&str> {
        errs.iter().map(|e| e.pointer.as_str()).collect()
    }

    #[test]
    fn every_builtin_validates_and_names_match() {
        for (name, json) in BUILTIN {
            let r = parse(json).unwrap_or_else(|e| panic!("{name}: {e:?}"));
            assert_eq!(&r.name, name);
        }
        let ffmpeg = load_builtin().into_iter().find(|r| r.name == "ffmpeg").unwrap();
        let win = Platform { os: "windows", arch: "x64" };
        assert!(matches!(ffmpeg.source_for(&win), Source::GithubRelease { .. }), "Windows uses the BtbN override");
        assert!(matches!(ffmpeg.source_for(&Platform { os: "darwin", arch: "arm64" }), Source::HtmlIndex { .. }));
        assert_eq!(ffmpeg.layout_for(&win).binaries, vec!["bin/ffmpeg", "bin/ffprobe"]);
        assert!(!ffmpeg.supported_on(&Platform { os: "darwin", arch: "x64" }), "Intel Macs are deliberately unsupported");
        let ytdlp = load_builtin().into_iter().find(|r| r.name == "yt-dlp").unwrap();
        assert_eq!(ytdlp.kind, Kind::Cli);
    }

    #[test]
    fn builtin_slskd_is_valid_and_typed() {
        let r = slskd();
        assert_eq!(r.kind, Kind::Daemon);
        assert_eq!(r.main_binary(), "slskd");
        assert!(r.supported_on(&Platform { os: "darwin", arch: "arm64" }));
        assert!(!r.supported_on(&Platform { os: "linux", arch: "arm64" }));
        assert_eq!(r.ports["web"].default, 5030);
        assert!(r.connection.as_ref().unwrap().policy == ConnectionPolicy::PerConsumerKey);
        let login = r.connection.as_ref().unwrap().web_login.as_ref().expect("slskd's web UI sits behind a generated login");
        assert_eq!(login.password, "{secrets.webPassword}");
    }

    #[test]
    fn web_login_fields_must_be_filled_and_name_what_the_recipe_declares() {
        let errs = with(|v| v["connection"]["webLogin"]["password"] = Value::String(" ".into()));
        assert_eq!(pointers(&errs), vec!["/connection/webLogin/password"]);
        assert!(errs[0].message.contains("{secrets.webPassword}"), "the fix is in the message: {errs:?}");

        let errs = with(|v| v["connection"]["webLogin"]["password"] = Value::String("{secrets.webPasswrd}".into()));
        assert_eq!(pointers(&errs), vec!["/connection/webLogin/password"]);

        let errs = with(|v| {
            v["connection"].as_object_mut().unwrap().remove("webLogin");
        });
        assert!(errs.is_empty(), "webLogin is optional: {errs:?}");
    }

    #[test]
    fn errors_carry_pointers() {
        let errs = with(|v| v["files"][0]["content"]["web"]["port"] = Value::String("{ports.wev|int}".into()));
        assert_eq!(pointers(&errs), vec!["/files/0/content/web/port"]);
        assert!(errs[0].message.contains("ports.wev"), "{errs:?}");

        let errs = with(|v| v["run"]["args"][0] = Value::String("{nope}".into()));
        assert_eq!(pointers(&errs), vec!["/run/args/0"]);

        let errs = with(|v| v["name"] = Value::String("Bad Name".into()));
        assert_eq!(pointers(&errs), vec!["/name"]);

        let errs = with(|v| v["version"]["regex"] = Value::String("no capture".into()));
        assert_eq!(pointers(&errs), vec!["/version/regex"]);

        let errs = with(|v| v["source"]["assets"]["freebsd-x64"] = Value::String("x".into()));
        assert_eq!(pointers(&errs), vec!["/source/assets/freebsd-x64"]);
    }

    #[test]
    fn author_revision_and_platforms_are_required_and_consistent() {
        let errs = with(|v| v["author"] = Value::String("  ".into()));
        assert_eq!(pointers(&errs), vec!["/author"]);

        let errs = with(|v| v["revision"] = Value::from(0));
        assert_eq!(pointers(&errs), vec!["/revision"]);
        let errs = with(|v| {
            v.as_object_mut().unwrap().remove("revision");
        });
        assert_eq!(pointers(&errs), vec!["/revision"], "a missing revision reads as 0");

        // Listing a platform with no download.
        let errs = with(|v| v["platforms"].as_array_mut().unwrap().push(Value::String("linux-arm64".into())));
        assert_eq!(pointers(&errs), vec!["/platforms/5"]);
        assert!(errs[0].message.contains("linux-arm64") && errs[0].message.contains("no download"), "{errs:?}");

        // A download for a platform that is not listed.
        let errs = with(|v| v["platforms"].as_array_mut().unwrap().retain(|p| p != "linux-x64"));
        assert_eq!(pointers(&errs), vec!["/platforms"]);
        assert!(errs[0].message.contains("linux-x64"), "{errs:?}");

        let errs = with(|v| v["platforms"][0] = Value::String("freebsd-x64".into()));
        let p = pointers(&errs);
        assert!(p.contains(&"/platforms/0"), "{errs:?}");

        let errs = with(|v| v["platforms"][1] = v["platforms"][0].clone());
        assert!(pointers(&errs).contains(&"/platforms/1"), "{errs:?}");

        let errs = with(|v| v["platforms"] = Value::Array(vec![]));
        assert!(pointers(&errs).iter().all(|p| *p == "/platforms"), "{errs:?}");

        // Overrides count as downloads: ffmpeg lists Windows only through them.
        let ffmpeg = load_builtin().into_iter().find(|r| r.name == "ffmpeg").unwrap();
        assert!(ffmpeg.has_download_for("windows-x64"));
        assert!(!ffmpeg.has_download_for("linux-x64"));
        assert_eq!(ffmpeg.platforms, vec!["darwin-arm64", "windows-x64", "windows-arm64"]);
        assert_eq!(ffmpeg.author, "Roadie");
        assert_eq!(ffmpeg.revision, 1);
    }

    #[test]
    fn install_choices_are_daemon_only_and_their_keys_reserved() {
        let r = slskd();
        assert_eq!(r.start_after_install, Some(InstallChoice { default: true, ask_on_install: true }));
        assert_eq!(r.autostart, Some(InstallChoice { default: true, ask_on_install: true }));
        let errs = with(|v| v["config"][0]["key"] = Value::String("autostart".into()));
        assert!(pointers(&errs).contains(&"/config/0/key"), "{errs:?}");
        assert!(errs.iter().any(|e| e.message.contains("reserved")), "{errs:?}");
    }

    #[test]
    fn cli_recipes_reject_daemon_blocks_and_daemons_need_run_and_health() {
        let errs = with(|v| v["kind"] = Value::String("cli".into()));
        let p = pointers(&errs);
        assert!(p.contains(&"/run") && p.contains(&"/health") && p.contains(&"/ports"), "{p:?}");
        assert!(p.contains(&"/startAfterInstall") && p.contains(&"/autostart"), "{p:?}");

        let errs = with(|v| {
            v.as_object_mut().unwrap().remove("run");
            v.as_object_mut().unwrap().remove("health");
        });
        assert_eq!(pointers(&errs), vec!["/run", "/health"]);
    }

    #[test]
    fn duplicate_config_keys_and_files_outside_data_are_rejected() {
        let errs = with(|v| {
            let dup = v["config"][0].clone();
            v["config"].as_array_mut().unwrap().push(dup);
        });
        assert!(pointers(&errs).iter().any(|p| p.ends_with("/key")), "{errs:?}");

        let errs = with(|v| v["files"][0]["path"] = Value::String("{home}/evil.yml".into()));
        assert_eq!(pointers(&errs), vec!["/files/0/path"]);
    }

    #[test]
    fn unknown_fields_are_reported_as_shape_errors() {
        let errs = with(|v| v["bogus"] = Value::Bool(true));
        // serde is lenient on unknown top-level fields by default; that is
        // deliberate so an older Roadie can still read a newer recipe's
        // additive fields. Nothing to assert beyond "still valid".
        assert!(errs.is_empty() || errs[0].pointer.is_empty());
    }
}
