//! Per-user login items.
//!
//! One item runs **the Roadie service** (`roadie --serve --data-dir <dir>`)
//! when "run in the background" is on; the service's startup reconcile then
//! starts every daemon marked "start at login". Nothing ever runs a daemon
//! by its own path from a login item. The older per-tool items
//! (`roadie --start-tool …`) are removed when found.
//!
//! No crate: a LaunchAgent plist is a hand-built XML string, the Windows Run
//! key goes through `reg.exe`, Linux gets an XDG autostart `.desktop`.

use std::path::{Path, PathBuf};

pub const IDENTIFIER: &str = "com.outcast1000.roadie";

/// The executable login items should run. Under an AppImage the mounted
/// path vanishes at the next boot; `$APPIMAGE` is the stable one.
pub fn launcher_exe() -> Result<PathBuf, String> {
    if let Some(appimage) = std::env::var_os("APPIMAGE") {
        return Ok(PathBuf::from(appimage));
    }
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
    if data_root == crate::paths::default_data_root() {
        return "service".into();
    }
    use sha2::{Digest, Sha256};
    let h = Sha256::digest(data_root.to_string_lossy().as_bytes());
    format!("service-{:08x}", u32::from_be_bytes(h[..4].try_into().unwrap()))
}

/// Register (or rewrite, after the app moved) the service's login item.
pub fn enable_service(data_root: &Path) -> Result<(), String> {
    let exe = launcher_exe()?;
    let args = crate::service::service_args(data_root);
    let log = data_root.join("logs").join(crate::service::SERVICE_LOG);
    enable_with(&service_item(data_root), "Roadie", &exe, &args, &log)
}

pub fn disable_service(data_root: &Path) -> Result<(), String> {
    disable(&service_item(data_root))
}

pub fn service_enabled(data_root: &Path) -> bool {
    is_enabled(&service_item(data_root))
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
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        desktop_path(name).map(|p| p.is_file()).unwrap_or(false)
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
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let text = std::fs::read_to_string(desktop_path(name).ok()?).ok()?;
        let exec = text.lines().find_map(|l| l.strip_prefix("Exec="))?;
        let exe = exec.strip_prefix('"')?.split('"').next()?;
        Some(PathBuf::from(exe.replace("\\\"", "\"").replace("\\\\", "\\")))
    }
}

/// Legacy per-tool item (kept so old items can be rewritten or removed).
pub fn enable(name: &str, display_name: &str, data_root: &Path, log_dir: &Path) -> Result<(), String> {
    let exe = launcher_exe()?;
    let args = launcher_args(name, data_root);
    enable_with(name, display_name, &exe, &args, &log_dir.join(super::process::LAUNCHER_LOG))
}

fn enable_with(name: &str, display_name: &str, exe: &Path, args: &[String], log: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let _ = display_name;
        enable_macos(name, exe, args, log)
    }
    #[cfg(windows)]
    {
        let _ = (log, display_name);
        enable_windows(name, exe, args)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = log;
        enable_linux(name, display_name, exe, args)
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
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let path = desktop_path(name)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("remove {}: {e}", path.display())),
        }
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

// --- Linux (XDG autostart) ---

#[cfg(all(unix, not(target_os = "macos")))]
fn desktop_path(name: &str) -> Result<PathBuf, String> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or(crate::paths::home_dir().join(".config"));
    Ok(config.join("autostart").join(format!("roadie-{name}.desktop")))
}

#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
pub fn desktop_exec(exe: &Path, args: &[String]) -> String {
    let q = |s: &str| {
        let mut out = String::from("\"");
        for c in s.chars() {
            if matches!(c, '"' | '`' | '$' | '\\') {
                out.push('\\');
            }
            out.push(c);
        }
        out.push('"');
        out
    };
    let mut s = q(&exe.to_string_lossy());
    for a in args {
        s.push(' ');
        s.push_str(&q(a));
    }
    s
}

#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
pub fn render_desktop(name: &str, display_name: &str, exe: &Path, args: &[String]) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=Roadie {name} launcher\nComment=Starts {display_name} (managed by Roadie)\nExec={}\nTerminal=false\nNoDisplay=true\nX-GNOME-Autostart-enabled=true\n",
        desktop_exec(exe, args)
    )
}

#[cfg(all(unix, not(target_os = "macos")))]
fn enable_linux(name: &str, display_name: &str, exe: &Path, args: &[String]) -> Result<(), String> {
    let path = desktop_path(name)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    std::fs::write(&path, render_desktop(name, display_name, exe, args)).map_err(|e| format!("write {}: {e}", path.display()))
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

    #[test]
    fn windows_and_desktop_command_lines_quote_spaces() {
        let exe = Path::new(r"C:\Program Files\Roadie\roadie.exe");
        let args = vec!["--start-tool".into(), "slskd".into(), "--data-dir".into(), r"C:\Users\x y\AppData".into()];
        assert_eq!(
            windows_command_line(exe, &args),
            r#""C:\Program Files\Roadie\roadie.exe" --start-tool slskd --data-dir "C:\Users\x y\AppData""#
        );
        let d = desktop_exec(Path::new("/opt/roadie"), &["a\"b".into(), "$HOME".into()]);
        assert_eq!(d, r#""/opt/roadie" "a\"b" "\$HOME""#);
    }
}
