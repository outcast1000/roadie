//! The desktop release's CLI: a client of the service over the local API.
//! It starts the service if none answers and never holds an owner token
//! except as a `terminal` on a machine with no screen.

use super::*;
use crate::{api, client, owner};
use serde_json::json;
use std::time::Duration;

fn enc(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c.to_string() } else { format!("%{:02X}", c as u32) }).collect()
}

type Call<'a> = &'a dyn Fn(&str, &str, Option<Value>) -> Result<Value, String>;

/// With a recipe file, say whether Roadie's trusted recipe is that file:
/// `Some(false)` differs (or is only a draft), `None` without a file.
fn recipe_matches(t: &Target, call: Call) -> Result<Option<bool>, String> {
    let (Some(v), Some(file)) = (&t.recipe, &t.file) else { return Ok(None) };
    let stored = call("GET", &format!("/v1/recipes/{}", enc(&t.name)), None)
        .map_err(|_| format!("Roadie has no recipe named {} yet; install it with `roadie tool install {file}`", t.name))?;
    let trusted = stored.get("origin").and_then(|o| o.as_str()) != Some("draft");
    let same = match (crate::recipe::from_value(v.clone()), stored.get("recipe").cloned().map(crate::recipe::from_value)) {
        (Ok(a), Some(Ok(b))) => crate::recipe::store::same(&a, &b),
        _ => false,
    };
    if !(same && trusted) {
        eprintln!("roadie: {file} differs from the recipe Roadie trusts for {}; `roadie tool upgrade {file}` proposes it to the user", t.name);
    }
    Ok(Some(same && trusted))
}


/// Run one client command. Returns the exit code and the JSON to print.
pub fn run_client(root: &std::path::Path, argv_in: &[String]) -> Result<(i32, Value), String> {
    let (as_name, argv) = split_as(argv_in);
    let argv = &argv[..];
    let cmd = argv.first().map(String::as_str).unwrap_or("help");
    if matches!(cmd, "version" | "--version") {
        return Ok((0, version_info()));
    }
    if matches!(cmd, "help" | "--help" | "-h") {
        return Ok((0, json!({ "usage": USAGE })));
    }
    // `service status` must not start a service just to report on it.
    if cmd == "service" && argv.get(1).map(String::as_str) == Some("status") {
        return Ok(match api::probe(root) {
            Some(h) => (0, json!({ "running": true, "health": h, "loginItem": crate::tools::autostart::service_enabled(root) })),
            None => (0, json!({ "running": false, "loginItem": crate::tools::autostart::service_enabled(root) })),
        });
    }
    // Validating a recipe file needs no service: it is the same validator.
    if cmd == "recipe" && argv.get(1).map(String::as_str) == Some("validate") {
        return validate_file(argv.get(2));
    }
    if cmd == "service" && argv.get(1).map(String::as_str) == Some("stop") {
        return Ok(match api::probe(root) {
            Some(_) => {
                api::bearer_post(root, "/v1/shutdown")?;
                (0, json!({ "stopping": true }))
            }
            None => (0, json!({ "stopping": false, "note": "no service was running" })),
        });
    }

    client::set_client_name(&as_name.unwrap_or_else(|| "roadie CLI".into()));
    client::connect_with(root, false)?;
    let call = |m: &str, p: &str, b: Option<Value>| client::call(m, p, b, false);
    let wait = has(argv, "--wait");

    let tool_arg = argv.get(2).map(|a| target(a)).transpose()?;
    match (cmd, argv.get(1).map(String::as_str), tool_arg) {
        ("tool", Some("list"), _) => Ok((0, call("GET", "/v1/tools", None)?)),
        ("tool", Some("status"), Some(t)) => {
            let m = recipe_matches(&t, &call)?;
            Ok((0, annotate(call("GET", &format!("/v1/tools/{}", enc(&t.name)), None)?, m)))
        }
        ("tool", Some(action @ ("start" | "stop" | "restart" | "check")), Some(t)) => {
            let m = recipe_matches(&t, &call)?;
            let route = if action == "check" { "check-updates" } else { action };
            match call("POST", &format!("/v1/tools/{}/{route}", enc(&t.name)), None) {
                Ok(v) => Ok((0, annotate(v, m))),
                Err(e) => Ok((1, json!({ "error": e }))),
            }
        }
        ("tool", Some("install"), Some(t)) => {
            let config = parse_sets(argv)?;
            let mut body = json!({ "config": config, "consumer": value_of(argv, "--consumer") });
            if let Some(r) = &t.recipe {
                body["recipe"] = r.clone();
            }
            let created = call("POST", &format!("/v1/tools/{}/install", enc(&t.name)), Some(body))?;
            wait_or_answer(root, created, wait, &call)
        }
        ("tool", Some("upgrade" | "update"), Some(t)) => {
            let body = t.recipe.as_ref().map(|r| json!({ "recipe": r }));
            match call("POST", &format!("/v1/tools/{}/update", enc(&t.name)), body) {
                // A changed recipe: the user reviews it, approving updates.
                Ok(v) if v.get("requestId").is_some() => wait_or_answer(root, v, wait, &call),
                Ok(v) => Ok((0, v)),
                Err(e) => Ok((1, json!({ "error": e }))),
            }
        }
        ("tool", Some("uninstall"), Some(t)) => {
            let keep = has(argv, "--keep-data");
            let created = call("DELETE", &format!("/v1/tools/{}?keepData={keep}", enc(&t.name)), None)?;
            wait_or_answer(root, created, wait, &call)
        }
        ("recipe", Some("dryrun"), Some(t)) => {
            let body = t.recipe.as_ref().map(|r| json!({ "recipe": r }));
            match call("POST", &format!("/v1/recipes/{}/dryrun", enc(&t.name)), body) {
                Ok(v) => Ok((0, v)),
                Err(e) => Ok((1, json!({ "error": e }))),
            }
        }
        ("request", Some("list"), _) => Ok((0, call("GET", "/v1/requests", None)?)),
        ("request", Some(id), Some(Target { name, .. })) if name == "answer" => answer(root, id, true, &call),
        ("request", Some(id), _) => {
            let r = call("GET", &format!("/v1/requests/{}", enc(id)), None)?;
            if !wait {
                return Ok((0, r));
            }
            finish_request(json!({ "requestId": id }), true, &call)
        }
        _ => Err(format!("unknown command `{}`\n{USAGE}", argv.join(" "))),
    }
}


/// After `install`/`uninstall --wait`: on a terminal, try to answer here
/// (granted only on a machine without a screen); otherwise wait for the
/// answer on the screen.
fn wait_or_answer(root: &std::path::Path, created: Value, wait: bool, call: Call) -> Result<(i32, Value), String> {
    match created.get("requestId").and_then(|i| i.as_str()) {
        Some(id) if wait && interactive() => answer(root, id, false, call),
        _ => finish_request(created, wait, call),
    }
}

/// `roadie request <id> answer`. With a screen, the owner channel refuses a
/// terminal: show the request there again (`reshow`) and wait. Without one,
/// print the prompt, read y/N, and decide over the owner channel.
fn answer(root: &std::path::Path, id: &str, reshow: bool, call: Call) -> Result<(i32, Value), String> {
    let described = call("GET", &format!("/v1/requests/{}/prompt", enc(id)), None)?;
    if described.get("status").and_then(|s| s.as_str()) != Some("pending") {
        return finish_request(json!({ "requestId": id }), true, call);
    }
    let surface = described.get("surface").and_then(|s| s.as_str()).unwrap_or("");
    if surface != "terminal" {
        if reshow {
            call("POST", &format!("/v1/requests/{}/prompt", enc(id)), None)?;
        }
        eprintln!("roadie: this computer has a screen, so Roadie asks there: answer the {surface} it shows (request {id}).");
        return finish_request(json!({ "requestId": id }), true, call);
    }
    if !interactive() {
        return Err(format!("request {id} can only be answered by a person at a terminal; run `roadie request {id} answer` yourself (there is no --yes)"));
    }
    let token = owner::connect_as(root, owner::Peer::Terminal)?;
    client::set_owner_token(token);
    let p: crate::prompt::Prompt = serde_json::from_value(described.get("prompt").cloned().unwrap_or(Value::Null)).map_err(|e| format!("the service sent no prompt: {e}"))?;
    let approve = match ask_tty(&p)? {
        TtyAnswer::Approve => true,
        TtyAnswer::Decline => false,
        TtyAnswer::Leave(note) => return Ok((1, json!({ "requestId": id, "status": "pending", "note": note }))),
    };
    // The decision runs the install inside the call; poll alongside it so
    // progress shows up here.
    let id_owned = id.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(client::call("POST", &format!("/v1/owner/requests/{}/decide", enc(&id_owned)), Some(json!({ "approve": approve })), true));
    });
    poll_request(id, call, Some(&rx))
}

/// A 202 came back. Without `--wait`, print it. With `--wait`, poll the
/// request until the user has decided and the action finished, mirroring
/// progress to stderr, and map the outcome to an exit code.
fn finish_request(created: Value, wait: bool, call: Call) -> Result<(i32, Value), String> {
    let Some(id) = created.get("requestId").and_then(|i| i.as_str()).map(str::to_string) else {
        return Ok((0, created));
    };
    if !wait {
        return Ok((0, created));
    }
    eprintln!("roadie: waiting for the user to answer (request {id}; `roadie request {id} answer` shows it again)…");
    poll_request(&id, call, None)
}

/// Poll until the request is done, failed or declined. `decision` is the
/// terminal's own decide call: if it fails before the request moves, stop.
fn poll_request(id: &str, call: Call, decision: Option<&std::sync::mpsc::Receiver<Result<Value, String>>>) -> Result<(i32, Value), String> {
    let mut last_line = String::new();
    loop {
        if let Some(Ok(Err(e))) = decision.map(|rx| rx.try_recv()) {
            return Ok((1, json!({ "requestId": id, "error": e })));
        }
        let r = call("GET", &format!("/v1/requests/{}", enc(id)), None)?;
        let status = r.get("status").and_then(|s| s.as_str()).unwrap_or("");
        if let Some(p) = r.get("progress").filter(|p| !p.is_null()) {
            let line = match (p.get("phase").and_then(|s| s.as_str()), p.get("downloaded").and_then(|d| d.as_u64()), p.get("total").and_then(|t| t.as_u64())) {
                (Some("downloading"), Some(d), Some(t)) if t > 0 => format!("downloading {}%", d * 100 / t),
                (Some(phase), _, _) => phase.to_string(),
                _ => String::new(),
            };
            if line != last_line {
                eprintln!("roadie: {line}");
                last_line = line;
            }
        }
        match status {
            "done" => return Ok((0, r)),
            "failed" => return Ok((1, r)),
            "declined" => return Ok((2, r)),
            _ => std::thread::sleep(Duration::from_millis(500)),
        }
    }
}

