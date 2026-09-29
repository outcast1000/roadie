//! The CLI release: every command in this process. There is no service, no
//! API and no owner channel. An install, upgrade, uninstall or connection
//! ask becomes a request in this process's queue. It is shown to the user
//! in a native dialog from this process (a terminal prompt where there is no
//! screen), and decided before the command returns: `actions::decide` runs
//! here, with progress on stderr. Asks are checked by `intake`, exactly as
//! the desktop release's API checks them.
//!
//! State lives in the data dir (`--data-dir`, or the CLI's own default), so
//! one run sees what the last one did. Changes to a tool are serialized
//! across processes by the tool lock (`tools::lock`, a file lock).

use super::*;
use crate::prompt;
#[cfg(not(test))]
use crate::prompt::{dialog, Surface};
use crate::recipe::store;
use crate::requests::{self, RequestStatus};
use crate::{actions, consent, events, intake, paths, tools};
use serde_json::json;
use std::path::Path;
use std::time::Duration;

/// The daily update pass runs from `maintain` when the last one is older.
const UPDATE_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

pub fn run_client(root: &Path, argv_in: &[String]) -> Result<(i32, Value), String> {
    let (as_name, argv) = split_as(argv_in);
    let argv = &argv[..];
    let cmd = argv.first().map(String::as_str).unwrap_or("help");
    if matches!(cmd, "version" | "--version") {
        return Ok((0, version_info()));
    }
    if matches!(cmd, "help" | "--help" | "-h") {
        return Ok((0, json!({ "usage": USAGE })));
    }
    if cmd == "recipe" && argv.get(1).map(String::as_str) == Some("validate") {
        return validate_file(argv.get(2));
    }
    open(root)?;
    let refresh = cmd == "catalog" || has(argv, "--refresh");
    if !refresh && !cfg!(test) {
        store::refresh_catalog_if_stale();
    }
    let by: String = as_name.unwrap_or_else(|| "roadie CLI".into()).trim().chars().take(60).collect();
    let tool_arg = argv.get(2).map(|a| target(a)).transpose()?;
    let out = match (cmd, argv.get(1).map(String::as_str), tool_arg) {
        ("maintain", _, _) => maintain(root, has(argv, "--at-login")),
        ("tool", Some("list"), _) => {
            if refresh {
                if let Err(e) = store::refresh_catalog() {
                    eprintln!("roadie: the recipe catalog could not be refreshed ({e}); listing the cached one");
                }
            }
            Ok((0, Value::Array(store::list().into_iter().map(|s| intake::public_status(tools::status(&s.recipe), &s)).collect())))
        }
        ("catalog", Some("refresh"), _) => {
            let r = store::refresh_catalog();
            Ok((if r.is_ok() { 0 } else { 1 }, json!(crate::recipe::catalog::status())))
        }
        ("catalog", Some("status") | None, _) => Ok((0, json!(crate::recipe::catalog::status()))),
        ("tool", Some("status"), Some(t)) => known(&t).map(|(stored, m)| (0, annotate(intake::public_status(tools::status(&stored.recipe), &stored), m))),
        ("tool", Some(action @ ("start" | "stop" | "restart" | "check")), Some(t)) => act(&t, action),
        ("tool", Some("options"), Some(t)) => options(&t),
        ("tool", Some("install"), Some(t)) => install(&t, argv, &by),
        ("tool", Some("upgrade" | "update"), Some(t)) => upgrade(&t, &by),
        ("tool", Some("uninstall"), Some(t)) => match intake::uninstall(&t.name, has(argv, "--keep-data")) {
            Ok(kind) => ask(requests::create(kind, &by)),
            Err(r) => Ok(refused(r)),
        },
        ("tool", Some("autostart"), Some(t)) => autostart(&t, argv.get(3).map(String::as_str)),
        ("tool", Some("connection"), Some(t)) => connection(&t, value_of(argv, "--consumer"), &by),
        ("tool", Some("logs"), Some(t)) => logs(&t, value_of(argv, "--lines")),
        ("recipe", Some("dryrun"), Some(t)) => dryrun(&t),
        _ => Err(format!("unknown command `{}`\n{USAGE}", argv.join(" "))),
    };
    // Any command may have changed a tool's "start at login": keep the one
    // login item in step, at this binary's current path.
    sync_login_item(root);
    out
}

fn open(root: &Path) -> Result<(), String> {
    std::fs::create_dir_all(root).map_err(|e| format!("create {}: {e}", root.display()))?;
    paths::init(root.to_path_buf());
    store::load_all();
    Ok(())
}

/// A refusal as output: invalid asks are usage errors (3), the rest failed (1).
fn refused(r: intake::Refusal) -> (i32, Value) {
    let code = match r.kind {
        intake::Refused::BadRequest | intake::Refused::Invalid => 3,
        intake::Refused::NotFound | intake::Refused::Conflict => 1,
    };
    let mut out = json!({ "error": r.message });
    if let (Some(o), Some(extra)) = (out.as_object_mut(), r.extra.as_object()) {
        for (k, v) in extra {
            o.insert(k.clone(), v.clone());
        }
    }
    eprintln!("roadie: {}", r.message);
    (code, out)
}

/// The stored recipe for a tool argument, and with a recipe file whether
/// that file is exactly the trusted recipe (`None` without a file).
fn known(t: &Target) -> Result<(store::Stored, Option<bool>), String> {
    let stored = store::get(&t.name).ok_or_else(|| match &t.file {
        Some(file) => format!("Roadie has no recipe named {} yet; install it with `roadie tool install {file}`", t.name),
        None => format!("unknown tool: {}", t.name),
    })?;
    let matches = t.recipe.as_ref().map(|v| {
        let same = crate::recipe::from_value(v.clone()).map(|r| store::compare(&r).is_none()).unwrap_or(false);
        if !same {
            eprintln!("roadie: {} differs from the recipe Roadie trusts for {}; `roadie tool upgrade {}` proposes it to the user", t.file.as_deref().unwrap_or(""), t.name, t.file.as_deref().unwrap_or(""));
        }
        same
    });
    Ok((stored, matches))
}

/// What an install can be given: the recipe file's own options when the
/// client names its file, else the stored (or catalog) recipe's.
fn options(t: &Target) -> Result<(i32, Value), String> {
    let recipe = match &t.recipe {
        Some(v) => match crate::recipe::from_value(v.clone()) {
            Ok(r) => r,
            Err(errors) => return Ok((1, json!({ "ok": false, "errors": errors }))),
        },
        None => store::get(&t.name).ok_or_else(|| format!("unknown tool: {}", t.name))?.recipe,
    };
    Ok((0, intake::options(&recipe)))
}

fn act(t: &Target, action: &str) -> Result<(i32, Value), String> {
    let (stored, m) = known(t)?;
    let recipe = match intake::trusted(&t.name) {
        Ok(r) => r,
        Err(r) => return Ok(refused(r)),
    };
    let result = match action {
        "start" => tools::start(&recipe, "cli"),
        "stop" => tools::stop(&recipe),
        "restart" => tools::restart(&recipe),
        _ => tools::check_updates(&recipe),
    };
    Ok(match result {
        Ok(st) => (0, annotate(intake::public_status(st, &stored), m)),
        Err(e) => (1, json!({ "error": e })),
    })
}

/// The CLI is the caller: register the consumer it names, under the name it
/// gives. Unlike the API this needs no separate registration step, because
/// nothing is granted until the user approves the grant in the prompt.
fn ensure_consumer(id: &str, display: &str) -> Result<(), String> {
    if consent::get(id).is_none() {
        consent::register(id, display, None)?;
    }
    Ok(())
}

fn install(t: &Target, argv: &[String], by: &str) -> Result<(i32, Value), String> {
    let values = parse_sets(argv)?;
    let consumer = value_of(argv, "--consumer");
    if let Some(c) = &consumer {
        ensure_consumer(c, by)?;
    }
    match intake::install(&t.name, intake::InstallAsk { values, consumer, recipe: t.recipe.clone() }) {
        Ok(plan) => {
            if let Some(o) = &plan.other_instance {
                eprintln!("roadie: {}", o.message);
            }
            ask(requests::create(plan.kind, by))
        }
        Err(r) => Ok(refused(r)),
    }
}

fn upgrade(t: &Target, by: &str) -> Result<(i32, Value), String> {
    match intake::update(&t.name, t.recipe.clone()) {
        Err(r) => Ok(refused(r)),
        Ok(intake::UpdatePlan::Review { kind, .. }) => ask(requests::create(kind, by)),
        Ok(intake::UpdatePlan::Now(recipe)) => {
            let stored = store::get(&recipe.name).ok_or("recipe vanished")?;
            let mut progress = progress_printer();
            Ok(match intake::update_now(&recipe, &mut progress) {
                Ok(st) => (0, intake::public_status(st, &stored)),
                Err(e) => (1, json!({ "error": e })),
            })
        }
    }
}

fn autostart(t: &Target, on: Option<&str>) -> Result<(i32, Value), String> {
    let enabled = match on {
        Some("on" | "true") => true,
        Some("off" | "false") => false,
        _ => return Err(format!("tool autostart needs on or off\n{USAGE}")),
    };
    let (stored, _) = known(t)?;
    let recipe = match intake::trusted(&t.name) {
        Ok(r) => r,
        Err(r) => return Ok(refused(r)),
    };
    Ok(match tools::set_autostart(&recipe, enabled) {
        Ok(st) => (0, intake::public_status(st, &stored)),
        Err(e) => (1, json!({ "error": e })),
    })
}

/// A tool's URL and the consumer's own key. The first time, the user is
/// asked to let that consumer connect; the grant is remembered.
fn connection(t: &Target, consumer: Option<String>, by: &str) -> Result<(i32, Value), String> {
    let consumer = consumer.ok_or_else(|| format!("tool connection needs --consumer <id>: the app asking, which gets its own key once the user allows it\n{USAGE}"))?;
    let recipe = match intake::trusted(&t.name) {
        Ok(r) => r,
        Err(r) => return Ok(refused(r)),
    };
    ensure_consumer(&consumer, by)?;
    for asked in [false, true] {
        match intake::consumer_connection(&recipe, &consumer) {
            Ok(v) => return Ok((0, v)),
            Err(intake::ConnError::ConsentRequired) if !asked => {
                let (code, out) = ask(requests::create(requests::RequestKind::Connect { consumer: consumer.clone(), tool: recipe.name.clone(), return_url: None }, by))?;
                if code != 0 {
                    return Ok((code, out));
                }
            }
            Err(intake::ConnError::NotInstalled) => return Ok((1, json!({ "error": format!("{} is not installed; install it first", recipe.display_name), "reason": "not-installed" }))),
            Err(intake::ConnError::Other(e)) => return Ok((1, json!({ "error": e }))),
            Err(e) => return Ok((1, json!({ "error": format!("{e:?}") }))),
        }
    }
    Ok((1, json!({ "error": "the grant did not take effect" })))
}

fn logs(t: &Target, lines: Option<String>) -> Result<(i32, Value), String> {
    let recipe = match intake::trusted(&t.name) {
        Ok(r) => r,
        Err(r) => return Ok(refused(r)),
    };
    let n = lines.and_then(|l| l.parse().ok()).unwrap_or(100usize).min(2000);
    Ok(match tools::log_tail(&recipe, n) {
        Ok(text) => (0, json!({ "lines": text.lines().collect::<Vec<_>>() })),
        Err(e) => (1, json!({ "error": e })),
    })
}

fn dryrun(t: &Target) -> Result<(i32, Value), String> {
    let recipe = match &t.recipe {
        Some(v) => match crate::recipe::from_value(v.clone()) {
            Ok(r) => r,
            Err(errors) => return Ok((1, json!({ "ok": false, "errors": errors }))),
        },
        None => store::get(&t.name).ok_or_else(|| format!("unknown recipe: {}", t.name))?.recipe,
    };
    Ok(match tools::dry_run(&recipe) {
        Ok(d) => (0, serde_json::to_value(d).unwrap_or_default()),
        Err(e) => (1, json!({ "error": e })),
    })
}

/// `roadie maintain`: start the daemons marked "start at login" (at login,
/// also the ones the user stopped last session, as a login would), apply
/// staged updates to idle tools, and run the update pass when a day has
/// passed. An app bundling Roadie runs it on launch; on Windows the login
/// item runs it with `--at-login` (elsewhere each daemon has its own item).
fn maintain(root: &Path, at_login: bool) -> Result<(i32, Value), String> {
    let recipes: Vec<_> = store::list().into_iter().filter(|s| s.trusted()).map(|s| s.recipe).collect();
    tools::reconcile_with(&recipes, &events::emit, at_login);
    let stamp = root.join("last-update-pass");
    let last = std::fs::read_to_string(&stamp).ok().and_then(|t| t.trim().parse::<u64>().ok()).unwrap_or(0);
    let due = paths::now_secs().saturating_sub(last) >= UPDATE_EVERY.as_secs();
    let ran = due && actions::auto_update_enabled();
    if ran {
        tools::auto_update(&recipes, &events::emit);
        paths::write_atomic(&stamp, paths::now_secs().to_string().as_bytes(), false)?;
    }
    Ok((0, json!({ "atLogin": at_login, "updatePass": ran, "tools": recipes.len() })))
}

/// macOS: each daemon that starts at login has a login item of its
/// own, and the `maintain` item an older Roadie registered goes (after the
/// daemons' items exist, so an upgrade never leaves a login that starts
/// nothing). Windows: one `maintain` item while any tool starts at login,
/// none otherwise.
fn sync_login_item(root: &Path) {
    if cfg!(test) {
        return;
    }
    if tools::autostart::NATIVE_TOOL_ITEMS {
        let recipes: Vec<_> = store::list().into_iter().filter(|s| s.trusted()).map(|s| s.recipe).collect();
        tools::sync_login_items(&recipes);
        if tools::autostart::maintain_enabled(root) {
            if let Err(e) = tools::autostart::disable_maintain(root) {
                eprintln!("roadie: could not remove the old login item: {e}");
            }
        }
        return;
    }
    let any = store::list().into_iter().filter(|s| s.trusted()).any(|s| paths::tool_paths(&s.recipe.name).is_ok_and(|p| tools::state::load(&p.data).autostart));
    let result = if any {
        tools::autostart::enable_maintain(root)
    } else if tools::autostart::maintain_enabled(root) {
        tools::autostart::disable_maintain(root)
    } else {
        Ok(())
    };
    if let Err(e) = result {
        eprintln!("roadie: could not update the login item: {e}");
    }
}

// --- Asking the user, here ---

enum Answer {
    Approve,
    Decline,
    /// Closed without an answer, or nothing could ask.
    #[cfg_attr(test, allow(dead_code))]
    Leave(String),
}

/// Show a request to the user from this process and carry out the answer.
fn ask(r: requests::Request) -> Result<(i32, Value), String> {
    let p = prompt::describe_here(&r);
    match present(&p)? {
        Answer::Approve => run_decision(&r.id),
        Answer::Decline => {
            let done = actions::decide(&r.id, false, None)?;
            Ok((2, serde_json::to_value(done).unwrap_or_default()))
        }
        Answer::Leave(note) => {
            let done = requests::set_status(&r.id, RequestStatus::Declined, Some(note.clone())).ok_or("request vanished")?;
            eprintln!("roadie: {note}");
            Ok((2, serde_json::to_value(done).unwrap_or_default()))
        }
    }
}

#[cfg(not(test))]
fn present(p: &prompt::Prompt) -> Result<Answer, String> {
    match prompt::surface() {
        Surface::Terminal => {
            if !interactive() {
                return Err("this computer has no screen to show the request on and no terminal to ask in; run the command yourself in a terminal (there is no --yes)".into());
            }
            Ok(match ask_tty(p)? {
                TtyAnswer::Approve => Answer::Approve,
                TtyAnswer::Decline => Answer::Decline,
                TtyAnswer::Leave(note) => Answer::Leave(note),
            })
        }
        Surface::Dialog | Surface::Window => {
            eprintln!("roadie: asking the user in a dialog…");
            Ok(match dialog::show(p)? {
                dialog::Answer::Approve if p.approve.is_some() => Answer::Approve,
                dialog::Answer::Approve | dialog::Answer::Decline => Answer::Decline,
                dialog::Answer::Dismissed => Answer::Leave("the dialog was closed without an answer".into()),
            })
        }
    }
}

/// Tests answer for the user (`answer_next`); unset means decline.
#[cfg(test)]
fn present(p: &prompt::Prompt) -> Result<Answer, String> {
    Ok(match tests::NEXT.lock().unwrap().take() {
        Some(true) if p.approve.is_some() => Answer::Approve,
        _ => Answer::Decline,
    })
}

/// Approve: run the action on a thread and mirror its progress to stderr.
fn run_decision(id: &str) -> Result<(i32, Value), String> {
    let id_owned = id.to_string();
    let worker = std::thread::spawn(move || actions::decide(&id_owned, true, None));
    let mut last = String::new();
    while !worker.is_finished() {
        if let Some(p) = requests::get(id).and_then(|r| r.progress) {
            let line = match (p.phase.as_str(), p.total) {
                ("downloading", Some(t)) if t > 0 => format!("downloading {}%", p.downloaded * 100 / t),
                (phase, _) => phase.to_string(),
            };
            if line != last {
                eprintln!("roadie: {line}");
                last = line;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let r = worker.join().map_err(|_| "the action panicked".to_string())??;
    let code = match r.status {
        RequestStatus::Done => 0,
        RequestStatus::Declined => 2,
        _ => 1,
    };
    Ok((code, serde_json::to_value(r).unwrap_or_default()))
}

fn progress_printer() -> impl FnMut(tools::install::Phase, u64, Option<u64>) {
    let mut last = String::new();
    move |phase, downloaded, total| {
        let phase = serde_json::to_value(phase).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
        let line = match total {
            Some(t) if t > 0 && phase == "downloading" => format!("downloading {}%", downloaded * 100 / t),
            _ => phase,
        };
        if line != last {
            eprintln!("roadie: {line}");
            last = line;
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::sync::Mutex;

    /// The user's next answer in `present`.
    pub static NEXT: Mutex<Option<bool>> = Mutex::new(None);

    fn answer_next(approve: bool) {
        *NEXT.lock().unwrap() = Some(approve);
    }

    /// Tests share one process-wide data root and request queue.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn root() -> std::path::PathBuf {
        let r = std::env::temp_dir().join(format!("roadie-cli-local-{}", std::process::id()));
        std::fs::create_dir_all(&r).unwrap();
        // The recipes a user who installed them before the catalog has:
        // trusted, from the catalog.
        static SEED: std::sync::Once = std::sync::Once::new();
        SEED.call_once(|| {
            paths::init(r.clone());
            for recipe in crate::recipe::fixtures::all() {
                store::put_trusted(recipe, store::RecipeSource::Catalog).unwrap();
            }
        });
        r
    }

    fn run(args: &[&str]) -> (i32, Value) {
        let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        run_client(&root(), &argv).unwrap()
    }

    /// yt-dlp for another platform only, under a new name: approving it
    /// trusts the recipe, then fails at install without the network.
    fn elsewhere_recipe(name: &str) -> std::path::PathBuf {
        let mut r: Value = serde_json::from_str(crate::recipe::fixtures::json("yt-dlp")).unwrap();
        let other = if crate::recipe::Platform::current().key().starts_with("darwin") { "windows-x64" } else { "darwin-arm64" };
        let asset = r["source"]["assets"][other].clone();
        r["name"] = json!(name);
        r["platforms"] = json!([other]);
        r["source"]["assets"] = json!({ other: asset });
        let path = root().join(format!("{name}.json"));
        std::fs::write(&path, serde_json::to_string(&r).unwrap()).unwrap();
        path
    }

    #[test]
    fn a_brought_recipe_is_asked_about_here_and_trusted_only_on_yes() {
        let _s = serial();
        let file = elsewhere_recipe("cli-local-demo");
        let file = file.to_str().unwrap();

        let unknown = run_client(&root(), &["tool".into(), "status".into(), file.into()]);
        assert!(unknown.unwrap_err().contains("roadie tool install"), "unknown before install says how to install it");

        answer_next(false);
        let (code, out) = run(&["--as", "Test App", "tool", "install", file]);
        assert_eq!((code, out["status"].as_str()), (2, Some("declined")), "{out}");
        assert!(store::get("cli-local-demo").is_none(), "declining stores nothing");

        answer_next(true);
        let (code, out) = run(&["--as", "Test App", "tool", "install", file]);
        assert_eq!((code, out["status"].as_str()), (1, Some("failed")), "approved, then the wrong platform fails: {out}");
        assert_eq!(store::get("cli-local-demo").map(|s| s.origin), Some(store::Origin::User), "approval trusted it");

        let (code, out) = run(&["tool", "status", file]);
        assert_eq!((code, out["recipeDiffers"].as_bool()), (0, Some(false)), "{out}");

        let (code, out) = run(&["tool", "upgrade", file]);
        assert_eq!(code, 1, "not installed: {out}");
        let _ = store::delete("cli-local-demo");
    }

    #[test]
    fn a_catalog_tool_installs_after_one_dialog_and_offers_recipe_updates() {
        let _s = serial();
        let _c = crate::recipe::catalog::tests::serial();
        root();
        let file = elsewhere_recipe("cli-local-cat");
        let v1 = crate::recipe::parse(&std::fs::read_to_string(&file).unwrap()).unwrap();
        crate::recipe::catalog::tests::publish(std::slice::from_ref(&v1));

        let (code, out) = run(&["tool", "status", "cli-local-cat"]);
        assert_eq!((code, out["origin"].as_str(), out["available"].as_bool(), out["installed"].as_bool()), (0, Some("catalog"), Some(true), Some(false)), "{out}");

        answer_next(false);
        let (code, _) = run(&["--as", "Test App", "tool", "install", "cli-local-cat"]);
        assert_eq!(code, 2, "declined");
        assert_eq!(store::get("cli-local-cat").map(|s| s.origin), Some(store::Origin::Catalog), "declining trusts nothing");

        answer_next(true);
        let (code, out) = run(&["--as", "Test App", "tool", "install", "cli-local-cat"]);
        assert_eq!(code, 1, "approved, then the wrong platform fails: {out}");
        let s = store::get("cli-local-cat").unwrap();
        assert_eq!((s.origin, s.source), (store::Origin::User, store::RecipeSource::Catalog), "the one dialog trusted the catalog's recipe");

        let mut v2 = v1.clone();
        v2.revision += 1;
        crate::recipe::catalog::tests::publish(&[v2]);
        let (code, out) = run(&["tool", "status", "cli-local-cat"]);
        assert_eq!((code, out["recipeUpdate"]["revision"].as_u64()), (0, Some(u64::from(v1.revision) + 1)), "{out}");
        let (code, out) = run(&["tool", "upgrade", "cli-local-cat"]);
        assert_eq!(code, 1, "not installed, so nothing to update: {out}");

        crate::recipe::catalog::tests::publish(&[]);
        let _ = store::delete("cli-local-cat");
    }

    /// The client's flow: it has the recipe file (downloaded from the
    /// catalog), lists the options to show its user, then asks to install
    /// with the values the user chose; one dialog approves.
    #[test]
    fn a_client_lists_options_then_installs_with_its_values() {
        let _s = serial();
        root();
        let mut r: Value = serde_json::from_str(crate::recipe::fixtures::json("slskd@6")).unwrap();
        let other = if crate::recipe::Platform::current().key().starts_with("darwin") { "windows-x64" } else { "darwin-arm64" };
        let asset = r["source"]["assets"][other].clone();
        r["name"] = json!("cli-flow");
        r["platforms"] = json!([other]);
        r["source"]["assets"] = json!({ other: asset });
        let file = root().join("cli-flow.json");
        std::fs::write(&file, serde_json::to_string(&r).unwrap()).unwrap();
        let file = file.to_str().unwrap();

        let (code, out) = run(&["tool", "options", file]);
        assert_eq!(code, 0, "{out}");
        let keys: Vec<&str> = out["options"].as_array().unwrap().iter().map(|o| o["key"].as_str().unwrap()).collect();
        assert!(["soulseekUsername", "ports.web", "secrets.internalKey", "installDir", "startNow"].iter().all(|k| keys.contains(k)), "{keys:?}");

        let required: Vec<&str> = out["options"].as_array().unwrap().iter().filter(|o| o["required"] == true).map(|o| o["key"].as_str().unwrap()).collect();
        assert_eq!(required, vec!["soulseekUsername", "soulseekPassword"]);

        // A dialog cannot ask for values: without the required ones, refused, naming them.
        let (code, out) = run(&["--as", "Flow App", "tool", "install", file, "--set", "soulseekUsername=bj"]);
        assert_eq!(code, 3, "a missing required value is the caller's to fix: {out}");
        assert!(out.to_string().contains("soulseekPassword"), "{out}");

        answer_next(true);
        let key = "a-key-the-client-chose-1234";
        let (code, out) = run(&[
            "--as", "Flow App", "tool", "install", file, "--set", "soulseekUsername=bj", "--set", "soulseekPassword=pw", "--set", "ports.web=6123",
            "--set", &format!("secrets.internalKey={key}"), "--set", "startNow=false",
        ]);
        assert_eq!(code, 1, "approved, then this platform has no build: {out}");
        assert!(!out.to_string().contains(key) && !out.to_string().contains("\"pw\""), "the chosen key and the password are never printed");
        let p = paths::tool_paths("cli-flow").unwrap();
        let st = tools::state::load(&p.data);
        assert_eq!((st.ports.get("web"), st.secrets.get("internalKey").map(String::as_str), st.config.get("soulseekUsername")), (Some(&6123), Some(key), Some(&json!("bj"))), "the client's values landed");
        let (_, status) = run(&["tool", "status", "cli-flow"]);
        assert!(status["installing"].is_null(), "no install running once it ended: {status}");
        let _ = tools::uninstall(&crate::recipe::parse(&serde_json::to_string(&r).unwrap()).unwrap(), false);
        let _ = store::delete("cli-flow");
    }

    #[test]
    fn refusals_and_reads_need_no_prompt() {
        let _s = serial();
        let (code, out) = run(&["tool", "list"]);
        assert_eq!(code, 0);
        assert!(out.as_array().is_some_and(|a| a.iter().any(|t| t["name"] == "slskd" && t.get("pid").is_none())), "{out}");
        let (code, out) = run(&["tool", "install", "no-such-tool"]);
        assert_eq!(code, 1, "{out}");
        let (code, _) = run(&["tool", "install", "yt-dlp", "--set", "nope=1"]);
        assert_eq!(code, 3, "an invalid value is a usage error");
        let (code, out) = run(&["tool", "connection", "yt-dlp", "--consumer", "test-app"]);
        assert_eq!(code, 1, "a command-line tool has no connection: {out}");
        let (code, out) = run(&["maintain"]);
        assert_eq!((code, out["atLogin"].as_bool()), (0, Some(false)), "{out}");
        assert!(run_client(&root(), &["tool".into(), "autostart".into(), "yt-dlp".into(), "sideways".into()]).is_err());
    }
}
