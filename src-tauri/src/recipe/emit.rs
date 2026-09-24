//! Serialize an expanded `files[].content` tree to text. Small emitters, no
//! crates: the YAML one writes every string as a JSON string, which is a valid
//! YAML double-quoted scalar for everything `serde_json` produces — so a
//! password full of `#`, `:` and quotes, or a Windows path, cannot break the
//! file or be read back differently.

use super::FileFormat;
use serde_json::Value;

pub fn render(format: FileFormat, content: &Value) -> Result<String, String> {
    Ok(match format {
        FileFormat::Yaml => {
            let mut out = String::new();
            yaml(content, 0, &mut out);
            out
        }
        FileFormat::Json => serde_json::to_string_pretty(content).map_err(|e| e.to_string())? + "\n",
        FileFormat::Env => env(content)?,
        FileFormat::Ini => ini(content)?,
        FileFormat::Raw => match content {
            Value::String(s) => s.clone(),
            _ => return Err("raw content must be a string".into()),
        },
    })
}

fn yaml_scalar(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => serde_json::to_string(s).expect("string serializes"),
        // Non-scalar in scalar position: inline JSON is valid YAML flow style.
        other => serde_json::to_string(other).expect("value serializes"),
    }
}

fn yaml_key(k: &str) -> String {
    let simple = !k.is_empty()
        && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        && !matches!(k, "true" | "false" | "null" | "yes" | "no" | "on" | "off");
    if simple {
        k.to_string()
    } else {
        serde_json::to_string(k).expect("string serializes")
    }
}

fn yaml(v: &Value, indent: usize, out: &mut String) {
    let pad = " ".repeat(indent);
    match v {
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}\n");
                return;
            }
            for (k, val) in map {
                out.push_str(&pad);
                out.push_str(&yaml_key(k));
                out.push(':');
                match val {
                    Value::Object(m) if !m.is_empty() => {
                        out.push('\n');
                        yaml(val, indent + 2, out);
                    }
                    Value::Array(a) if !a.is_empty() => {
                        out.push('\n');
                        yaml(val, indent + 2, out);
                    }
                    Value::Array(_) => out.push_str(" []\n"),
                    Value::Object(_) => out.push_str(" {}\n"),
                    scalar => {
                        out.push(' ');
                        out.push_str(&yaml_scalar(scalar));
                        out.push('\n');
                    }
                }
            }
        }
        Value::Array(items) => {
            for it in items {
                out.push_str(&pad);
                out.push_str("- ");
                match it {
                    Value::Object(m) if !m.is_empty() => {
                        // First key on the dash line, the rest indented under it.
                        let mut inner = String::new();
                        yaml(it, indent + 2, &mut inner);
                        out.push_str(inner.trim_start());
                    }
                    Value::Array(a) if !a.is_empty() => {
                        out.push('\n');
                        yaml(it, indent + 2, out);
                    }
                    scalar => {
                        out.push_str(&yaml_scalar(scalar));
                        out.push('\n');
                    }
                }
            }
        }
        scalar => {
            out.push_str(&pad);
            out.push_str(&yaml_scalar(scalar));
            out.push('\n');
        }
    }
}

fn env_value(v: &Value) -> Result<String, String> {
    Ok(match v {
        Value::String(s) => {
            if s.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-./:@".contains(&b)) {
                s.clone()
            } else {
                serde_json::to_string(s).expect("string serializes")
            }
        }
        Value::Bool(_) | Value::Number(_) => v.to_string(),
        Value::Null => String::new(),
        other => return Err(format!("env values must be scalars, got {other}")),
    })
}

fn env(v: &Value) -> Result<String, String> {
    let map = v.as_object().ok_or("env content must be an object")?;
    let mut out = String::new();
    for (k, val) in map {
        out.push_str(k);
        out.push('=');
        out.push_str(&env_value(val)?);
        out.push('\n');
    }
    Ok(out)
}

fn ini(v: &Value) -> Result<String, String> {
    let map = v.as_object().ok_or("ini content must be an object")?;
    let mut out = String::new();
    // Top-level scalars first (global section), then one [section] per object.
    for (k, val) in map.iter().filter(|(_, v)| !v.is_object()) {
        out.push_str(&format!("{k} = {}\n", env_value(val)?));
    }
    for (section, val) in map.iter().filter(|(_, v)| v.is_object()) {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("[{section}]\n"));
        for (k, item) in val.as_object().expect("filtered") {
            out.push_str(&format!("{k} = {}\n", env_value(item)?));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn yaml_round_trips_hostile_password_and_windows_path() {
        let pass = "p#a:s\"s\\w'ord ü";
        let v = json!({
            "web": { "port": 5030, "address": "127.0.0.1", "authentication": { "password": pass,
                     "api_keys": { "viboplr": { "key": "k", "cidr": "127.0.0.1/32" } } } },
            "directories": { "downloads": "C:\\Users\\x\\Music\\Soulseek" },
            "shares": { "directories": ["C:\\Users\\x\\Music\\Soulseek"] },
            "empty": { "list": [], "map": {} },
            "flag": true
        });
        let y = render(FileFormat::Yaml, &v).unwrap();
        assert!(y.contains("  port: 5030\n"), "{y}");
        assert!(y.contains("  address: \"127.0.0.1\"\n"));
        assert!(y.contains(&format!("    password: {}\n", serde_json::to_string(pass).unwrap())), "{y}");
        assert!(y.contains("        key: \"k\"\n"));
        assert!(y.contains(r#"  downloads: "C:\\Users\\x\\Music\\Soulseek""#));
        assert!(y.contains("  directories:\n    - \"C:\\\\Users"));
        assert!(y.contains("  list: []\n") && y.contains("  map: {}\n"));
        assert!(y.contains("flag: true\n"));
        // Read the password back the way a YAML parser would: JSON string decode.
        let line = y.lines().find(|l| l.trim_start().starts_with("password:")).unwrap();
        let back: String = serde_json::from_str(line.split_once(':').unwrap().1.trim()).unwrap();
        assert_eq!(back, pass);
    }

    #[test]
    fn yaml_lists_of_objects_and_odd_keys() {
        let v = json!({ "items": [{ "a": 1, "b": "x" }, { "a": 2 }], "true": 1, "with space": 2 });
        let y = render(FileFormat::Yaml, &v).unwrap();
        assert!(y.contains("items:\n  - a: 1\n    b: \"x\"\n  - a: 2\n"), "{y}");
        assert!(y.contains("\"true\": 1\n") && y.contains("\"with space\": 2\n"));
    }

    #[test]
    fn env_ini_json_raw() {
        let v = json!({ "PORT": 3030, "DIR": "/a b", "ON": true });
        assert_eq!(render(FileFormat::Env, &v).unwrap(), "PORT=3030\nDIR=\"/a b\"\nON=true\n", "author order is kept");
        let v = json!({ "global": 1, "sec": { "k": "v" } });
        assert_eq!(render(FileFormat::Ini, &v).unwrap(), "global = 1\n\n[sec]\nk = v\n");
        assert_eq!(render(FileFormat::Raw, &json!("plain")).unwrap(), "plain");
        assert!(render(FileFormat::Raw, &json!(1)).is_err());
        assert!(render(FileFormat::Json, &json!({"a":1})).unwrap().ends_with("}\n"));
    }
}
