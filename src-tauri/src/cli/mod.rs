//! Command-line modes, intercepted at the very top of `run()` before the
//! Tauri builder. The same commands exist in both releases; what runs them
//! differs:
//!
//! - **Desktop release** (`service` feature, `remote.rs`): a thin client of
//!   the service over the local API. It starts the service if none answers,
//!   asks, and waits for the user's answer in the window.
//!   `roadie --serve [--data-dir <dir>]` is the service itself.
//! - **CLI release** (no features, `local.rs`): everything in this process.
//!   No service, no API, no owner channel. A request is shown in a native
//!   dialog from this process (a terminal prompt on a machine without a
//!   screen) and answered before the command returns.
//!
//! Client commands print one JSON document to stdout. Exit codes: 0 ok,
//! 1 the action failed, 2 the user declined, 3 usage or connection error.
//! There is deliberately no `--yes`.
//!
//! A tool argument is a recipe name or the path of a recipe file the
//! calling app ships (`Target`). With a file, `install` and `upgrade` send
//! the recipe along: the same as the trusted one changes nothing, a new or
//! changed one rides in the request for the user to review and trust. The
//! other commands act on the stored recipe and report `recipeDiffers`.

#[cfg(not(feature = "service"))]
mod local;
#[cfg(feature = "service")]
mod remote;
#[cfg(not(feature = "service"))]
pub use local::run_client;
#[cfg(feature = "service")]
pub use remote::run_client;

use serde_json::{Map, Value};
use std::io::IsTerminal;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Serve { data_dir: Option<PathBuf> },
    LegacyStartTool { data_dir: Option<PathBuf> },
    /// `roadie tool …` etc.; `argv` is everything after the program name
    /// with `--data-dir <dir>` removed.
    Client { data_dir: Option<PathBuf>, argv: Vec<String> },
}

#[cfg(feature = "service")]
pub const USAGE: &str = const_format_usage::DESKTOP;
#[cfg(not(feature = "service"))]
pub const USAGE: &str = const_format_usage::CLI;

/// The two usage texts, assembled at compile time without a macro crate.
mod const_format_usage {
    macro_rules! usage {
        ($extra:literal, $options:literal) => {
            concat!(
                "usage:\n",
                "  roadie tool list\n",
                "  roadie tool status|check <tool>          (check: look for a newer release)\n",
                "  roadie tool start|stop|restart <tool>\n",
                "  roadie tool install <tool> [--set key=value]... [--consumer <id>] [--wait]\n",
                "                              (--consumer: one approval installs and grants that app its key)\n",
                "  roadie tool upgrade <tool> [--wait]      (alias: update)\n",
                "  roadie tool uninstall <tool> [--keep-data] [--wait]\n",
                "  roadie recipe validate <file>           (offline; errors name a JSON pointer)\n",
                "  roadie recipe dryrun <tool>             (resolve and render; downloads and writes nothing)\n",
                $extra,
                "  <tool> is a recipe name or the path of a recipe file (.json) your app ships. install and\n",
                "  upgrade send the file along: a new or changed recipe is shown to the user to review and trust.\n",
                $options,
                "exit codes: 0 ok · 1 failed · 2 declined by the user · 3 usage or connection error"
            )
        };
    }
    #[allow(dead_code)]
    pub const DESKTOP: &str = usage!(
        "  roadie request list\n  roadie request <id> [--wait]\n  roadie request <id> answer    (show it again; prompts here only on a machine without a screen)\n  roadie service status|stop\n  roadie --serve\n",
        "options: --data-dir <dir>   (default: Roadie's app data dir)\n         --as <app name>    what the prompt says asked (default: roadie CLI)\n"
    );
    #[allow(dead_code)]
    pub const CLI: &str = usage!(
        "  roadie tool connection <tool> --consumer <id>   (its URL and that app's key; asks the user once)\n  roadie tool autostart <tool> on|off\n  roadie tool logs <tool> [--lines n]\n  roadie maintain                  (start the 'start at login' daemons, daily updates; run at login)\n",
        "options: --data-dir <dir>   (default: this CLI's own data dir; an app bundling Roadie passes its own)\n         --as <app name>    what the prompt says asked (default: roadie CLI)\n"
    );
}

pub fn maybe_run(args: &[String]) -> Option<i32> {
    let mode = parse(args)?;
    let filter = if matches!(mode, Mode::Client { .. }) { "warn" } else { "info" };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(filter)).init();
    Some(match mode {
        #[cfg(feature = "service")]
        Mode::Serve { data_dir } => crate::service::run(data_dir.unwrap_or_else(crate::paths::default_data_root)),
        #[cfg(not(feature = "service"))]
        Mode::Serve { .. } => {
            eprintln!("roadie: this is the command-line release; it has no service (--serve is the desktop release's)");
            3
        }
        #[cfg(feature = "service")]
        Mode::LegacyStartTool { data_dir } => {
            let root = data_dir.unwrap_or_else(crate::paths::default_data_root);
            log::info!("legacy launcher item: making sure the service runs instead");
            match crate::service::ensure_running(&root) {
                Ok(_) => 0,
                Err(e) => {
                    log::error!("{e}");
                    1
                }
            }
        }
        #[cfg(not(feature = "service"))]
        Mode::LegacyStartTool { data_dir } => print_result(run_client(&data_dir.unwrap_or_else(crate::paths::default_data_root), &["maintain".to_string()])),
        Mode::Client { data_dir, argv } => print_result(run_client(&data_dir.unwrap_or_else(crate::paths::default_data_root), &argv)),
    })
}

fn print_result(r: Result<(i32, Value), String>) -> i32 {
    match r {
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
    } else if matches!(command, Some("tool" | "request" | "service" | "recipe" | "maintain" | "help" | "--help" | "-h")) {
        Some(Mode::Client { data_dir: dir, argv: rest })
    } else {
        None
    }
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

/// Add `recipeDiffers` to an object result when a file was given.
pub(crate) fn annotate(mut out: Value, matches: Option<bool>) -> Value {
    if let (Some(m), Some(o)) = (matches, out.as_object_mut()) {
        o.insert("recipeDiffers".into(), Value::Bool(!m));
    }
    out
}

/// Lift `--as <name>` out, so positional parsing sees the command first.
pub(crate) fn split_as(argv_in: &[String]) -> (Option<String>, Vec<String>) {
    let as_name = value_of(argv_in, "--as");
    let mut argv = Vec::with_capacity(argv_in.len());
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
    (as_name, argv)
}

/// `roadie recipe validate <file>`: offline, in either release.
pub(crate) fn validate_file(file: Option<&String>) -> Result<(i32, Value), String> {
    let file = file.ok_or_else(|| format!("recipe validate needs a file\n{USAGE}"))?;
    let t = target(file)?;
    let v = t.recipe.ok_or_else(|| format!("{file} is not a recipe file (.json)"))?;
    Ok(match crate::recipe::from_value(v) {
        Ok(r) => (0, serde_json::json!({ "ok": true, "name": r.name, "supported": r.supported_on(&crate::recipe::Platform::current()), "errors": [] })),
        Err(errors) => (1, serde_json::json!({ "ok": false, "errors": errors })),
    })
}

pub(crate) fn has(argv: &[String], flag: &str) -> bool {
    argv.iter().any(|a| a == flag)
}

pub(crate) fn value_of(argv: &[String], flag: &str) -> Option<String> {
    argv.iter().position(|a| a == flag).and_then(|i| argv.get(i + 1)).cloned()
}

/// Stdin and stderr are both a terminal: a person may be typing.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

#[cfg_attr(test, allow(dead_code))]
pub(crate) enum TtyAnswer {
    Approve,
    Decline,
    /// No answer (end of input), or a blocked prompt not declined: the
    /// note says which.
    Leave(String),
}

/// Print a prompt on the terminal and read y/N. Only ever reached where
/// nothing can be shown on a screen (`prompt::Surface::Terminal`).
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn ask_tty(p: &crate::prompt::Prompt) -> Result<TtyAnswer, String> {
    use std::io::{BufRead, Write};
    eprintln!("\n{}\n", p.headline);
    for l in &p.lines {
        eprintln!("  {l}");
    }
    let question = if p.approve.is_some() {
        p.question.clone()
    } else {
        eprintln!("\n  {}", p.blocked.clone().unwrap_or_default());
        "Decline it?".to_string()
    };
    eprint!("\n{question} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    let read = std::io::stdin().lock().read_line(&mut line).map_err(|e| format!("read the answer: {e}"))?;
    if read == 0 {
        eprintln!();
        return Ok(TtyAnswer::Leave("no answer (end of input)".into()));
    }
    let yes = matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes");
    Ok(match (p.approve.is_some(), yes) {
        (true, true) => TtyAnswer::Approve,
        (true, false) | (false, true) => TtyAnswer::Decline,
        (false, false) => TtyAnswer::Leave("left unanswered; it cannot be approved from here".into()),
    })
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
