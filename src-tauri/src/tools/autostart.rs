//! Per-user login items.
//!
//! - **A daemon's own item** (macOS): while a tool's "start at login"
//!   is on, a login item runs the daemon's binary itself with the command
//!   Roadie would start it with (`tool-<name>`). The OS then names the
//!   daemon ("slskd can run in the background") and the switch in Login
//!   Items is that tool's. The engine rewrites it whenever the command
//!   changes (`tools::sync_login_item`).
//! - **The Roadie service** (`roadie --serve --data-dir <dir>`, desktop
//!   release) while "run in the background" is on.
//! - **The CLI's `maintain` item** (Windows): a Run value starting a console
//!   daemon would open a console window at every login, so there Roadie's
//!   own item starts the daemons in its reconcile.
//!
//! The older per-tool items (`roadie --start-tool …`) are removed when found.
//!
//! No crate: a LaunchAgent plist is a hand-built XML string, the Windows Run
//! key goes through `reg.exe`. macOS and Windows only.

use super::process::SpawnPlan;
use std::path::{Path, PathBuf};

pub const IDENTIFIER: &str = "com.outcast1000.roadie";

/// The executable Roadie's own login items run.
pub fn launcher_exe() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|e| format!("current_exe: {e}"))
}

pub fn launcher_args(name: &str, data_root: &Path) -> Vec<String> {
    vec![
        "--start-tool".to_string(),
        name.to_string(),
        "--data-dir".to_string(),
        data_root.to_string_lossy().into_owned(),
    ]
}

pub fn label(name: &str) -> String {
    format!("{IDENTIFIER}.{name}")
}

/// The item name for the service: `service` for the default data dir
/// (`com.outcast1000.roadie.service`), `service-<hash>` for any other, so a
/// service on a test or secondary data dir never touches the real item.
pub fn service_item(data_root: &Path) -> String {
    item_for("service", data_root)
}

fn item_for(prefix: &str, data_root: &Path) -> String {
    if data_root == crate::paths::default_data_root() {
        return prefix.into();
    }
    use sha2::{Digest, Sha256};
    let h = Sha256::digest(data_root.to_string_lossy().as_bytes());
    format!("{prefix}-{:08x}", u32::from_be_bytes(h[..4].try_into().unwrap()))
}

/// The CLI release's one login item: `roadie --data-dir <dir> maintain --at-login`,
/// which starts the daemons marked "start at login", runs the daily update
/// pass, and exits. Named `cli` for the CLI's default data dir and
/// `cli-<hash>` for any other, so each app bundling Roadie with its own
/// data dir has its own item. It points at the binary's current path; if
/// the binary moves, the next command run from the new place rewrites it.
pub fn maintain_item(data_root: &Path) -> String {
    item_for("cli", data_root)
}

pub fn enable_maintain(data_root: &Path) -> Result<(), String> {
    let exe = launcher_exe()?;
    let args = vec!["--data-dir".to_string(), data_root.to_string_lossy().into_owned(), "maintain".to_string(), "--at-login".to_string()];
    enable_with(&maintain_item(data_root), &exe, &args, &data_root.join("logs").join("roadie-maintain.log"))
}

pub fn disable_maintain(data_root: &Path) -> Result<(), String> {
    disable(&maintain_item(data_root))
}

pub fn maintain_enabled(data_root: &Path) -> bool {
    is_enabled(&maintain_item(data_root))
}

/// Register (or rewrite, after the app moved) the service's login item.
#[cfg(feature = "service")]
pub fn enable_service(data_root: &Path) -> Result<(), String> {
    let exe = launcher_exe()?;
    let args = crate::service::service_args(data_root);
    let log = data_root.join("logs").join(crate::service::SERVICE_LOG);
    enable_with(&service_item(data_root), &exe, &args, &log)
}

#[cfg(feature = "service")]
pub fn disable_service(data_root: &Path) -> Result<(), String> {
    disable(&service_item(data_root))
}

#[cfg(feature = "service")]
pub fn service_enabled(data_root: &Path) -> bool {
    is_enabled(&service_item(data_root))
}

// --- A daemon's own login item ---

/// Daemons get login items of their own on macOS; on Windows Roadie's item
/// starts them.
pub const NATIVE_TOOL_ITEMS: bool = cfg!(target_os = "macos");

/// `tool-<name>` for the default data dir, `tool-<name>-<hash>` for any
/// other, like the service's item.
pub fn tool_item(name: &str, data_root: &Path) -> String {
    item_for(&format!("tool-{name}"), data_root)
}

pub fn tool_enabled(name: &str, data_root: &Path) -> bool {
    is_enabled(&tool_item(name, data_root))
}

/// Write the daemon's login item for `plan`, leaving an unchanged one alone.
/// It is **not** loaded now: launchd reads it at the next login, and loading
/// a `RunAtLoad` item would start a second copy of a daemon Roadie runs.
pub fn enable_tool(name: &str, data_root: &Path, plan: &SpawnPlan) -> Result<(), String> {
    let item = tool_item(name, data_root);
    // launchd opens the log without creating its folder, and a tool never
    // started has none yet.
    if let Some(dir) = plan.log.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    #[cfg(target_os = "macos")]
    {
        write_if_changed(&plist_path(&item)?, &render_tool_plist(&label(&item), plan))
    }
    #[cfg(windows)]
    {
        let _ = (item, plan);
        Err("a daemon's own login item is not used on Windows".into())
    }
}

/// Remove the daemon's login item. The file only: booting a loaded item out
/// of launchd would stop the daemon it started, and turning "start at login"
/// off must leave a running daemon running.
pub fn disable_tool(name: &str, data_root: &Path) -> Result<(), String> {
    let item = tool_item(name, data_root);
    #[cfg(target_os = "macos")]
    {
        let path = plist_path(&item)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("remove {}: {e}", path.display())),
        }
    }
    #[cfg(windows)]
    {
        let _ = item;
        Ok(())
    }
}

/// The pid of the daemon its login item started, when launchd knows one
/// (the item is loaded from the login on). Such a daemon has no pid
/// file, so this is how Roadie finds it to stop it and does not start a
/// second copy.
pub fn tool_item_pid(name: &str, data_root: &Path) -> Option<u32> {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("/bin/launchctl").args(["list", &label(&tool_item(name, data_root))]).output().ok()?;
        if !out.status.success() {
            return None;
        }
        parse_launchctl_pid(&String::from_utf8_lossy(&out.stdout))
    }
    #[cfg(windows)]
    {
        let _ = (name, data_root);
        None
    }
}

/// `"PID" = 123;` out of `launchctl list <label>` (absent while the job is
/// not running).
#[cfg_attr(windows, allow(dead_code))]
pub fn parse_launchctl_pid(text: &str) -> Option<u32> {
    text.lines().find_map(|l| l.trim().strip_prefix("\"PID\" = ")?.trim_end_matches(';').trim().parse().ok())
}

#[cfg(target_os = "macos")]
fn write_if_changed(path: &Path, contents: &str) -> Result<(), String> {
    if std::fs::read_to_string(path).ok().as_deref() == Some(contents) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    std::fs::write(path, contents).map_err(|e| format!("write {}: {e}", path.display()))
}

/// Where a plan runs: its `cwd`, else the binary's folder (as `spawn_detached`).
fn plan_cwd(plan: &SpawnPlan) -> PathBuf {
    plan.cwd.clone().or_else(|| plan.exe.parent().map(Path::to_path_buf)).unwrap_or_default()
}

/// A daemon's LaunchAgent: its binary and args, env and working dir, output
/// appended to the tool's log. `KeepAlive` false: Roadie's Stop must stick.
pub fn render_tool_plist(label: &str, plan: &SpawnPlan) -> String {
    let mut prog = format!("    <string>{}</string>\n", xml_escape(&plan.exe.to_string_lossy()));
    for a in &plan.args {
        prog.push_str(&format!("    <string>{}</string>\n", xml_escape(a)));
    }
    let mut env = String::new();
    if !plan.env.is_empty() {
        env.push_str("  <key>EnvironmentVariables</key>\n  <dict>\n");
        for (k, v) in &plan.env {
            env.push_str(&format!("    <key>{}</key><string>{}</string>\n", xml_escape(k), xml_escape(v)));
        }
        env.push_str("  </dict>\n");
    }
    let cwd = xml_escape(&plan_cwd(plan).to_string_lossy());
    let log = xml_escape(&plan.log.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array>
{prog}  </array>
{env}  <key>WorkingDirectory</key><string>{cwd}</string>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><false/>
  <key>ProcessType</key><string>Standard</string>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#
    )
}

pub fn is_enabled(name: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        plist_path(name).map(|p| p.is_file()).unwrap_or(false)
    }
    #[cfg(windows)]
    {
        run_key_exists(name)
    }
}

/// The launcher path the current login item runs, when readable — used to
/// rewrite the item after the app moved.
pub fn recorded_launcher(name: &str) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let text = std::fs::read_to_string(plist_path(name).ok()?).ok()?;
        let start = text.find("<array>")? + "<array>".len();
        let rest = &text[start..];
        let s = rest.find("<string>")? + "<string>".len();
        let e = rest[s..].find("</string>")? + s;
        Some(PathBuf::from(xml_unescape(&rest[s..e])))
    }
    #[cfg(windows)]
    {
        let out = reg(&["query", RUN_KEY, "/v", &value_name(name)]).ok()?;
        let line = out.lines().find(|l| l.contains("REG_SZ"))?;
        let data = line.split("REG_SZ").nth(1)?.trim();
        let exe = data.strip_prefix('"')?.split('"').next()?;
        Some(PathBuf::from(exe))
    }
}

/// Legacy per-tool item (kept so old items can be rewritten or removed).
pub fn enable(name: &str, data_root: &Path, log_dir: &Path) -> Result<(), String> {
    let exe = launcher_exe()?;
    let args = launcher_args(name, data_root);
    enable_with(name, &exe, &args, &log_dir.join(super::process::LAUNCHER_LOG))
}

fn enable_with(name: &str, exe: &Path, args: &[String], log: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        enable_macos(name, exe, args, log)
    }
    #[cfg(windows)]
    {
        let _ = log;
        enable_windows(name, exe, args)
    }
}

pub fn disable(name: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        disable_macos(name)
    }
    #[cfg(windows)]
    {
        disable_windows(name)
    }
}

// --- macOS ---

#[cfg(target_os = "macos")]
fn plist_path(name: &str) -> Result<PathBuf, String> {
    Ok(crate::paths::home_dir().join("Library").join("LaunchAgents").join(format!("{}.plist", label(name))))
}

pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

#[allow(dead_code)]
fn xml_unescape(s: &str) -> String {
    s.replace("&quot;", "\"").replace("&gt;", ">").replace("&lt;", "<").replace("&amp;", "&")
}

/// `KeepAlive` false (it would fight the Stop button); `AbandonProcessGroup`
/// true so launchd does not kill the daemon when the launcher exits.
pub fn render_plist(label: &str, exe: &Path, args: &[String], log: &Path) -> String {
    let mut prog = format!("    <string>{}</string>\n", xml_escape(&exe.to_string_lossy()));
    for a in args {
        prog.push_str(&format!("    <string>{}</string>\n", xml_escape(a)));
    }
    let log = xml_escape(&log.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array>
{prog}  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><false/>
  <key>AbandonProcessGroup</key><true/>
  <key>ProcessType</key><string>Background</string>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#
    )
}

#[cfg(target_os = "macos")]
fn launchctl(args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("/bin/launchctl").args(args).output().map_err(|e| format!("launchctl: {e}"))?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if out.status.success() {
        Ok(text)
    } else {
        Err(format!("launchctl {} failed: {}", args.join(" "), text.trim()))
    }
}

#[cfg(target_os = "macos")]
fn enable_macos(name: &str, exe: &Path, args: &[String], log: &Path) -> Result<(), String> {
    let path = plist_path(name)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    if let Some(dir) = log.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let contents = render_plist(&label(name), exe, args, log);
    // Unchanged item: leave launchd alone (a bootout/bootstrap would start a
    // second instance that only exits again).
    if std::fs::read_to_string(&path).ok().as_deref() == Some(contents.as_str()) {
        return Ok(());
    }
    let domain = format!("gui/{}", unsafe { libc::getuid() });
    let target = format!("{domain}/{}", label(name));
    let _ = launchctl(&["bootout", &target]);
    std::fs::write(&path, contents).map_err(|e| format!("write {}: {e}", path.display()))?;
    launchctl(&["bootstrap", &domain, &path.to_string_lossy()])?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn disable_macos(name: &str) -> Result<(), String> {
    let path = plist_path(name)?;
    let target = format!("gui/{}/{}", unsafe { libc::getuid() }, label(name));
    let _ = launchctl(&["bootout", &target]);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("remove {}: {e}", path.display())),
    }
}

// --- Windows ---

#[cfg(windows)]
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

#[cfg(windows)]
fn value_name(name: &str) -> String {
    format!("Roadie {name}")
}

#[cfg(windows)]
fn reg(args: &[&str]) -> Result<String, String> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("reg.exe")
        .args(args)
        .creation_flags(super::process::CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("reg.exe: {e}"))?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if out.status.success() {
        Ok(text)
    } else {
        Err(format!("reg {} failed: {}", args.join(" "), text.trim()))
    }
}

#[cfg(windows)]
fn run_key_exists(name: &str) -> bool {
    reg(&["query", RUN_KEY, "/v", &value_name(name)]).is_ok()
}

/// Compiled everywhere so the quoting test runs on every CI runner.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn windows_command_line(exe: &Path, args: &[String]) -> String {
    let mut s = format!("\"{}\"", exe.to_string_lossy());
    for a in args {
        if a.contains(' ') {
            s.push_str(&format!(" \"{a}\""));
        } else {
            s.push_str(&format!(" {a}"));
        }
    }
    s
}

#[cfg(windows)]
fn enable_windows(name: &str, exe: &Path, args: &[String]) -> Result<(), String> {
    let data = windows_command_line(exe, args);
    reg(&["add", RUN_KEY, "/v", &value_name(name), "/t", "REG_SZ", "/d", &data, "/f"]).map(|_| ())
}

#[cfg(windows)]
fn disable_windows(name: &str) -> Result<(), String> {
    if !run_key_exists(name) {
        return Ok(());
    }
    reg(&["delete", RUN_KEY, "/v", &value_name(name), "/f"]).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_escapes_paths_and_carries_abandon_process_group() {
        let args = launcher_args("slskd", Path::new("/Users/a b/Library/App & Co"));
        let plist = render_plist(&label("slskd"), Path::new("/Applications/Roadie.app/Contents/MacOS/roadie"), &args, Path::new("/tmp/launcher.log"));
        assert!(plist.contains("<string>/Users/a b/Library/App &amp; Co</string>"));
        assert!(plist.contains("<string>--start-tool</string>\n    <string>slskd</string>"));
        assert!(plist.contains("<key>AbandonProcessGroup</key><true/>"));
        assert!(plist.contains("<key>KeepAlive</key><false/>"));
        assert!(plist.contains("com.outcast1000.roadie.slskd"));
    }

    fn plan() -> SpawnPlan {
        SpawnPlan {
            exe: PathBuf::from("/Users/a b/Roadie/tools/slskd/versions/0.22.5/slskd"),
            args: vec!["--config".into(), "/Users/a b/data/slskd.yml".into()],
            env: vec![("DOTNET_NOLOGO".into(), "1".into()), ("X".into(), "a&b".into())],
            cwd: None,
            log: PathBuf::from("/Users/a b/logs/stdout.log"),
            append_log: false,
        }
    }

    #[test]
    fn a_tools_own_plist_runs_the_daemon_itself() {
        let root = Path::new("/somewhere/else");
        let item = tool_item("slskd", root);
        assert!(item.starts_with("tool-slskd-"), "a non-default data dir gets its own item: {item}");
        let plist = render_tool_plist(&label(&item), &plan());
        assert!(plist.contains("<array>\n    <string>/Users/a b/Roadie/tools/slskd/versions/0.22.5/slskd</string>\n    <string>--config</string>"), "{plist}");
        assert!(plist.contains("  <key>EnvironmentVariables</key>\n  <dict>\n    <key>DOTNET_NOLOGO</key><string>1</string>\n    <key>X</key><string>a&amp;b</string>\n  </dict>\n"), "{plist}");
        assert!(plist.contains("<key>WorkingDirectory</key><string>/Users/a b/Roadie/tools/slskd/versions/0.22.5</string>"), "the binary's folder, as spawn_detached");
        assert!(plist.contains("<key>KeepAlive</key><false/>"));
        assert!(plist.contains("<key>StandardOutPath</key><string>/Users/a b/logs/stdout.log</string>"));
        assert!(!plist.contains("roadie</string>\n"), "no Roadie binary in between");
        let mut bare = plan();
        bare.env.clear();
        assert!(!render_tool_plist("l", &bare).contains("EnvironmentVariables"));

        // launchd skips a malformed plist without a word; plutil is the judge.
        #[cfg(target_os = "macos")]
        {
            let f = std::env::temp_dir().join(format!("roadie-tool-plist-{}.plist", std::process::id()));
            std::fs::write(&f, &plist).unwrap();
            let out = std::process::Command::new("/usr/bin/plutil").arg("-lint").arg(&f).output().unwrap();
            let _ = std::fs::remove_file(&f);
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
        }
    }

    #[test]
    fn launchctl_list_pid_is_read_only_while_running() {
        let running = "{\n\t\"LimitLoadToSessionType\" = \"Aqua\";\n\t\"Label\" = \"com.outcast1000.roadie.tool-slskd\";\n\t\"PID\" = 4242;\n\t\"Program\" = \"/x/slskd\";\n};\n";
        assert_eq!(parse_launchctl_pid(running), Some(4242));
        let exited = "{\n\t\"Label\" = \"com.outcast1000.roadie.tool-slskd\";\n\t\"LastExitStatus\" = 0;\n};\n";
        assert_eq!(parse_launchctl_pid(exited), None);
    }

    #[test]
    fn windows_command_lines_quote_spaces() {
        let exe = Path::new(r"C:\Program Files\Roadie\roadie.exe");
        let args = vec!["--start-tool".into(), "slskd".into(), "--data-dir".into(), r"C:\Users\x y\AppData".into()];
        assert_eq!(
            windows_command_line(exe, &args),
            r#""C:\Program Files\Roadie\roadie.exe" --start-tool slskd --data-dir "C:\Users\x y\AppData""#
        );
    }
}
