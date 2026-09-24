//! Command-line modes, intercepted at the very top of `run()` before the
//! Tauri builder:
//!
//! - `roadie --serve [--data-dir <dir>]` — the background service
//!   (`service.rs`). Login items and the window start it this way.
//! - `roadie tool|request|service … [--data-dir <dir>]` — a thin client for
//!   scripts and other apps (below). It starts the service if none answers
//!   and never holds an owner token: it can ask, never approve.
//! - `roadie --start-tool <name> --data-dir <dir>` — what login items from
//!   an older Roadie run. It now just makes sure the service is up.
//!
//! Client commands print one JSON document to stdout. Exit codes: 0 ok,
//! 1 the action failed, 2 the user declined, 3 usage or connection error.
//!
//! ```text
//! roadie tool list
//! roadie tool status|check slskd
//! roadie tool start|stop|restart slskd
//! roadie tool install slskd [--set soulseekUsername=bj] [--set startNow=false] [--consumer <id>] [--wait]
//! roadie tool upgrade slskd [--wait]
//! roadie tool uninstall slskd [--keep-data] [--wait]
//! roadie recipe validate ./slskd.json
//! roadie recipe dryrun ./slskd.json
//! roadie request list
//! roadie request <id> [--wait]
//! roadie request <id> answer
//! roadie service status|stop
//! ```
//!
//! Answering: the service shows a request where it can (`prompt.rs`): the
//! window, or a native dialog in the build without one. Only when the
//! machine has no screen does `request <id> answer` (and `install --wait`
//! on a TTY) prompt in the terminal; there is deliberately no `--yes`.
//!
//! A tool argument is a recipe name or the path of a recipe file the
//! calling app ships (`Target`). With a file, `install` and `upgrade` send
//! the recipe along: the same as the trusted one changes nothing, a new or
//! changed one rides in the request for the user to review and trust. The
//! other commands act on the stored recipe and report `recipeDiffers`.

use crate::{api, client, owner, service};
use std::io::{BufRead, IsTerminal, Write};
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Serve { data_dir: Option<PathBuf> },
    LegacyStartTool { data_dir: Option<PathBuf> },
    /// `roadie tool …` etc.; `argv` is everything after the program name
    /// with `--data-dir <dir>` removed.
    Client { data_dir: Option<PathBuf>, argv: Vec<String> },
}

pub const USAGE: &str = "usage:
  roadie tool list
  roadie tool status|check <tool>          (check: look for a newer release)
  roadie tool start|stop|restart <tool>
  roadie tool install <tool> [--set key=value]... [--consumer <id>] [--wait]
                              (--consumer: one approval installs and grants that app its key)
  roadie tool upgrade <tool> [--wait]      (alias: update)
  roadie tool uninstall <tool> [--keep-data] [--wait]
  roadie recipe validate <file>           (offline; errors name a JSON pointer)
  roadie recipe dryrun <tool>             (resolve and render; downloads and writes nothing)
  <tool> is a recipe name or the path of a recipe file (.json) your app ships. install and
  upgrade send the file along: a new or changed recipe is shown to the user to review and trust.
  roadie request list
  roadie request <id> [--wait]
  roadie request <id> answer    (show it again; prompts here only on a machine without a screen)
  roadie service status|stop
  roadie --serve
options: --data-dir <dir>   (default: Roadie's app data dir)
         --as <app name>    what the prompt says asked (default: roadie CLI)
exit codes: 0 ok · 1 failed · 2 declined by the user · 3 usage or connection error";

pub fn maybe_run(args: &[String]) -> Option<i32> {
    let mode = parse(args)?;
    let filter = if matches!(mode, Mode::Client { .. }) { "warn" } else { "info" };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(filter)).init();
    Some(match mode {
        Mode::Serve { data_dir } => service::run(data_dir.unwrap_or_else(crate::paths::default_data_root)),
        Mode::LegacyStartTool { data_dir } => {
            let root = data_dir.unwrap_or_else(crate::paths::default_data_root);
            log::info!("legacy launcher item: making sure the service runs instead");
            match service::ensure_running(&root) {
                Ok(_) => 0,
                Err(e) => {
                    log::error!("{e}");
                    1
                }
            }
        }
        Mode::Client { data_dir, argv } => {
            let root = data_dir.unwrap_or_else(crate::paths::default_data_root);
            match run_client(&root, &argv) {
                Ok((code, out)) => {
                    println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
                    code
                }
                Err(e) => {
                    eprintln!("roadie: {e}");
                    3
                }
            }
        }
    })
}

/// `--data-dir <dir>` when present, for the window (which otherwise uses
/// Tauri's app data dir). The service passes it when it opens a window on a
/// non-default data dir.
pub fn data_dir_arg(args: &[String]) -> Option<PathBuf> {
    args.iter().position(|a| a == "--data-dir").and_then(|i| args.get(i + 1)).map(PathBuf::from)
}

pub fn parse(args: &[String]) -> Option<Mode> {
    let mut serve = false;
    let mut start_tool = false;
    let mut dir = None;
    let mut rest = Vec::new();
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--serve" => serve = true,
            "--start-tool" => {
                start_tool = true;
                let _ = it.next();
            }
            "--data-dir" => dir = it.next().map(PathBuf::from),
            "--as" => {
                rest.push(a.clone());
                if let Some(v) = it.next() {
                    rest.push(v.clone());
                }
            }
            other => rest.push(other.to_string()),
        }
    }
    // The command word is the first token that is not a flag or a flag's
    // value, so `roadie --as "My app" tool install …` works too.
    let command = rest
        .iter()
        .enumerate()
        .find(|(i, t)| !t.starts_with("--") && (*i == 0 || rest[i - 1] != "--as"))
        .map(|(_, t)| t.as_str())
        .or_else(|| rest.iter().find(|t| matches!(t.as_str(), "--help" | "-h")).map(String::as_str));
    if serve {
        Some(Mode::Serve { data_dir: dir })
    } else if start_tool {
        Some(Mode::LegacyStartTool { data_dir: dir })
    } else if matches!(command, Some("tool" | "request" | "service" | "recipe" | "help" | "--help" | "-h")) {
        Some(Mode::Client { data_dir: dir, argv: rest })
    } else {
        None
    }
}

fn enc(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c.to_string() } else { format!("%{:02X}", c as u32) }).collect()
}

/// Parse `--set key=value` into typed JSON: `true`/`false` and integers
/// become themselves, everything else a string.
pub fn parse_sets(argv: &[String]) -> Result<Map<String, Value>, String> {
    let mut out = Map::new();
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        if a == "--set" {
            let kv = it.next().ok_or("--set needs key=value")?;
            let (k, v) = kv.split_once('=').ok_or_else(|| format!("--set {kv}: expected key=value"))?;
            let value = match v {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                _ => v.parse::<i64>().map(Value::from).unwrap_or_else(|_| Value::String(v.to_string())),
            };
            out.insert(k.to_string(), value);
        }
    }
    Ok(out)
}

/// What a tool argument names: a recipe by name, or a recipe file the
/// calling app holds (anything ending in `.json` or containing a path
/// separator). The file's `/name` names the tool.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub name: String,
    pub recipe: Option<Value>,
    pub file: Option<String>,
}

pub fn target(arg: &str) -> Result<Target, String> {
    let is_file = arg.ends_with(".json") || arg.contains('/') || arg.contains('\\');
    if !is_file {
        return Ok(Target { name: arg.to_string(), recipe: None, file: None });
    }
    let text = std::fs::read_to_string(arg).map_err(|e| format!("read recipe {arg}: {e}"))?;
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("recipe {arg} is not JSON: {e}"))?;
    // A draft's `{submittedBy, recipe}` envelope is accepted too.
    let v = match v.get("recipe") {
        Some(inner) if inner.is_object() => inner.clone(),
        _ => v,
    };
    let name = v.get("name").and_then(|n| n.as_str()).filter(|n| !n.is_empty()).ok_or_else(|| format!("recipe {arg} has no /name"))?.to_string();
    Ok(Target { name, recipe: Some(v), file: Some(arg.to_string()) })
}

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

/// Add `recipeDiffers` to an object result when a file was given.
fn annotate(mut out: Value, matches: Option<bool>) -> Value {
    if let (Some(m), Some(o)) = (matches, out.as_object_mut()) {
        o.insert("recipeDiffers".into(), Value::Bool(!m));
    }
    out
}

fn has(argv: &[String], flag: &str) -> bool {
    argv.iter().any(|a| a == flag)
}

fn value_of(argv: &[String], flag: &str) -> Option<String> {
    argv.iter().position(|a| a == flag).and_then(|i| argv.get(i + 1)).cloned()
}

/// Run one client command. Returns the exit code and the JSON to print.
pub fn run_client(root: &std::path::Path, argv_in: &[String]) -> Result<(i32, Value), String> {
    // Lift `--as <name>` out so positional parsing below sees the command first.
    let as_name = value_of(argv_in, "--as");
    let mut argv: Vec<String> = Vec::with_capacity(argv_in.len());
    let mut skip = false;
    for a in argv_in {
        if skip {
            skip = false;
            continue;
        }
        if a == "--as" {
            skip = true;
            continue;
        }
        argv.push(a.clone());
    }
    let argv = &argv[..];
    let cmd = argv.first().map(String::as_str).unwrap_or("help");
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
        let file = argv.get(2).ok_or_else(|| format!("recipe validate needs a file\n{USAGE}"))?;
        let t = target(file)?;
        let v = t.recipe.ok_or_else(|| format!("{file} is not a recipe file (.json)"))?;
        return Ok(match crate::recipe::from_value(v) {
            Ok(r) => (0, json!({ "ok": true, "name": r.name, "supported": r.supported_on(&crate::recipe::Platform::current()), "errors": [] })),
            Err(errors) => (1, json!({ "ok": false, "errors": errors })),
        });
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

type Call<'a> = &'a dyn Fn(&str, &str, Option<Value>) -> Result<Value, String>;

/// Stdin and stderr are both a terminal: a person may be typing.
fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
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
    let p = described.get("prompt").cloned().unwrap_or(Value::Null);
    let s = |k: &str| p.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let lines: Vec<String> = p.get("lines").and_then(|l| l.as_array()).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
    eprintln!("\n{}\n", s("headline"));
    for l in &lines {
        eprintln!("  {l}");
    }
    let can_approve = p.get("approve").is_some_and(|a| a.is_string());
    let question = if can_approve {
        s("question")
    } else {
        eprintln!("\n  {}", s("blocked"));
        "Decline it?".to_string()
    };
    eprint!("\n{question} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    let read = std::io::stdin().lock().read_line(&mut line).map_err(|e| format!("read the answer: {e}"))?;
    if read == 0 {
        eprintln!();
        return Ok((3, json!({ "requestId": id, "status": "pending", "note": "no answer (end of input); the request is still pending" })));
    }
    let yes = matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes");
    if !can_approve && !yes {
        return Ok((1, json!({ "requestId": id, "status": "pending", "note": "left pending; it cannot be approved from here" })));
    }
    let approve = yes && can_approve;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modes_and_ignores_everything_else() {
        let serve = vec!["roadie".into(), "--serve".into(), "--data-dir".into(), "/x y".into()];
        assert_eq!(parse(&serve), Some(Mode::Serve { data_dir: Some(PathBuf::from("/x y")) }));
        assert_eq!(parse(&["roadie".into(), "--serve".into()]), Some(Mode::Serve { data_dir: None }));
        let legacy = vec!["roadie".into(), "--start-tool".into(), "slskd".into(), "--data-dir".into(), "/d".into()];
        assert_eq!(parse(&legacy), Some(Mode::LegacyStartTool { data_dir: Some(PathBuf::from("/d")) }));
        let cli = vec!["roadie".into(), "tool".into(), "install".into(), "slskd".into(), "--data-dir".into(), "/d".into(), "--wait".into()];
        assert_eq!(
            parse(&cli),
            Some(Mode::Client { data_dir: Some(PathBuf::from("/d")), argv: vec!["tool".into(), "install".into(), "slskd".into(), "--wait".into()] })
        );
        let named = vec!["roadie".into(), "--as".into(), "My Player".into(), "tool".into(), "list".into()];
        assert!(matches!(parse(&named), Some(Mode::Client { argv, .. }) if argv == vec!["--as", "My Player", "tool", "list"]));
        let unrelated = vec!["roadie".into(), "roadie://open/slskd".into()];
        assert_eq!(parse(&unrelated), None);
        assert_eq!(maybe_run(&unrelated), None);
        assert_eq!(parse(&["roadie".into()]), None, "no arguments is the window");
    }

    #[test]
    fn a_tool_argument_is_a_name_or_a_recipe_file_and_validate_needs_no_service() {
        assert_eq!(target("slskd").unwrap(), Target { name: "slskd".into(), recipe: None, file: None });
        let dir = std::env::temp_dir().join(format!("roadie-cli-target-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("ffmpeg.json");
        std::fs::write(&good, crate::recipe::BUILTIN.iter().find(|(n, _)| *n == "ffmpeg").unwrap().1).unwrap();
        let t = target(good.to_str().unwrap()).unwrap();
        assert_eq!(t.name, "ffmpeg");
        assert!(t.recipe.is_some());

        let wrapped = dir.join("wrapped.json");
        std::fs::write(&wrapped, r#"{"submittedBy":"x","recipe":{"name":"inner"}}"#).unwrap();
        assert_eq!(target(wrapped.to_str().unwrap()).unwrap().name, "inner", "a draft envelope is accepted");
        let nameless = dir.join("nameless.json");
        std::fs::write(&nameless, r#"{"kind":"cli"}"#).unwrap();
        assert!(target(nameless.to_str().unwrap()).unwrap_err().contains("/name"));
        assert!(target("./missing.json").unwrap_err().contains("missing.json"));

        let (code, out) = run_client(std::path::Path::new("/nonexistent"), &["recipe".into(), "validate".into(), good.to_string_lossy().into_owned()]).unwrap();
        assert_eq!((code, out["ok"].as_bool()), (0, Some(true)), "{out}");
        let (code, out) = run_client(std::path::Path::new("/nonexistent"), &["recipe".into(), "validate".into(), wrapped.to_string_lossy().into_owned()]).unwrap();
        assert_eq!(code, 1);
        assert!(out["errors"][0]["pointer"].is_string(), "errors carry pointers: {out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sets_are_typed_and_help_needs_no_service() {
        let sets = parse_sets(&["--set".into(), "soulseekUsername=bj".into(), "--set".into(), "startNow=false".into(), "--set".into(), "port=5030".into()]).unwrap();
        assert_eq!(sets["soulseekUsername"], "bj");
        assert_eq!(sets["startNow"], false);
        assert_eq!(sets["port"], 5030);
        assert!(parse_sets(&["--set".into(), "novalue".into()]).is_err());
        let (code, out) = run_client(std::path::Path::new("/nonexistent"), &["help".into()]).unwrap();
        assert_eq!(code, 0);
        assert!(out["usage"].as_str().unwrap().contains("roadie tool install"));
    }
}
