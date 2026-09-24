//! How a request reaches the user. Three surfaces, picked by the service:
//!
//! - **Window**: the desktop build (`window` feature) opens or focuses
//!   Roadie's window, which shows `RequestPrompt` (`service::open_window_if_needed`).
//! - **Dialog**: the build without the window shows a native yes/no dialog
//!   from the service itself: `osascript` on macOS, `MessageBoxW` on
//!   Windows, `zenity` or `kdialog` on Linux. The service owns the answer and
//!   calls `actions::decide` directly, so no other process is involved.
//! - **Terminal**: no screen at all (SSH, a headless box). The CLI may then
//!   prompt on its own TTY (`roadie request <id> answer`) and approve over
//!   the owner channel, which mints a *terminal* token only in this case
//!   (`owner.rs`). On a machine with a screen the owner channel refuses the
//!   terminal, because a program can run the CLI inside a pseudo-terminal it
//!   controls and type "y" itself; a dialog on the screen it cannot answer.
//!
//! "Has a screen" is asked of the OS, not of the environment, where it can
//! be (macOS: the security session's graphic access; Windows: whether the
//! window station is visible), because a program that restarts the service
//! with a doctored environment must not be able to downgrade it to Terminal.
//! Linux has no such check; `DISPLAY`/`WAYLAND_DISPLAY` decide, and the same
//! program could drive an X11 dialog anyway, exactly as it could the window.

use crate::recipe::store::{self, Change};
use crate::recipe::{self, ConfigField, Recipe, Source};
use crate::tools::DryRun;
use crate::requests::{self, Request, RequestKind, RequestStatus};
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Surface {
    Window,
    Dialog,
    Terminal,
}

impl Surface {
    pub fn name(self) -> &'static str {
        match self {
            Surface::Window => "window",
            Surface::Dialog => "dialog",
            Surface::Terminal => "terminal",
        }
    }
}

/// Where this service shows requests.
pub fn surface() -> Surface {
    pick(screen::available(), cfg!(feature = "window"), dialog::available())
}

fn pick(screen: bool, window_build: bool, dialog_tool: bool) -> Surface {
    match (screen, window_build, dialog_tool) {
        (false, _, _) => Surface::Terminal,
        (true, true, _) => Surface::Window,
        (true, false, true) => Surface::Dialog,
        (true, false, false) => Surface::Terminal,
    }
}

/// May a CLI on a terminal answer requests? Only when nothing can be shown
/// on a screen.
pub fn terminal_allowed() -> bool {
    surface() == Surface::Terminal
}

// --- What the prompt says ---

/// One request, described for a dialog or a terminal. Mirrors the window's
/// `RequestPrompt`: who asks, what, what they decided, what is still open.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Prompt {
    pub headline: String,
    pub lines: Vec<String>,
    /// "Install slskd?"
    pub question: String,
    /// The approving button's label; `None` when this request cannot be
    /// approved from a dialog or terminal (see `blocked`).
    pub approve: Option<String>,
    pub blocked: Option<String>,
}

impl Prompt {
    /// Headline, details and the reason it is blocked, as plain text.
    pub fn text(&self) -> String {
        let mut out = self.headline.clone();
        if !self.lines.is_empty() {
            out.push_str("\n\n");
            out.push_str(&self.lines.join("\n"));
        }
        if let Some(b) = &self.blocked {
            out.push_str("\n\n");
            out.push_str(b);
        }
        out
    }
}

/// Text a client chose (its name, config values) is shown, never
/// interpreted: no control or bidi-override characters, no `<`/`>` (kdialog
/// renders anything that looks like HTML), one line, bounded.
pub fn clean(s: &str, max: usize) -> String {
    let t: String = s
        .chars()
        .filter(|c| !matches!(c, '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'))
        .map(|c| match c {
            '<' => '‹',
            '>' => '›',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() > max {
        format!("{}…", t.chars().take(max).collect::<String>())
    } else {
        t
    }
}

fn show_value(v: &Value) -> String {
    match v {
        Value::Bool(true) => "yes".into(),
        Value::Bool(false) => "no".into(),
        Value::String(s) => clean(s, 80),
        other => clean(&other.to_string(), 80),
    }
}

fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    rest.split(['/', '?', '#']).next().unwrap_or(rest).to_string()
}

fn source_line(r: &Recipe) -> String {
    match &r.source {
        Source::GithubRelease { repo, .. } => format!("Downloads from github.com/{repo} (its latest release)."),
        Source::HttpRedirect { latest_url, .. } => {
            let mut hosts: Vec<String> = latest_url.values().map(|u| host_of(u)).collect();
            hosts.dedup();
            format!("Downloads from {}.", hosts.join(", "))
        }
        Source::HtmlIndex { page, .. } => format!("Downloads from {}.", host_of(page)),
    }
}

fn filled(v: &Value) -> bool {
    !v.is_null() && v.as_str() != Some("")
}

/// Required install fields with no value anywhere: not `supplied` by the
/// request, no recipe default, nothing already stored for the tool.
pub fn missing_required<'a>(recipe: &'a Recipe, supplied: &dyn Fn(&str) -> bool, stored: &Map<String, Value>) -> Vec<&'a ConfigField> {
    recipe
        .install_fields()
        .into_iter()
        .filter(|f| f.required)
        .filter(|f| {
            !(supplied(&f.key)
                || f.default.as_ref().is_some_and(filled)
                || stored.get(&f.key).is_some_and(filled)
                || stored.get(&format!("has_{}", f.key)) == Some(&Value::Bool(true)))
        })
        .collect()
}

/// An install request that leaves a required value open fails up front
/// wherever nothing on screen can ask for it (a dialog or a terminal: only
/// the window has a form). The error names each key's pointer and the fix.
pub fn refuse_missing(recipe: &Recipe, values: &Map<String, Value>, stored: &Map<String, Value>, surface: Surface) -> Option<(String, Vec<String>)> {
    if surface == Surface::Window {
        return None;
    }
    let missing = missing_required(recipe, &|k| values.get(k).is_some_and(filled), stored);
    if missing.is_empty() {
        return None;
    }
    let keys: Vec<String> = missing.iter().map(|f| f.key.clone()).collect();
    let named = missing.iter().map(|f| format!("{} (/config/{})", f.label, f.key)).collect::<Vec<_>>().join(", ");
    let example = keys.iter().map(|k| format!("--set {k}=…")).collect::<Vec<_>>().join(" ");
    Some((
        format!("{} needs {named}, and this Roadie has no window to ask for it: pass it with the request (CLI: `roadie tool install {} {example}`)", recipe.display_name, recipe.name),
        keys,
    ))
}

/// How a request's own recipe differs from what Roadie has, in words.
pub fn change_line(change: Change, recipe: &Recipe) -> String {
    let what = match change {
        Change::New => "It brings its own recipe for a tool Roadie does not know yet",
        Change::ReplacesBuiltin => "It brings its own recipe, replacing Roadie's built-in one",
        Change::ChangesTrusted => "It brings a changed recipe, replacing the one you trusted",
        Change::ReplacesDraft => "It brings its own recipe, replacing an unreviewed draft",
    };
    let by = if recipe.author.trim().is_empty() { "an unnamed author".to_string() } else { clean(&recipe.author, 60) };
    format!("{what}: by {by}, revision {}. Approving trusts it.", recipe.revision)
}

/// What a brought recipe would do on this computer, from a dry run: the
/// review screen's facts as lines. Every string here came from the client,
/// so all of it goes through `clean`.
pub fn review_lines(proposed: &Recipe, current: Option<&Recipe>, dry: Result<&DryRun, &str>) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(cur) = current {
        let keys = store::changed_keys(cur, proposed);
        if !keys.is_empty() {
            lines.push(format!("Changed from the current recipe: {}.", clean(&keys.join(", "), 200)));
        }
    }
    match dry {
        Ok(d) => {
            match (&d.resolved, &d.resolve_error) {
                (Some(res), _) => lines.push(format!(
                    "Downloads {} ({}).",
                    clean(&res.download_url, 300),
                    if res.checksums_url.is_some() { "checked against the upstream checksum" } else { "no upstream checksum; verified by running it" }
                )),
                (None, Some(e)) => {
                    lines.push(source_line(proposed));
                    lines.push(format!("Could not resolve the latest release: {}.", clean(e, 200)));
                }
                (None, None) => lines.push(format!("Not available for this computer ({}).", d.platform)),
            }
            if proposed.kind == recipe::Kind::Daemon {
                let bin = proposed.binaries().into_iter().next().unwrap_or_else(|| proposed.name.clone());
                lines.push(format!("Runs: {}", clean(&std::iter::once(bin).chain(d.run_args.iter().cloned()).collect::<Vec<_>>().join(" "), 400)));
                if !d.ports.is_empty() {
                    lines.push(format!("Listens on 127.0.0.1, ports {}.", d.ports.iter().map(|(k, p)| format!("{} {p}", clean(k, 30))).collect::<Vec<_>>().join(", ")));
                }
            }
            if let Some(bin) = &d.bin_path {
                lines.push(format!("Installs the command {}.", clean(bin, 200)));
            }
            if !d.files.is_empty() {
                lines.push(format!("Writes: {}.", clean(&d.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>().join(", "), 400)));
            }
            if !d.create_dirs.is_empty() {
                lines.push(format!("Creates folders: {}.", clean(&d.create_dirs.join(", "), 300)));
            }
        }
        Err(e) => {
            lines.push(source_line(proposed));
            lines.push(format!("Could not preview it: {}.", clean(e, 200)));
        }
    }
    lines
}

/// Describe `r`. `trusted` is the tool's trusted recipe (`None` for an
/// unknown tool or a draft); a recipe the request brought takes its place.
/// `stored` is the tool's current public config (`ToolStatus.config`, with
/// `has_<key>` for secrets); `review` are the `review_lines` of a brought
/// recipe.
pub fn describe(r: &Request, trusted: Option<&Recipe>, stored: &Map<String, Value>, review: &[String]) -> Prompt {
    let by = clean(&r.requested_by, 60);
    let brought: Option<(&Recipe, Change)> = match &r.kind {
        RequestKind::Install { recipe: Some(p), recipe_change: Some(c), .. } | RequestKind::ReplaceRecipe { recipe: p, recipe_change: c, .. } => Some((p, *c)),
        _ => None,
    };
    let recipe = brought.map(|(p, _)| p).or(trusted);
    let name = recipe.map(|r| clean(&r.display_name, 60)).unwrap_or_else(|| r.kind.tool().to_string());
    let provenance = format!("\u{201C}{by}\u{201D} is the name the asking program gave; Roadie cannot verify it.");
    let Some(recipe) = recipe else {
        return Prompt {
            headline: format!("{by} asks about {name}"),
            lines: vec![provenance],
            question: format!("Decline this request for {name}?"),
            approve: None,
            blocked: Some(format!("{name}'s recipe is not trusted (an unreviewed draft or unknown), and this request did not bring one, so it can only be declined here.")),
        };
    };
    let mut recipe_lines = Vec::new();
    if let Some((p, change)) = brought {
        recipe_lines.push(change_line(change, p));
        recipe_lines.extend(review.iter().cloned());
    }
    match &r.kind {
        RequestKind::Install { consumer, config, secret_keys, .. } => {
            let mut lines = Vec::new();
            if !recipe.summary.trim().is_empty() {
                lines.push(clean(&recipe.summary, 240));
            }
            if brought.is_some() {
                lines.extend(recipe_lines);
            } else {
                lines.push(source_line(recipe));
                if !recipe.author.trim().is_empty() {
                    lines.push(format!("Recipe by {}.", clean(&recipe.author, 60)));
                }
            }
            if consumer.is_some() {
                lines.push(format!("Approving also gives {by} its own access key for {name}."));
            }
            let mut decided = Vec::new();
            let mut left_empty = Vec::new();
            let mut missing = Vec::new();
            for f in recipe.install_fields() {
                let label = clean(&f.label, 60);
                if secret_keys.contains(&f.key) {
                    decided.push(format!("{label}: provided"));
                } else if let Some(v) = config.get(&f.key).filter(|v| filled(v)) {
                    decided.push(format!("{label}: {}", show_value(v)));
                } else {
                    let settled = f.default.as_ref().is_some_and(filled) || stored.get(&f.key).is_some_and(filled) || stored.get(&format!("has_{}", f.key)) == Some(&Value::Bool(true));
                    if settled {
                        continue;
                    }
                    if f.required {
                        missing.push(format!("{label} ({})", f.key));
                    } else {
                        left_empty.push(label);
                    }
                }
            }
            if recipe.kind == recipe::Kind::Daemon {
                for (key, label, offer) in [("startNow", "Start now", recipe.start_after_install), ("autostart", "Start at login", recipe.autostart)] {
                    if let Some(o) = offer {
                        let v = config.get(key).and_then(|v| v.as_bool()).unwrap_or(o.default);
                        decided.push(format!("{label}: {}", if v { "yes" } else { "no" }));
                    }
                }
            }
            if !decided.is_empty() {
                lines.push(format!("Settings: {}.", decided.join(" · ")));
            }
            if !left_empty.is_empty() {
                lines.push(format!("Left empty: {}.", left_empty.join(", ")));
            }
            lines.push(provenance);
            let blocked = (!missing.is_empty()).then(|| {
                format!("{name} needs a value for {}, and this prompt cannot ask for one. Decline, then ask again with --set key=value.", missing.join(", "))
            });
            let approve = if brought.is_some() { "Trust and install" } else { "Install" };
            Prompt {
                headline: if consumer.is_some() { format!("{by} asks to install {name} and connect to it") } else { format!("{by} asks to install {name}") },
                lines,
                question: if brought.is_some() { format!("Trust this recipe and install {name}?") } else { format!("Install {name}?") },
                approve: blocked.is_none().then(|| approve.to_string()),
                blocked,
            }
        }
        RequestKind::ReplaceRecipe { .. } => {
            let mut lines = recipe_lines;
            lines.push(format!("{name} is installed; approving re-renders its files and updates it (a busy daemon is not restarted until it is idle)."));
            lines.push(provenance);
            Prompt {
                headline: format!("{by} asks to change {name}'s recipe"),
                lines,
                question: format!("Trust the new recipe and update {name}?"),
                approve: Some("Trust and update".into()),
                blocked: None,
            }
        }
        RequestKind::Uninstall { keep_data, .. } => Prompt {
            headline: format!("{by} asks to remove {name}"),
            lines: vec![
                if *keep_data { "Its settings will be kept.".into() } else { "Its settings will be removed too. Your own folders are never deleted.".into() },
                provenance,
            ],
            question: format!("Remove {name}?"),
            approve: Some("Remove".into()),
            blocked: None,
        },
        RequestKind::Connect { .. } => Prompt {
            headline: format!("{by} wants to connect to {name}"),
            lines: vec![format!("It will receive its own access key for {name}."), provenance],
            question: format!("Let {by} connect to {name}?"),
            approve: Some("Allow".into()),
            blocked: None,
        },
    }
}

/// `describe` with the recipe and config looked up in this service. A
/// brought recipe is dry-run for its review lines (this resolves the latest
/// release over the network, so call it off the async runtime).
pub fn describe_here(r: &Request) -> Prompt {
    let tool = r.kind.tool();
    let trusted = store::get_trusted(tool).ok();
    let brought = match &r.kind {
        RequestKind::Install { recipe: Some(p), .. } | RequestKind::ReplaceRecipe { recipe: p, .. } => Some(p.as_ref()),
        _ => None,
    };
    let effective = brought.or(trusted.as_ref());
    let stored_config = effective.map(|rc| crate::tools::status(rc).config).unwrap_or_default();
    let review = match brought {
        Some(p) => {
            let current = store::get(tool).map(|s| s.recipe);
            match crate::tools::dry_run(p) {
                Ok(d) => review_lines(p, current.as_ref(), Ok(&d)),
                Err(e) => review_lines(p, current.as_ref(), Err(&e)),
            }
        }
        None => vec![],
    };
    describe(r, trusted.as_ref(), &stored_config, &review)
}

// --- The service's dialog queue ---

fn shown() -> &'static Mutex<HashSet<String>> {
    static S: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashSet::new()))
}

static RUNNING: AtomicBool = AtomicBool::new(false);

fn next_unshown() -> Option<Request> {
    let seen = shown().lock().unwrap();
    requests::pending().into_iter().find(|r| !seen.contains(&r.id))
}

/// Show every pending request not shown yet, one dialog at a time, on a
/// background thread. A request whose dialog was dismissed stays pending
/// and is not shown again until `show_again`.
pub fn ask_pending() {
    // Unit tests create requests through the API; they must not pop dialogs.
    if cfg!(test) {
        return;
    }
    if RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    let spawned = std::thread::Builder::new().name("prompt".into()).spawn(|| loop {
        match next_unshown() {
            Some(r) => {
                shown().lock().unwrap().insert(r.id.clone());
                present(&r);
            }
            None => {
                RUNNING.store(false, Ordering::SeqCst);
                // A request may have arrived between the check and the store.
                if next_unshown().is_some() && !RUNNING.swap(true, Ordering::SeqCst) {
                    continue;
                }
                return;
            }
        }
    });
    if let Err(e) = spawned {
        RUNNING.store(false, Ordering::SeqCst);
        log::warn!("could not start the prompt thread: {e}");
    }
}

/// `roadie request <id> answer` on a machine with a screen: show it again.
pub fn show_again(id: &str) {
    shown().lock().unwrap().remove(id);
}

fn present(r: &Request) {
    let p = describe_here(r);
    let answer = dialog::show(&p);
    // Decided elsewhere while the dialog was up: nothing to do.
    if requests::get(&r.id).map(|x| x.status) != Some(RequestStatus::Pending) {
        return;
    }
    match answer {
        Ok(dialog::Answer::Approve) if p.approve.is_some() => {
            log::info!("request {} approved in the dialog", r.id);
            if let Err(e) = crate::actions::decide(&r.id, true, None) {
                log::warn!("request {}: {e}", r.id);
            }
        }
        Ok(dialog::Answer::Approve | dialog::Answer::Decline) => {
            log::info!("request {} declined in the dialog", r.id);
            let _ = crate::actions::decide(&r.id, false, None);
        }
        Ok(dialog::Answer::Dismissed) => log::info!("request {} dismissed; still pending (`roadie request {} answer` shows it again)", r.id, r.id),
        Err(e) => log::warn!("could not show a dialog for request {}: {e}", r.id),
    }
}

// --- Is there a screen? ---

pub mod screen {
    #[cfg(target_os = "macos")]
    pub fn available() -> bool {
        // Security.framework: does this process's security session have
        // access to the window server? False under SSH and for launchd jobs
        // outside the GUI session, whatever the environment says.
        #[link(name = "Security", kind = "framework")]
        unsafe extern "C" {
            fn SessionGetInfo(session: u32, session_id: *mut u32, attributes: *mut u32) -> i32;
        }
        const CALLER_SECURITY_SESSION: u32 = u32::MAX;
        const SESSION_HAS_GRAPHIC_ACCESS: u32 = 0x0010;
        let (mut id, mut attrs) = (0u32, 0u32);
        let status = unsafe { SessionGetInfo(CALLER_SECURITY_SESSION, &mut id, &mut attrs) };
        status == 0 && attrs & SESSION_HAS_GRAPHIC_ACCESS != 0
    }

    #[cfg(windows)]
    pub fn available() -> bool {
        // A visible window station is an interactive desktop session; the
        // one OpenSSH and services run in is not.
        use std::ffi::c_void;
        #[repr(C)]
        struct UserObjectFlags {
            inherit: i32,
            reserved: i32,
            flags: u32,
        }
        #[link(name = "user32")]
        unsafe extern "system" {
            fn GetProcessWindowStation() -> *mut c_void;
            fn GetUserObjectInformationW(obj: *mut c_void, index: i32, info: *mut c_void, len: u32, needed: *mut u32) -> i32;
        }
        const UOI_FLAGS: i32 = 1;
        const WSF_VISIBLE: u32 = 1;
        unsafe {
            let station = GetProcessWindowStation();
            if station.is_null() {
                return false;
            }
            let mut f = UserObjectFlags { inherit: 0, reserved: 0, flags: 0 };
            let mut needed = 0u32;
            let ok = GetUserObjectInformationW(station, UOI_FLAGS, &mut f as *mut _ as *mut c_void, std::mem::size_of::<UserObjectFlags>() as u32, &mut needed);
            ok != 0 && f.flags & WSF_VISIBLE != 0
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn available() -> bool {
        ["DISPLAY", "WAYLAND_DISPLAY"].iter().any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()))
    }
}

// --- Native dialogs ---

pub mod dialog {
    use super::Prompt;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Answer {
        Approve,
        Decline,
        /// Closed without an answer or timed out: the request stays pending.
        Dismissed,
    }

    /// Seconds a dialog waits before giving up (macOS and zenity).
    pub const GIVE_UP_SECS: u32 = 600;

    /// The AppleScript run by `osascript`. Every word the user reads comes
    /// in through `argv`, so nothing a client chose is ever parsed as
    /// script: item 1 the text, item 2 the approve label ("" for none).
    pub const MAC_SCRIPT: &[&str] = &[
        "on run argv",
        "activate",
        "if item 2 of argv is \"\" then",
        "set r to display dialog (item 1 of argv) with title \"Roadie\" buttons {\"Decline\"} with icon caution giving up after (item 3 of argv as integer)",
        "else",
        "set r to display dialog (item 1 of argv) with title \"Roadie\" buttons {\"Decline\", (item 2 of argv)} with icon note giving up after (item 3 of argv as integer)",
        "end if",
        "if gave up of r then return \"\"",
        "return button returned of r",
        "end run",
    ];

    /// Arguments for `/usr/bin/osascript`.
    pub fn mac_args(p: &Prompt) -> Vec<String> {
        let mut a: Vec<String> = MAC_SCRIPT.iter().flat_map(|l| ["-e".to_string(), l.to_string()]).collect();
        a.push(p.text());
        a.push(p.approve.clone().unwrap_or_default());
        a.push(GIVE_UP_SECS.to_string());
        a
    }

    pub fn mac_answer(stdout: &str, approve: Option<&str>) -> Answer {
        match stdout.trim() {
            "Decline" => Answer::Decline,
            s if !s.is_empty() && Some(s) == approve => Answer::Approve,
            _ => Answer::Dismissed,
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum LinuxTool {
        Zenity,
        Kdialog,
    }

    /// Arguments for zenity or kdialog. zenity gets `--no-markup`; kdialog
    /// has no such switch, which is why `clean` strips `<` and `>`.
    pub fn linux_args(tool: LinuxTool, p: &Prompt) -> Vec<String> {
        let text = p.text();
        match (tool, &p.approve) {
            (LinuxTool::Zenity, Some(label)) => vec![
                "--question".into(),
                "--title=Roadie".into(),
                "--no-markup".into(),
                "--width=460".into(),
                format!("--timeout={GIVE_UP_SECS}"),
                format!("--ok-label={label}"),
                "--cancel-label=Decline".into(),
                format!("--text={text}"),
            ],
            (LinuxTool::Zenity, None) => vec![
                "--warning".into(),
                "--title=Roadie".into(),
                "--no-markup".into(),
                "--width=460".into(),
                format!("--timeout={GIVE_UP_SECS}"),
                "--ok-label=Decline".into(),
                format!("--text={text}"),
            ],
            (LinuxTool::Kdialog, Some(label)) => vec![
                "--title".into(),
                "Roadie".into(),
                "--yes-label".into(),
                label.clone(),
                "--no-label".into(),
                "Decline".into(),
                "--yesno".into(),
                text,
            ],
            (LinuxTool::Kdialog, None) => vec!["--title".into(), "Roadie".into(), "--sorry".into(), format!("{text}\n\nClosing this declines the request.")],
        }
    }

    /// zenity: 0 the OK button, 1 Cancel *or* the window closed, 5 timeout.
    /// kdialog: 0 yes, 1 no. A blocked prompt's only button declines.
    pub fn linux_answer(tool: LinuxTool, code: Option<i32>, can_approve: bool) -> Answer {
        match (tool, code, can_approve) {
            (_, Some(0), true) => Answer::Approve,
            (_, Some(0), false) => Answer::Decline,
            (LinuxTool::Kdialog, Some(_), false) => Answer::Decline,
            (_, Some(1), true) => Answer::Decline,
            _ => Answer::Dismissed,
        }
    }

    /// Absolute paths only: a program that restarts the service with its
    /// own `PATH` must not substitute a "zenity" that exits 0.
    #[cfg(all(unix, not(target_os = "macos")))]
    fn linux_tool() -> Option<(LinuxTool, &'static str)> {
        const CANDIDATES: &[(LinuxTool, &str)] = &[
            (LinuxTool::Zenity, "/usr/bin/zenity"),
            (LinuxTool::Zenity, "/bin/zenity"),
            (LinuxTool::Zenity, "/run/current-system/sw/bin/zenity"),
            (LinuxTool::Kdialog, "/usr/bin/kdialog"),
            (LinuxTool::Kdialog, "/bin/kdialog"),
            (LinuxTool::Kdialog, "/run/current-system/sw/bin/kdialog"),
        ];
        CANDIDATES.iter().copied().find(|(_, p)| std::path::Path::new(p).is_file())
    }

    /// Can this OS show our dialog at all (given a screen)?
    pub fn available() -> bool {
        #[cfg(target_os = "macos")]
        return std::path::Path::new("/usr/bin/osascript").is_file();
        #[cfg(windows)]
        return true;
        #[cfg(all(unix, not(target_os = "macos")))]
        return linux_tool().is_some();
    }

    /// Show `p` and block until it is answered, dismissed or times out.
    pub fn show(p: &Prompt) -> Result<Answer, String> {
        #[cfg(target_os = "macos")]
        {
            let out = std::process::Command::new("/usr/bin/osascript")
                .args(mac_args(p))
                .stdin(std::process::Stdio::null())
                .output()
                .map_err(|e| format!("run osascript: {e}"))?;
            if !out.status.success() {
                return Err(format!("osascript: {}", String::from_utf8_lossy(&out.stderr).trim()));
            }
            Ok(mac_answer(&String::from_utf8_lossy(&out.stdout), p.approve.as_deref()))
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let (tool, path) = linux_tool().ok_or("neither zenity nor kdialog is installed")?;
            let status = std::process::Command::new(path)
                .args(linux_args(tool, p))
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map_err(|e| format!("run {path}: {e}"))?;
            Ok(linux_answer(tool, status.code(), p.approve.is_some()))
        }
        #[cfg(windows)]
        {
            windows_box(p)
        }
    }

    /// The text of a Windows message box, whose buttons are always Yes/No
    /// (or OK), so the question spells out what Yes does.
    pub fn windows_text(p: &Prompt) -> String {
        match &p.approve {
            Some(label) => format!("{}\n\n{}\nYes: {}.   No: decline.", p.text(), p.question, label.to_lowercase()),
            None => format!("{}\n\nOK declines the request.", p.text()),
        }
    }

    #[cfg(windows)]
    fn windows_box(p: &Prompt) -> Result<Answer, String> {
        use std::ffi::c_void;
        #[link(name = "user32")]
        unsafe extern "system" {
            fn MessageBoxW(hwnd: *mut c_void, text: *const u16, caption: *const u16, kind: u32) -> i32;
        }
        const MB_OK: u32 = 0x0;
        const MB_YESNO: u32 = 0x4;
        const MB_ICONWARNING: u32 = 0x30;
        const MB_ICONQUESTION: u32 = 0x20;
        const MB_DEFBUTTON2: u32 = 0x100;
        const MB_SETFOREGROUND: u32 = 0x10000;
        const MB_TOPMOST: u32 = 0x40000;
        const IDOK: i32 = 1;
        const IDYES: i32 = 6;
        const IDNO: i32 = 7;
        let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
        let text = wide(&windows_text(p));
        let title = wide("Roadie");
        let kind = if p.approve.is_some() { MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2 } else { MB_OK | MB_ICONWARNING };
        let r = unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), kind | MB_SETFOREGROUND | MB_TOPMOST) };
        match (r, p.approve.is_some()) {
            (0, _) => Err(format!("MessageBoxW failed: {}", std::io::Error::last_os_error())),
            (IDYES, true) => Ok(Answer::Approve),
            (IDNO, true) | (IDOK, false) => Ok(Answer::Decline),
            _ => Ok(Answer::Dismissed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(kind: RequestKind, by: &str) -> Request {
        Request { id: "r1".into(), kind, requested_by: by.into(), status: RequestStatus::Pending, created_at: 0, error: None, progress: None }
    }

    fn slskd() -> Recipe {
        recipe::load_builtin().into_iter().find(|r| r.name == "slskd").unwrap()
    }

    fn install(config: Map<String, Value>, secret_keys: Vec<String>, consumer: Option<&str>) -> RequestKind {
        RequestKind::Install { tool: "slskd".into(), consumer: consumer.map(str::to_string), config, secrets: Map::new(), secret_keys, recipe: None, recipe_change: None }
    }

    #[test]
    fn surface_needs_a_screen_and_prefers_the_window() {
        assert_eq!(pick(false, true, true), Surface::Terminal, "no screen: only a terminal can answer");
        assert_eq!(pick(true, true, false), Surface::Window);
        assert_eq!(pick(true, false, true), Surface::Dialog);
        assert_eq!(pick(true, false, false), Surface::Terminal, "a screen but no dialog program");
    }

    #[test]
    fn client_text_is_shown_never_interpreted() {
        assert_eq!(clean("Evil<b>App</b>\n\u{202E}gpj.exe", 60), "Evil‹b›App‹/b› gpj.exe");
        assert_eq!(clean("  a\tb  ", 60), "a b");
        assert_eq!(clean("abcdef", 3), "abc…");
    }

    #[test]
    fn install_prompt_lists_decisions_and_blocks_on_missing_required_values() {
        // Stock slskd: the Soulseek account is optional, so an empty request
        // is approvable and says what stays empty.
        let recipe = slskd();
        let p = describe(&request(install(Map::new(), vec![], None), "Viboplr"), Some(&recipe), &Map::new(), &[]);
        assert_eq!(p.headline, "Viboplr asks to install slskd");
        assert_eq!(p.approve.as_deref(), Some("Install"), "{p:?}");
        assert!(p.text().contains("Left empty:"), "{}", p.text());
        assert!(p.text().contains("Start now: yes"), "engine choices show their defaults: {}", p.text());

        // A required field with no value: it cannot be approved from here.
        let mut strict = recipe.clone();
        let field = strict.config.iter_mut().find(|f| f.key == "soulseekUsername").unwrap();
        field.required = true;
        let p = describe(&request(install(Map::new(), vec![], None), "Viboplr"), Some(&strict), &Map::new(), &[]);
        assert!(p.approve.is_none(), "{p:?}");
        let blocked = p.blocked.as_deref().unwrap();
        assert!(blocked.contains("soulseekUsername") && blocked.contains("--set key=value"), "names the missing key: {blocked}");
        assert!(!p.text().contains('<'), "no markup-looking text for kdialog: {}", p.text());

        // Everything supplied: approvable, secrets never shown.
        let mut config = Map::new();
        let mut secrets = vec![];
        for f in recipe.install_fields() {
            if f.secret {
                secrets.push(f.key.clone());
            } else {
                config.insert(f.key.clone(), Value::String("bj".into()));
            }
        }
        let p = describe(&request(install(config, secrets, Some("viboplr")), "Viboplr"), Some(&recipe), &Map::new(), &[]);
        assert_eq!(p.approve.as_deref(), Some("Install"), "{p:?}");
        assert_eq!(p.headline, "Viboplr asks to install slskd and connect to it");
        let text = p.text();
        assert!(text.contains("provided") && text.contains("its own access key"), "{text}");
        assert!(text.contains("github.com/"), "says where the download comes from: {text}");
        assert!(text.contains("cannot verify"), "the name is the client's claim: {text}");
    }

    #[test]
    fn a_missing_required_value_fails_the_request_where_nothing_can_ask_for_it() {
        let mut strict = slskd();
        strict.config.iter_mut().find(|f| f.key == "soulseekUsername").unwrap().required = true;
        let none = Map::new();
        for surface in [Surface::Dialog, Surface::Terminal] {
            let (msg, keys) = refuse_missing(&strict, &none, &none, surface).expect("refused");
            assert_eq!(keys, vec!["soulseekUsername".to_string()]);
            assert!(msg.contains("/config/soulseekUsername") && msg.contains("--set soulseekUsername="), "pointer and fix: {msg}");
        }
        assert!(refuse_missing(&strict, &none, &none, Surface::Window).is_none(), "the window's form asks for it");
        let mut given = Map::new();
        given.insert("soulseekUsername".into(), Value::String("bj".into()));
        assert!(refuse_missing(&strict, &given, &none, Surface::Dialog).is_none());
        let mut blank = Map::new();
        blank.insert("soulseekUsername".into(), Value::String("".into()));
        assert!(refuse_missing(&strict, &blank, &none, Surface::Dialog).is_some(), "an empty string is not a value");
        let mut stored = Map::new();
        stored.insert("soulseekUsername".into(), Value::String("kept".into()));
        assert!(refuse_missing(&strict, &none, &stored, Surface::Dialog).is_none(), "a reinstall keeps the stored value");
        assert!(refuse_missing(&slskd(), &none, &none, Surface::Dialog).is_none(), "stock slskd requires nothing");
    }

    #[test]
    fn a_brought_recipe_is_reviewed_in_the_prompt_itself() {
        let current = slskd();
        let mut theirs = current.clone();
        theirs.summary = "their slskd".into();
        theirs.author = "Viboplr <script>".into();
        theirs.revision = 7;
        let kind = RequestKind::Install {
            tool: "slskd".into(),
            consumer: None,
            config: Map::new(),
            secrets: Map::new(),
            secret_keys: vec![],
            recipe: Some(Box::new(theirs.clone())),
            recipe_change: Some(Change::ReplacesBuiltin),
        };
        let review = review_lines(&theirs, Some(&current), Err("offline"));
        assert!(review[0].contains("summary"), "names what changed: {review:?}");
        let p = describe(&request(kind, "Viboplr"), Some(&current), &Map::new(), &review);
        let text = p.text();
        assert_eq!(p.approve.as_deref(), Some("Trust and install"));
        assert!(text.contains("replacing Roadie's built-in one") && text.contains("revision 7"), "{text}");
        assert!(text.contains("their slskd"), "the brought recipe is what is described: {text}");
        assert!(text.contains("github.com/slskd/slskd") && text.contains("Could not preview it: offline"), "{text}");
        assert!(!text.contains('<'), "author text is cleaned: {text}");

        let replace = RequestKind::ReplaceRecipe { tool: "slskd".into(), recipe: Box::new(theirs), recipe_change: Change::ChangesTrusted };
        let p = describe(&request(replace, "Viboplr"), Some(&current), &Map::new(), &[]);
        assert_eq!((p.headline.as_str(), p.approve.as_deref()), ("Viboplr asks to change slskd's recipe", Some("Trust and update")));
        assert!(p.text().contains("replacing the one you trusted"));
    }

    #[test]
    fn stored_values_settle_a_reinstall_and_untrusted_recipes_only_decline() {
        let recipe = slskd();
        let mut stored = Map::new();
        for f in recipe.install_fields() {
            if f.secret {
                stored.insert(format!("has_{}", f.key), Value::Bool(true));
            } else {
                stored.insert(f.key.clone(), Value::String("kept".into()));
            }
        }
        let p = describe(&request(install(Map::new(), vec![], None), "a local API client"), Some(&recipe), &stored, &[]);
        assert!(p.blocked.is_none(), "{p:?}");

        let p = describe(&request(install(Map::new(), vec![], None), "x"), None, &Map::new(), &[]);
        assert!(p.approve.is_none() && p.blocked.is_some());

        let p = describe(&request(RequestKind::Connect { consumer: "viboplr".into(), tool: "slskd".into(), return_url: None }, "Viboplr"), Some(&recipe), &Map::new(), &[]);
        assert_eq!((p.question.as_str(), p.approve.as_deref()), ("Let Viboplr connect to slskd?", Some("Allow")));
        let p = describe(&request(RequestKind::Uninstall { tool: "slskd".into(), keep_data: true }, "CLI"), Some(&recipe), &Map::new(), &[]);
        assert_eq!(p.approve.as_deref(), Some("Remove"));
    }

    #[test]
    fn dialogs_take_client_text_as_arguments_not_script() {
        let p = Prompt { headline: "\" & do shell script \"touch /tmp/x".into(), lines: vec![], question: "Install x?".into(), approve: Some("Install".into()), blocked: None };
        let a = dialog::mac_args(&p);
        let script: Vec<&String> = a.iter().skip(1).step_by(2).take(dialog::MAC_SCRIPT.len()).collect();
        assert!(script.iter().all(|l| dialog::MAC_SCRIPT.contains(&l.as_str())), "the script is fixed");
        assert_eq!(a[a.len() - 3], p.text(), "the text is an argument");
        assert_eq!(dialog::mac_answer("Install\n", Some("Install")), dialog::Answer::Approve);
        assert_eq!(dialog::mac_answer("Decline\n", Some("Install")), dialog::Answer::Decline);
        assert_eq!(dialog::mac_answer("\n", Some("Install")), dialog::Answer::Dismissed, "gave up");
        assert_eq!(dialog::mac_answer("Install\n", None), dialog::Answer::Dismissed, "a blocked prompt cannot approve");

        let z = dialog::linux_args(dialog::LinuxTool::Zenity, &p);
        assert!(z.contains(&"--no-markup".to_string()) && z.contains(&"--ok-label=Install".to_string()));
        use dialog::{linux_answer, Answer, LinuxTool::*};
        assert_eq!(linux_answer(Zenity, Some(0), true), Answer::Approve);
        assert_eq!(linux_answer(Zenity, Some(1), true), Answer::Decline);
        assert_eq!(linux_answer(Zenity, Some(5), true), Answer::Dismissed, "timeout");
        assert_eq!(linux_answer(Zenity, Some(0), false), Answer::Decline, "a blocked prompt's button declines");
        assert_eq!(linux_answer(Kdialog, Some(1), false), Answer::Decline);
        assert_eq!(linux_answer(Zenity, None, true), Answer::Dismissed, "killed");
        assert!(dialog::windows_text(&p).contains("Yes: install."));
    }
}
