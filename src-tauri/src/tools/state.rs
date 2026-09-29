//! `state.json` in a tool's data dir: chosen ports, generated secrets, the
//! user's configuration values, autostart flag. One file per tool, 0600,
//! because it holds every secret the tool needs (the rendered config file
//! holds them too, under the same permissions).

use crate::paths;
use crate::recipe::{self, FieldKind, Recipe};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::Path;

pub const STATE_FILE: &str = "state.json";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ToolState {
    pub schema: u32,
    pub installed_version: Option<String>,
    /// sha256 of the archive we installed — integrity of *our* copy.
    pub archive_sha256: Option<String>,
    pub ports: BTreeMap<String, u16>,
    /// Ports the app or the user chose at install: never moved.
    #[serde(default)]
    pub chosen_ports: std::collections::BTreeSet<String>,
    /// Generated secrets plus secret config fields, by key.
    pub secrets: BTreeMap<String, String>,
    /// Non-secret config values, by key.
    pub config: Map<String, Value>,
    pub autostart: bool,
    /// Set by an explicit Stop; keeps the startup reconcile from undoing it.
    pub user_stopped: bool,
    /// A config change or staged update needs a restart the daemon was too
    /// busy for.
    pub restart_pending: bool,
    pub last_start: Option<u64>,
}

pub fn load(data_dir: &Path) -> ToolState {
    std::fs::read_to_string(data_dir.join(STATE_FILE))
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default()
}

/// Load, then fill whatever a fresh install needs — secrets, port defaults,
/// config defaults — persisting if anything was filled.
pub fn load_or_init(recipe: &Recipe, data_dir: &Path, platform: &recipe::Platform) -> Result<ToolState, String> {
    let mut s = load(data_dir);
    if fill(recipe, &mut s, data_dir, platform)? {
        save(data_dir, &s)?;
    }
    Ok(s)
}

/// What `load_or_init` *would* produce, without writing anything: the
/// window shows an uninstalled tool's defaults (a downloads folder, a port)
/// in the install prompt before a single file exists.
pub fn preview(recipe: &Recipe, data_dir: &Path, platform: &recipe::Platform) -> ToolState {
    let mut s = load(data_dir);
    let _ = fill(recipe, &mut s, data_dir, platform);
    s
}

/// Fill missing secrets, ports and config defaults; `Ok(true)` when
/// anything changed.
fn fill(recipe: &Recipe, s: &mut ToolState, data_dir: &Path, platform: &recipe::Platform) -> Result<bool, String> {
    let mut dirty = false;
    if s.schema == 0 {
        s.schema = 1;
        dirty = true;
    }
    for sec in &recipe.secrets {
        // Any value given at install only has to be long enough; a secret
        // Roadie generates is minted once. One without `generate` stays
        // missing until an install supplies it.
        if s.secrets.get(&sec.key).is_some_and(|v| v.len() >= sec.min_len().min(sec.generated_len().unwrap_or(usize::MAX))) {
            continue;
        }
        if let Some(n) = sec.generated_len() {
            s.secrets.insert(sec.key.clone(), paths::random_hex(n / 2)?);
            dirty = true;
        }
    }
    for (name, port) in &recipe.ports {
        if !s.ports.contains_key(name) {
            s.ports.insert(name.clone(), port.default);
            dirty = true;
        }
    }
    let mut ctx = recipe::template::Ctx::empty(*platform);
    ctx.home = paths::home_dir().to_string_lossy().into_owned();
    ctx.data = data_dir.to_string_lossy().into_owned();
    for f in &recipe.fields() {
        if f.secret || s.config.contains_key(&f.key) {
            continue;
        }
        if let Some(d) = &f.default {
            let v = match d {
                Value::String(t) => recipe::template::expand(t, &ctx)?,
                other => other.clone(),
            };
            // Recipes write `{home}/Music/x`; on Windows `{home}` is `C:\Users\…`, so a path
            // default would come out half and half. Give it the platform's separator.
            let v = match v {
                Value::String(p) if f.kind == FieldKind::Path && platform.os == "windows" => Value::String(p.replace('/', "\\")),
                other => other,
            };
            s.config.insert(f.key.clone(), v);
            dirty = true;
        }
    }
    Ok(dirty)
}

pub fn save(data_dir: &Path, s: &ToolState) -> Result<(), String> {
    let contents = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    paths::write_atomic(&data_dir.join(STATE_FILE), contents.as_bytes(), true)
}

/// Apply a config patch: validates against the recipe's fields, routes
/// secret fields to `secrets`, everything else to `config`. Password rule:
/// key absent = keep, `""` = clear.
pub fn apply_patch(recipe: &Recipe, s: &mut ToolState, patch: &Map<String, Value>) -> Result<(), String> {
    for (k, v) in patch {
        let f = recipe
            .config_field(k)
            .ok_or_else(|| format!("unknown config key `{k}`"))?;
        match f.kind {
            FieldKind::Password => {
                let t = v.as_str().ok_or_else(|| format!("`{k}` must be a string"))?;
                if t.is_empty() {
                    s.secrets.remove(k);
                } else {
                    s.secrets.insert(k.clone(), t.to_string());
                }
            }
            FieldKind::Bool => {
                let b = v.as_bool().ok_or_else(|| format!("`{k}` must be true or false"))?;
                s.config.insert(k.clone(), Value::Bool(b));
            }
            FieldKind::Port => {
                let n = v.as_u64().filter(|n| (1..=65535).contains(n)).ok_or_else(|| format!("`{k}` must be 1–65535"))?;
                s.config.insert(k.clone(), Value::from(n));
            }
            FieldKind::Path => {
                let t = v.as_str().map(str::trim).ok_or_else(|| format!("`{k}` must be a string"))?;
                if t.is_empty() {
                    if f.required {
                        return Err(format!("`{}` cannot be empty", f.label));
                    }
                    s.config.remove(k);
                } else {
                    if !Path::new(t).is_absolute() {
                        return Err(format!("`{}` must be an absolute path", f.label));
                    }
                    s.config.insert(k.clone(), Value::String(t.to_string()));
                }
            }
            FieldKind::Paths => {
                let list = parse_paths(v).map_err(|e| format!("`{}`: {e}", f.label))?;
                if list.is_empty() && f.required {
                    return Err(format!("`{}` cannot be empty", f.label));
                }
                s.config.insert(k.clone(), Value::Array(list.into_iter().map(Value::String).collect()));
            }
            FieldKind::Text => {
                let t = v.as_str().map(str::trim).ok_or_else(|| format!("`{k}` must be a string"))?;
                if t.is_empty() {
                    if f.required {
                        return Err(format!("`{}` cannot be empty", f.label));
                    }
                    s.config.remove(k);
                } else {
                    s.config.insert(k.clone(), Value::String(t.to_string()));
                }
            }
        }
    }
    Ok(())
}

/// A `paths` value: a JSON array of strings, or one string holding either a
/// JSON array or one path per line (what `--set key=…` hands over). Every
/// entry must be absolute; blanks and repeats are dropped, order kept.
pub fn parse_paths(v: &Value) -> Result<Vec<String>, String> {
    let items: Vec<String> = match v {
        Value::Array(a) => a
            .iter()
            .map(|i| i.as_str().map(str::to_string).ok_or_else(|| "every entry must be a string".to_string()))
            .collect::<Result<_, _>>()?,
        Value::String(s) if s.trim_start().starts_with('[') => {
            let parsed: Value = serde_json::from_str(s).map_err(|e| format!("not a JSON array: {e}"))?;
            return parse_paths(&parsed);
        }
        Value::String(s) => s.lines().map(str::to_string).collect(),
        _ => return Err("must be a list of folders".into()),
    };
    let mut out: Vec<String> = Vec::new();
    for raw in items {
        let t = raw.trim();
        if t.is_empty() {
            continue;
        }
        if !Path::new(t).is_absolute() {
            return Err(format!("`{t}` is not an absolute path"));
        }
        if !out.iter().any(|o| o == t) {
            out.push(t.to_string());
        }
    }
    Ok(out)
}

/// Check a patch against the recipe without touching any state — the API
/// uses it to reject a bad install body before a prompt is shown.
pub fn validate_patch(recipe: &Recipe, patch: &Map<String, Value>) -> Result<(), String> {
    let mut scratch = ToolState::default();
    apply_patch(recipe, &mut scratch, patch)
}

/// Config as the UI and API see it: non-secret values plus `has<Key>`
/// booleans for secret fields, never the secrets themselves.
pub fn public_config(recipe: &Recipe, s: &ToolState) -> Map<String, Value> {
    let mut out = s.config.clone();
    for f in recipe.config.iter().filter(|f| f.secret) {
        out.insert(format!("has_{}", f.key), Value::Bool(s.secrets.contains_key(&f.key)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recipe::Platform;

    fn slskd() -> Recipe {
        recipe::fixtures::recipe("slskd")
    }

    #[test]
    fn paths_take_a_json_array_or_one_folder_per_line() {
        let abs = if cfg!(windows) { ("C:\\m", "C:\\n") } else { ("/m", "/n") };
        let want = vec![abs.0.to_string(), abs.1.to_string()];
        assert_eq!(parse_paths(&serde_json::json!([abs.0, abs.1, abs.0])).unwrap(), want, "repeats dropped, order kept");
        assert_eq!(parse_paths(&Value::String(format!("{}\n\n  {}  \n", abs.0, abs.1))).unwrap(), want, "blank lines and padding dropped");
        assert_eq!(parse_paths(&Value::String(serde_json::to_string(&want).unwrap())).unwrap(), want, "a JSON array in a --set string");
        assert!(parse_paths(&serde_json::json!(["relative/dir"])).unwrap_err().contains("not an absolute path"));
        assert!(parse_paths(&serde_json::json!(true)).is_err());

        let r = slskd();
        let mut st = ToolState::default();
        let mut patch = Map::new();
        patch.insert("shares.directories".into(), Value::String(format!("{}\n{}", abs.0, abs.1)));
        apply_patch(&r, &mut st, &patch).unwrap();
        assert_eq!(st.config["shares.directories"], serde_json::json!(want));
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("roadie-state-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn init_mints_secrets_once_and_fills_defaults() {
        let dir = tmp("init");
        let r = slskd();
        let a = load_or_init(&r, &dir, &Platform::current()).unwrap();
        assert_eq!(a.secrets["internalKey"].len(), 48);
        assert_eq!(a.ports["web"], 5030);
        assert_eq!(a.config["shareDownloads"], Value::Bool(true));
        assert!(a.config["downloadsDir"].as_str().unwrap().ends_with("Soulseek"));
        assert!(!a.config.contains_key("soulseekPassword"), "secret fields never land in config");
        let b = load_or_init(&r, &dir, &Platform::current()).unwrap();
        assert_eq!(a.secrets, b.secrets, "secrets must be stable across loads");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn path_defaults_use_the_platform_separator() {
        let r = slskd();
        let win = Platform { os: "windows", arch: "x64" };
        let mut s = ToolState::default();
        fill(&r, &mut s, Path::new("C:\\data"), &win).unwrap();
        let d = s.config["downloadsDir"].as_str().unwrap();
        assert!(!d.contains('/') && d.ends_with("\\Music\\Soulseek"), "{d}");
        let mac = Platform { os: "darwin", arch: "arm64" };
        let mut s = ToolState::default();
        fill(&r, &mut s, Path::new("/data"), &mac).unwrap();
        assert!(s.config["downloadsDir"].as_str().unwrap().ends_with("/Music/Soulseek"));
    }

    #[test]
    fn patch_routes_secrets_and_validates() {
        let r = slskd();
        let mut s = ToolState::default();
        let mut p = Map::new();
        p.insert("soulseekPassword".into(), Value::String("pw".into()));
        p.insert("soulseekUsername".into(), Value::String("  björk ".into()));
        p.insert("shareDownloads".into(), Value::Bool(false));
        apply_patch(&r, &mut s, &p).unwrap();
        assert_eq!(s.secrets["soulseekPassword"], "pw");
        assert_eq!(s.config["soulseekUsername"], "björk");
        assert!(!s.config.contains_key("soulseekPassword"));
        let pubc = public_config(&r, &s);
        assert_eq!(pubc["has_soulseekPassword"], Value::Bool(true));
        assert!(!pubc.contains_key("soulseekPassword"));

        let mut clear = Map::new();
        clear.insert("soulseekPassword".into(), Value::String(String::new()));
        apply_patch(&r, &mut s, &clear).unwrap();
        assert!(!s.secrets.contains_key("soulseekPassword"));

        let mut bad = Map::new();
        bad.insert("downloadsDir".into(), Value::String("relative/dir".into()));
        assert!(apply_patch(&r, &mut s, &bad).unwrap_err().contains("absolute"));
        let mut unknown = Map::new();
        unknown.insert("nope".into(), Value::Bool(true));
        assert!(apply_patch(&r, &mut s, &unknown).unwrap_err().contains("unknown config key"));
    }
}
