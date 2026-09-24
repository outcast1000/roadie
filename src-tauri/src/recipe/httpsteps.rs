//! Run the HTTP requests a recipe describes (health, busy, stop chains)
//! against the tool's local API. Blocking `reqwest`, short timeouts, no
//! proxy: these run on status reads, so an unresponsive daemon must cost a
//! few seconds, not a hang. Captures (`{token}`) flow through `ctx.vars`.

use super::template::{self, Ctx};
use super::{jsonq, HttpRequest};
use serde_json::Value;
use std::time::Duration;

#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    pub json: Value,
}

#[derive(Debug)]
pub enum SendError {
    /// Nothing answered (refused / timeout / DNS).
    Unreachable(String),
    Other(String),
}

fn client(timeout: Duration) -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent("Roadie")
        .timeout(timeout)
        .no_proxy()
        .build()
        .map_err(|e| format!("HTTP client error: {e}"))
}

pub fn err_chain(e: &reqwest::Error) -> String {
    let mut msg = e.to_string();
    let mut src = std::error::Error::source(e);
    while let Some(s) = src {
        msg.push_str(": ");
        msg.push_str(&s.to_string());
        src = s.source();
    }
    msg
}

/// Expand and send one request. Does not judge the status code.
pub fn send(req: &HttpRequest, ctx: &Ctx) -> Result<Reply, SendError> {
    let url = template::expand_string(&req.url, ctx).map_err(SendError::Other)?;
    let c = client(Duration::from_secs(req.timeout_sec.max(1))).map_err(SendError::Other)?;
    let method = reqwest::Method::from_bytes(req.method.as_bytes()).map_err(|e| SendError::Other(e.to_string()))?;
    let mut builder = c.request(method, &url);
    for (k, v) in &req.headers {
        builder = builder.header(k, template::expand_string(v, ctx).map_err(SendError::Other)?);
    }
    if let Some(body) = &req.json {
        builder = builder.json(&template::expand_value(body, ctx).map_err(SendError::Other)?);
    }
    let resp = builder.send().map_err(|e| {
        if e.is_connect() || e.is_timeout() {
            SendError::Unreachable(err_chain(&e))
        } else {
            SendError::Other(err_chain(&e))
        }
    })?;
    let status = resp.status().as_u16();
    let text = resp.text().unwrap_or_default();
    let json = serde_json::from_str(&text).unwrap_or(Value::Null);
    Ok(Reply { status, json })
}

/// Run a chain, requiring 2xx from each step and storing its captures for
/// the next. Used for the graceful-stop route.
pub fn run_chain(steps: &[HttpRequest], ctx: &Ctx) -> Result<(), String> {
    let mut ctx = ctx.clone();
    for (i, step) in steps.iter().enumerate() {
        let reply = send(step, &ctx).map_err(|e| match e {
            SendError::Unreachable(m) => format!("step {i}: unreachable: {m}"),
            SendError::Other(m) => format!("step {i}: {m}"),
        })?;
        if !(200..300).contains(&reply.status) {
            return Err(format!("step {i} ({} {}): HTTP {}", step.method, step.url, reply.status));
        }
        for (name, path) in &step.capture {
            let v = jsonq::first(&reply.json, path)
                .cloned()
                .ok_or_else(|| format!("step {i}: `{path}` not found in the response"))?;
            ctx.vars.insert(name.clone(), v);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recipe::Platform;
    use std::collections::BTreeMap;
    use std::io::{Read, Write};

    /// One-shot HTTP/1.1 responder on a random loopback port.
    fn serve(responses: Vec<(u16, &'static str)>) -> (u16, std::thread::JoinHandle<Vec<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for (status, body) in responses {
                let (mut s, _) = listener.accept().unwrap();
                let mut buf = [0u8; 8192];
                let n = s.read(&mut buf).unwrap();
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                // Read the JSON body if a Content-Length says there is more.
                seen.push(req);
                let _ = s.write_all(
                    format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
                        .as_bytes(),
                );
            }
            seen
        });
        (port, h)
    }

    fn req(method: &str, url: &str) -> HttpRequest {
        HttpRequest {
            method: method.into(),
            url: url.into(),
            headers: BTreeMap::new(),
            json: None,
            capture: BTreeMap::new(),
            timeout_sec: 2,
        }
    }

    #[test]
    fn chain_captures_and_forwards_a_token() {
        let (port, h) = serve(vec![(200, r#"{"token":"abc123"}"#), (204, "")]);
        let mut ctx = Ctx::empty(Platform::current());
        ctx.connection_url = Some(format!("http://127.0.0.1:{port}"));
        ctx.secrets.insert("pw".into(), "s3".into());
        let mut login = req("POST", "{connection.url}/api/v0/session");
        login.json = Some(serde_json::json!({ "username": "roadie", "password": "{secrets.pw}" }));
        login.capture.insert("token".into(), "$.token".into());
        let mut shutdown = req("DELETE", "{connection.url}/api/v0/application");
        shutdown.headers.insert("Authorization".into(), "Bearer {token}".into());
        run_chain(&[login, shutdown], &ctx).unwrap();
        let seen = h.join().unwrap();
        assert!(seen[0].starts_with("POST /api/v0/session"));
        assert!(seen[0].contains(r#""password":"s3""#), "{}", seen[0]);
        assert!(seen[1].starts_with("DELETE /api/v0/application"));
        assert!(seen[1].to_lowercase().contains("authorization: bearer abc123"));
    }

    #[test]
    fn non_2xx_stops_the_chain_and_refused_is_unreachable() {
        let (port, h) = serve(vec![(401, "{}")]);
        let mut ctx = Ctx::empty(Platform::current());
        ctx.connection_url = Some(format!("http://127.0.0.1:{port}"));
        let err = run_chain(&[req("GET", "{connection.url}/x"), req("GET", "{connection.url}/y")], &ctx).unwrap_err();
        assert!(err.contains("HTTP 401"), "{err}");
        h.join().unwrap();

        let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        match send(&req("GET", &format!("http://127.0.0.1:{dead}/")), &ctx) {
            Err(SendError::Unreachable(_)) => {}
            other => panic!("expected unreachable, got {other:?}"),
        }
    }
}
