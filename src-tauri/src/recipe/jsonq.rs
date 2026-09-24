//! A very small JSON query language for reading values off a tool's API
//! responses: `$` root, `.key`, `[n]`, `[*]` (every element), `..key`
//! (every descendant with that key). Returns every match; callers take the
//! first when they want one value.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seg {
    Key(String),
    Index(usize),
    Wildcard,
    Descend(String),
}

pub fn parse(path: &str) -> Result<Vec<Seg>, String> {
    let rest = path.strip_prefix('$').ok_or("query must start with $")?;
    let mut segs = Vec::new();
    let mut chars = rest.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        match c {
            '.' => {
                let descend = chars.peek().map(|(_, c)| *c) == Some('.');
                if descend {
                    chars.next();
                }
                let start = i + if descend { 2 } else { 1 };
                let mut end = start;
                while let Some((j, c)) = chars.peek().copied() {
                    if c == '.' || c == '[' {
                        break;
                    }
                    end = j + c.len_utf8();
                    chars.next();
                }
                let key = &rest[start..end];
                if key.is_empty() {
                    return Err(format!("empty key at offset {i}"));
                }
                segs.push(if descend { Seg::Descend(key.to_string()) } else { Seg::Key(key.to_string()) });
            }
            '[' => {
                let mut inner = String::new();
                let mut closed = false;
                for (_, c) in chars.by_ref() {
                    if c == ']' {
                        closed = true;
                        break;
                    }
                    inner.push(c);
                }
                if !closed {
                    return Err("unclosed [".into());
                }
                segs.push(if inner == "*" {
                    Seg::Wildcard
                } else {
                    Seg::Index(inner.parse().map_err(|_| format!("bad index `{inner}`"))?)
                });
            }
            other => return Err(format!("unexpected `{other}` at offset {i}")),
        }
    }
    Ok(segs)
}

pub fn query<'a>(v: &'a Value, path: &str) -> Result<Vec<&'a Value>, String> {
    let segs = parse(path)?;
    let mut current = vec![v];
    for seg in &segs {
        let mut next = Vec::new();
        for cur in current {
            match seg {
                Seg::Key(k) => {
                    if let Some(x) = cur.get(k) {
                        next.push(x);
                    }
                }
                Seg::Index(i) => {
                    if let Some(x) = cur.get(i) {
                        next.push(x);
                    }
                }
                Seg::Wildcard => match cur {
                    Value::Array(a) => next.extend(a.iter()),
                    Value::Object(o) => next.extend(o.values()),
                    _ => {}
                },
                Seg::Descend(k) => descend(cur, k, &mut next),
            }
        }
        current = next;
    }
    Ok(current)
}

fn descend<'a>(v: &'a Value, key: &str, out: &mut Vec<&'a Value>) {
    match v {
        Value::Object(o) => {
            if let Some(x) = o.get(key) {
                out.push(x);
            }
            for x in o.values() {
                descend(x, key, out);
            }
        }
        Value::Array(a) => {
            for x in a {
                descend(x, key, out);
            }
        }
        _ => {}
    }
}

pub fn first<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    query(v, path).ok()?.into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_segments() {
        assert_eq!(
            parse("$.a.b[0][*]..state").unwrap(),
            vec![Seg::Key("a".into()), Seg::Key("b".into()), Seg::Index(0), Seg::Wildcard, Seg::Descend("state".into())]
        );
        assert!(parse("a.b").is_err());
        assert!(parse("$.").is_err());
        assert!(parse("$[x]").is_err());
    }

    #[test]
    fn descends_and_wildcards() {
        let v = json!([{ "username": "a", "directories": [{ "files": [
            { "filename": "x", "state": "Completed, Succeeded" }, { "filename": "y", "state": "InProgress" } ] }] }]);
        let states: Vec<&str> = query(&v, "$..state").unwrap().iter().filter_map(|s| s.as_str()).collect();
        assert_eq!(states, vec!["Completed, Succeeded", "InProgress"]);
        assert_eq!(first(&v, "$[0].username").unwrap(), &json!("a"));
        assert_eq!(query(&v, "$[*].directories[*].files[*].filename").unwrap().len(), 2);
        assert_eq!(first(&json!({"version": {"current": "0.26.0"}}), "$.version.current").unwrap(), "0.26.0");
        assert!(first(&v, "$.nope").is_none());
    }
}
