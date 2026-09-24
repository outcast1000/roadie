//! End-to-end: the whole service in this process, driven over HTTP by two
//! actors — a client (Viboplr, bearer token) and the user (owner token from
//! the credentialed socket, which this process gets because it *is* the
//! binary the service runs). Real download of slskd, real process, real
//! slskd API. Ignored by default:
//!
//! ```bash
//! cd src-tauri && cargo test --lib e2e -- --ignored --nocapture
//! ```
//!
//! It uses a temp data dir, so it never touches the real installation, and
//! it binds the next free API port when a real service holds 47630. It
//! registers no login item and opens no window (an owner is connected
//! before any request is created, so `open_window_if_needed` stays quiet).

#[cfg(test)]
mod tests {
    use crate::{api, owner, paths, recipe};
    use serde_json::{json, Value};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    struct Roadie {
        root: PathBuf,
        port: u16,
        token: String,
        owner_token: String,
        http: reqwest::blocking::Client,
    }

    impl Roadie {
        fn start() -> Self {
            let root = std::env::temp_dir().join(format!("roadie-e2e-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            paths::init(root.clone());
            recipe::store::load_all();
            let r2 = root.clone();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(async {
                    api::start(&r2, "e2e".into(), "e2e-build".into()).expect("api starts");
                    std::future::pending::<()>().await;
                });
            });
            let disc = wait_for(Duration::from_secs(10), || {
                std::fs::read_to_string(api::discovery_path(&root)).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok())
            })
            .expect("discovery file appears");
            let port = disc["port"].as_u64().unwrap() as u16;
            let token = disc["token"].as_str().unwrap().to_string();
            // The user's side: the owner channel, from this very binary.
            owner::serve(&root).unwrap();
            let owner_token = owner::connect(&root).expect("same-binary peer gets an owner token");
            wait_for(Duration::from_secs(5), || owner::is_owner(&owner_token).then_some(())).unwrap();
            let http = reqwest::blocking::Client::builder().timeout(Duration::from_secs(15 * 60)).build().unwrap();
            Roadie { root, port, token, owner_token, http }
        }

        fn url(&self, path: &str) -> String {
            format!("http://127.0.0.1:{}{}", self.port, path)
        }

        /// Viboplr: public or bearer, identified by name.
        fn client(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
            let mut req = self.http.request(method.parse().unwrap(), self.url(path)).bearer_auth(&self.token).header("X-Roadie-Client", "Viboplr");
            if let Some(b) = body {
                req = req.json(&b);
            }
            let resp = req.send().unwrap();
            let status = resp.status().as_u16();
            let text = resp.text().unwrap_or_default();
            (status, if text.is_empty() { Value::Null } else { serde_json::from_str(&text).unwrap_or(Value::String(text)) })
        }

        fn public(&self, path: &str) -> (u16, Value) {
            let resp = self.http.get(self.url(path)).send().unwrap();
            let status = resp.status().as_u16();
            (status, resp.json().unwrap_or(Value::Null))
        }

        /// The user: a click in the window, relayed over the owner route.
        fn user_decides(&self, id: &str, approve: bool) -> Value {
            let resp = self
                .http
                .post(self.url(&format!("/v1/owner/requests/{id}/decide")))
                .header(owner::HEADER, &self.owner_token)
                .json(&json!({ "approve": approve }))
                .send()
                .unwrap();
            assert_eq!(resp.status().as_u16(), 200, "decide answers 200");
            resp.json().unwrap()
        }

        fn tool(&self) -> Value {
            self.public("/v1/tools/slskd").1
        }

        fn wait_healthy(&self, want: bool) -> Value {
            wait_for(Duration::from_secs(90), || {
                let t = self.tool();
                (t["healthy"].as_bool() == Some(want) && t["running"].as_bool() == Some(want)).then_some(t)
            })
            .unwrap_or_else(|| panic!("slskd never reached healthy={want}: {}", self.tool()))
        }
    }

    fn wait_for<T>(timeout: Duration, mut f: impl FnMut() -> Option<T>) -> Option<T> {
        let start = Instant::now();
        loop {
            if let Some(v) = f() {
                return Some(v);
            }
            if start.elapsed() > timeout {
                return None;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// Viboplr talks to slskd itself with the key Roadie gave it.
    fn hello(http: &reqwest::blocking::Client, url: &str, key: &str) -> Result<Value, String> {
        let resp = http
            .get(format!("{url}/api/v0/application"))
            .header("X-API-Key", key)
            .timeout(Duration::from_secs(5))
            .send()
            .map_err(|e| recipe::httpsteps::err_chain(&e))?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }
        resp.json().map_err(|e| e.to_string())
    }

    fn versions_dir(root: &Path) -> PathBuf {
        root.join("tools").join("slskd").join("versions")
    }

    #[test]
    #[ignore = "real download and process; run with --ignored"]
    fn viboplr_installs_connects_restarts_and_removes_slskd() {
        let r = Roadie::start();
        eprintln!("service on port {} with data in {}", r.port, r.root.display());

        // 0. Health, and nothing installed.
        let (st, health) = r.public("/v1/health");
        assert_eq!(st, 200);
        assert_eq!(health["role"], "service");
        assert_eq!(health["windowConnected"], true, "the owner (our 'window') is connected");
        assert_eq!(r.tool()["installed"], false);

        // 1. The bearer token alone cannot approve anything.
        let resp = r.http.post(r.url("/v1/owner/recipes/slskd/trust")).bearer_auth(&r.token).send().unwrap();
        assert_eq!(resp.status().as_u16(), 403, "owner routes refuse a bearer token");

        // 2. Viboplr asks to install, with its decisions and itself as the consumer.
        let (st, created) = r.client(
            "POST",
            "/v1/tools/slskd/install",
            Some(json!({ "config": { "soulseekUsername": "e2e", "startNow": true, "autostart": false }, "consumer": "viboplr" })),
        );
        assert_eq!(st, 202, "{created}");
        let id = created["requestId"].as_str().unwrap().to_string();
        assert_eq!(created["status"], "pending");
        assert!(created["decisions"].as_array().unwrap().iter().any(|d| d["key"] == "soulseekPassword" && d["settled"] == false), "{created}");
        let (_, pending) = r.client("GET", &format!("/v1/requests/{id}"), None);
        assert_eq!(pending["consumer"], "viboplr");
        assert_eq!(pending["requestedBy"], "Viboplr");
        assert_eq!(r.tool()["installed"], false, "nothing installs before the click");

        // 3. The user clicks Install. One click: install + start + grant.
        let decided = r.user_decides(&id, true);
        assert_eq!(decided["status"], "done", "{decided}");
        let (_, after) = r.client("GET", &format!("/v1/requests/{id}"), None);
        assert_eq!(after["status"], "done");
        let t = r.wait_healthy(true);
        assert_eq!(t["installed"], true);
        assert!(t["version"].as_str().is_some_and(|v| !v.is_empty()));
        assert_eq!(t["autostart"], false);
        assert!(t["approvedConsumers"].as_array().unwrap().iter().any(|c| c == "viboplr"), "{t}");
        assert!(versions_dir(&r.root).is_dir());
        assert_eq!(t.get("pid"), None, "public status never carries the pid");

        // 4. The key was granted by the same click; hello slskd with it.
        let (st, conn) = r.public("/v1/tools/slskd/connection?consumer=viboplr");
        assert_eq!(st, 200, "{conn}");
        let url = conn["url"].as_str().unwrap().to_string();
        let key = conn["apiKey"].as_str().unwrap().to_string();
        assert!(key.len() >= 16);
        let app = wait_for(Duration::from_secs(30), || hello(&r.http, &url, &key).ok()).expect("slskd answers its own API with Viboplr's key");
        assert!(app["version"]["current"].as_str().is_some(), "{app}");
        assert!(hello(&r.http, &url, "wrong-key").is_err(), "a wrong key is refused by slskd");

        // 5. Stop: Roadie says stopped, slskd stops answering.
        let (st, stopped) = r.client("POST", "/v1/tools/slskd/stop", None);
        assert_eq!(st, 200, "{stopped}");
        r.wait_healthy(false);
        assert!(hello(&r.http, &url, &key).is_err());

        // 6. Start again: same key still works (config re-rendered from the grant list).
        let (st, started) = r.client("POST", "/v1/tools/slskd/start", None);
        assert_eq!(st, 200, "{started}");
        r.wait_healthy(true);
        wait_for(Duration::from_secs(30), || hello(&r.http, &url, &key).ok()).expect("hello after restart");

        // 7. Stop, then ask to remove; nothing happens before the click.
        r.client("POST", "/v1/tools/slskd/stop", None);
        r.wait_healthy(false);
        let (st, created) = r.client("DELETE", "/v1/tools/slskd?keepData=false", None);
        assert_eq!(st, 202, "{created}");
        let rid = created["requestId"].as_str().unwrap().to_string();
        assert_eq!(r.tool()["installed"], true, "still installed until the user clicks");

        // 8. A decline leaves everything in place; a second ask is a new request.
        let declined = r.user_decides(&rid, false);
        assert_eq!(declined["status"], "declined");
        assert_eq!(r.tool()["installed"], true);
        let (_, created) = r.client("DELETE", "/v1/tools/slskd?keepData=false", None);
        let rid2 = created["requestId"].as_str().unwrap().to_string();
        assert_ne!(rid, rid2);

        // 9. The user clicks Remove: files, process and Viboplr's grant are gone.
        let removed = r.user_decides(&rid2, true);
        assert_eq!(removed["status"], "done", "{removed}");
        let t = r.tool();
        assert_eq!(t["installed"], false);
        assert_eq!(t["running"], false);
        assert!(!versions_dir(&r.root).exists(), "versions dir removed");
        assert!(hello(&r.http, &url, &key).is_err(), "slskd is not running");
        let (st, conn) = r.public("/v1/tools/slskd/connection?consumer=viboplr");
        assert_eq!(st, 403, "{conn}");
        assert_eq!(conn["reason"], "consent-required", "the grant did not survive a full removal");
        let (_, consumers) = r.client("GET", "/v1/consumers", None);
        let vib = consumers.as_array().unwrap().iter().find(|c| c["id"] == "viboplr").unwrap();
        assert!(vib["tools"].as_array().unwrap().is_empty(), "{vib}");

        let _ = std::fs::remove_dir_all(&r.root);
    }
}
