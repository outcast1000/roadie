//! `{placeholder}` expansion over a per-tool context, plus the two structural
//! directives (`$each` over consumers, `$if`/`then`/`else`) used inside
//! `files[].content`. No text templating: config files are JSON objects
//! expanded here and serialized by `emit`, so quoting is by construction.
//!
//! Roots: `home data bin version platform.os platform.arch ports.X config.X
//! secrets.X connection.url item.id item.key` and any capture a previous
//! HTTP step stored. Filters: `|int` (emit as number), `|json`
//! (JSON-string-quoted), `|shell` (single-quoted for a POSIX shell).
//! `{{` and `}}` are literal braces.

use super::Platform;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Consumer {
    pub id: String,
    pub key: String,
}

#[derive(Debug, Clone)]
pub struct Ctx {
    pub home: String,
    pub data: String,
    pub bin: String,
    pub version: String,
    pub platform: Platform,
    pub ports: BTreeMap<String, u16>,
    pub config: Map<String, Value>,
    pub secrets: BTreeMap<String, String>,
    pub connection_url: Option<String>,
    pub consumers: Vec<Consumer>,
    /// Captures from HTTP steps and the current `$each` item.
    pub vars: BTreeMap<String, Value>,
}

impl Ctx {
    pub fn empty(platform: Platform) -> Self {
        Ctx {
            home: String::new(),
            data: String::new(),
            bin: String::new(),
            version: String::new(),
            platform,
            ports: BTreeMap::new(),
            config: Map::new(),
            secrets: BTreeMap::new(),
            connection_url: None,
            consumers: Vec::new(),
            vars: BTreeMap::new(),
        }
    }
}

/// The placeholder bodies in `s`, in order, with filters (`ports.web|int`).
pub fn placeholders(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '{' {
            if chars.peek().map(|(_, c)| *c) == Some('{') {
                chars.next();
                continue;
            }
            if let Some(end) = s[i + 1..].find('}') {
                let body = &s[i + 1..i + 1 + end];
                if !body.is_empty() && !body.contains('{') {
                    out.push(body.to_string());
                }
            }
        }
    }
    out
}

pub fn split_filter(body: &str) -> (&str, Option<&str>) {
    match body.split_once('|') {
        Some((p, f)) => (p.trim(), Some(f.trim())),
        None => (body.trim(), None),
    }
}

fn resolve(path: &str, ctx: &Ctx) -> Result<Value, String> {
    let mut segs = path.split('.');
    let root = segs.next().unwrap_or("");
    let rest: Vec<&str> = segs.collect();
    let missing = || format!("`{{{path}}}` is not set");
    Ok(match (root, rest.as_slice()) {
        ("home", []) => Value::String(ctx.home.clone()),
        ("data", []) => Value::String(ctx.data.clone()),
        ("bin", []) => Value::String(ctx.bin.clone()),
        ("version", []) => Value::String(ctx.version.clone()),
        ("platform", ["os"]) => Value::String(ctx.platform.os.into()),
        ("platform", ["arch"]) => Value::String(ctx.platform.arch.into()),
        ("ports", [k]) => Value::from(*ctx.ports.get(*k).ok_or_else(missing)?),
        ("config", [k]) => ctx.config.get(*k).cloned().unwrap_or(Value::Null),
        ("secrets", [k]) => Value::String(ctx.secrets.get(*k).cloned().unwrap_or_default()),
        ("connection", ["url"]) => Value::String(ctx.connection_url.clone().ok_or_else(missing)?),
        (root, rest) => {
            let mut v = ctx.vars.get(root).ok_or_else(missing)?;
            for k in rest {
                v = v.get(*k).ok_or_else(missing)?;
            }
            v.clone()
        }
    })
}

fn scalar_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        other => other.to_string(),
    }
}

fn apply_filter(v: Value, filter: Option<&str>, path: &str) -> Result<Value, String> {
    Ok(match filter {
        None => v,
        Some("int") => match &v {
            Value::Number(_) => v,
            Value::String(s) => Value::from(
                s.trim()
                    .parse::<i64>()
                    .map_err(|_| format!("`{{{path}|int}}`: `{s}` is not an integer"))?,
            ),
            other => return Err(format!("`{{{path}|int}}`: cannot make a number of {other}")),
        },
        Some("json") => Value::String(serde_json::to_string(&scalar_text(&v)).expect("string serializes")),
        Some("shell") => Value::String(format!("'{}'", scalar_text(&v).replace('\'', "'\\''"))),
        Some(f) => return Err(format!("unknown filter `{f}`")),
    })
}

/// Expand a string. A string that is exactly one placeholder keeps the
/// resolved value's type (a bool stays a bool, `|int` yields a number);
/// anything else interpolates to text.
pub fn expand(s: &str, ctx: &Ctx) -> Result<Value, String> {
    let trimmed = s.trim();
    if trimmed.starts_with('{') && !trimmed.starts_with("{{") && trimmed.ends_with('}') && placeholders(trimmed).len() == 1 {
        let body = &trimmed[1..trimmed.len() - 1];
        if !body.contains('}') {
            let (path, filter) = split_filter(body);
            return apply_filter(resolve(path, ctx)?, filter, path);
        }
    }
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i].replace("}}", "}"));
        rest = &rest[i..];
        if rest.starts_with("{{") {
            out.push('{');
            rest = &rest[2..];
            continue;
        }
        let Some(end) = rest.find('}') else {
            out.push_str(rest);
            rest = "";
            break;
        };
        let body = &rest[1..end];
        let (path, filter) = split_filter(body);
        let v = apply_filter(resolve(path, ctx)?, filter, path)?;
        out.push_str(&scalar_text(&v));
        rest = &rest[end + 1..];
    }
    out.push_str(&rest.replace("}}", "}"));
    Ok(Value::String(out))
}

pub fn expand_string(s: &str, ctx: &Ctx) -> Result<String, String> {
    Ok(scalar_text(&expand(s, ctx)?))
}

/// Expand a JSON tree: strings expand, `$each` fans out over consumers,
/// `$if` picks a branch, everything else recurses.
pub fn expand_value(v: &Value, ctx: &Ctx) -> Result<Value, String> {
    match v {
        Value::String(s) => expand(s, ctx),
        Value::Array(items) => items.iter().map(|i| expand_value(i, ctx)).collect::<Result<Vec<_>, _>>().map(Value::Array),
        Value::Object(map) => {
            if map.get("$each").is_some() {
                let key_t = map.get("key").and_then(|k| k.as_str()).ok_or("$each needs a string `key`")?;
                let val_t = map.get("value").ok_or("$each needs `value`")?;
                let mut out = Map::new();
                // Static entries first; consumer entries never overwrite them.
                for (k, val) in map.iter().filter(|(k, _)| !matches!(k.as_str(), "$each" | "key" | "value")) {
                    out.insert(k.clone(), expand_value(val, ctx)?);
                }
                for c in &ctx.consumers {
                    if out.contains_key(&c.id) {
                        continue;
                    }
                    let mut inner = ctx.clone();
                    inner.vars.insert(
                        "item".into(),
                        serde_json::json!({ "id": c.id, "key": c.key }),
                    );
                    let k = expand_string(key_t, &inner)?;
                    out.insert(k, expand_value(val_t, &inner)?);
                }
                return Ok(Value::Object(out));
            }
            if let Some(cond) = map.get("$if") {
                let path = cond.as_str().ok_or("$if must be a placeholder path")?;
                let truthy = match resolve(path, ctx)? {
                    Value::Bool(b) => b,
                    Value::Null => false,
                    Value::String(s) => !s.is_empty(),
                    Value::Number(n) => n.as_f64().unwrap_or(0.0) != 0.0,
                    Value::Array(a) => !a.is_empty(),
                    Value::Object(o) => !o.is_empty(),
                };
                let branch = if truthy { map.get("then") } else { map.get("else") };
                return match branch {
                    Some(b) => expand_value(b, ctx),
                    None => Ok(Value::Null),
                };
            }
            let mut out = Map::new();
            for (k, val) in map {
                let expanded = expand_value(val, ctx)?;
                // A `$if` without `else` that evaluated false drops the key.
                if expanded.is_null() && matches!(val, Value::Object(m) if m.contains_key("$if")) {
                    continue;
                }
                out.insert(k.clone(), expanded);
            }
            Ok(Value::Object(out))
        }
        other => Ok(other.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx() -> Ctx {
        let mut c = Ctx::empty(Platform { os: "darwin", arch: "arm64" });
        c.home = "/Users/a b".into();
        c.data = "/data".into();
        c.ports.insert("web".into(), 5030);
        c.config.insert("dir".into(), json!("/Users/a b/Music"));
        c.config.insert("share".into(), json!(true));
        c.secrets.insert("pw".into(), "p#a:s\"s".into());
        c.connection_url = Some("http://127.0.0.1:5030".into());
        c.consumers = vec![
            Consumer { id: "viboplr".into(), key: "k1".into() },
            Consumer { id: "other".into(), key: "k2".into() },
        ];
        c
    }

    #[test]
    fn lists_placeholders_and_filters() {
        assert_eq!(placeholders("{a}/{b.c|int} {{lit}}"), vec!["a", "b.c|int"]);
        assert_eq!(split_filter("ports.web|int"), ("ports.web", Some("int")));
    }

    #[test]
    fn single_placeholder_keeps_type_and_filters_apply() {
        let c = ctx();
        assert_eq!(expand("{ports.web}", &c).unwrap(), json!(5030));
        assert_eq!(expand("{ports.web|int}", &c).unwrap(), json!(5030));
        assert_eq!(expand("{config.share}", &c).unwrap(), json!(true));
        assert_eq!(expand("{secrets.pw|json}", &c).unwrap(), json!("\"p#a:s\\\"s\""));
        assert_eq!(expand("{home|shell}", &c).unwrap(), json!("'/Users/a b'"));
        assert_eq!(expand("{config.dir}/.incomplete", &c).unwrap(), json!("/Users/a b/Music/.incomplete"));
        assert_eq!(expand("{{literal}} {platform.os}", &c).unwrap(), json!("{literal} darwin"));
        assert!(expand("{ports.nope}", &c).unwrap_err().contains("ports.nope"));
        assert!(expand("{home|int}", &c).unwrap_err().contains("not an integer"));
    }

    #[test]
    fn each_and_if_expand_structurally() {
        let c = ctx();
        let v = json!({
            "api_keys": { "roadie": { "key": "{secrets.pw}" }, "$each": "consumers", "key": "{item.id}", "value": { "key": "{item.key}", "role": "readwrite" } },
            "shares": { "$if": "config.share", "then": ["{config.dir}"], "else": [] },
            "gone": { "$if": "config.missing", "then": 1 },
            "port": "{ports.web|int}"
        });
        let out = expand_value(&v, &c).unwrap();
        assert_eq!(out["api_keys"]["roadie"]["key"], json!("p#a:s\"s"), "static entry kept beside $each");
        assert_eq!(out["api_keys"]["viboplr"]["key"], json!("k1"));
        assert_eq!(out["api_keys"]["other"]["role"], json!("readwrite"));
        assert_eq!(out["shares"], json!(["/Users/a b/Music"]));
        assert!(out.get("gone").is_none(), "false $if without else drops the key");
        assert_eq!(out["port"], json!(5030));
    }
}
