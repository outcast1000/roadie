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
//! roadie tool status slskd
//! roadie tool start|stop|restart slskd
//! roadie tool install slskd [--set soulseekUsername=bj] [--set startNow=false] [--consumer <id>] [--wait]
//! roadie tool uninstall slskd [--keep-data] [--wait]
//! roadie request <id> [--wait]
//! roadie service status|stop
//! ```

use crate::{api, client, service};
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
  roadie tool status <name>
  roadie tool start|stop|restart <name>
  roadie tool install <name> [--set key=value]... [--consumer <id>] [--wait]
                              (--consumer: one approval installs and grants that app its key)
  roadie tool uninstall <name> [--keep-data] [--wait]
  roadie request <id> [--wait]
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
    } else if matches!(command, Some("tool" | "request" | "service" | "help" | "--help" | "-h")) {
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

    match (cmd, argv.get(1).map(String::as_str), argv.get(2).map(String::as_str)) {
        ("tool", Some("list"), _) => Ok((0, call("GET", "/v1/tools", None)?)),
        ("tool", Some("status"), Some(name)) => Ok((0, call("GET", &format!("/v1/tools/{}", enc(name)), None)?)),
        ("tool", Some(action @ ("start" | "stop" | "restart")), Some(name)) => {
            match call("POST", &format!("/v1/tools/{}/{action}", enc(name)), None) {
                Ok(v) => Ok((0, v)),
                Err(e) => Ok((1, json!({ "error": e }))),
            }
        }
        ("tool", Some("install"), Some(name)) => {
            let config = parse_sets(argv)?;
            let created = call("POST", &format!("/v1/tools/{}/install", enc(name)), Some(json!({ "config": config, "consumer": value_of(argv, "--consumer") })))?;
            finish_request(created, wait, &call)
        }
        ("tool", Some("uninstall"), Some(name)) => {
            let keep = has(argv, "--keep-data");
            let created = call("DELETE", &format!("/v1/tools/{}?keepData={keep}", enc(name)), None)?;
            finish_request(created, wait, &call)
        }
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

/// A 202 came back. Without `--wait`, print it. With `--wait`, poll the
/// request until the user has decided and the action finished, mirroring
/// progress to stderr, and map the outcome to an exit code.
fn finish_request(created: Value, wait: bool, call: &dyn Fn(&str, &str, Option<Value>) -> Result<Value, String>) -> Result<(i32, Value), String> {
    let Some(id) = created.get("requestId").and_then(|i| i.as_str()).map(str::to_string) else {
        return Ok((0, created));
    };
    if !wait {
        return Ok((0, created));
    }
    eprintln!("roadie: waiting for the user to answer in Roadie's window (request {id})…");
    let mut last_line = String::new();
    loop {
        let r = call("GET", &format!("/v1/requests/{}", enc(&id)), None)?;
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
