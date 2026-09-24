//! Resolve, download, verify and lay out a release; swap versions safely.
//!
//! Versions live side by side under `tools/<name>/versions/<version>/`;
//! `current` is a text file naming the live one (a symlink would not survive
//! Windows). A newer version is *staged* until the daemon is idle or stopped
//! — an update must never restart a tool mid-work.
//!
//! Verification is whatever the recipe's source offers — a sums file, a
//! sidecar digest — plus, always, running the extracted binary's version
//! flag and requiring it to agree with the resolved version. For sources
//! that publish no checksums that run is the strongest check available.

use crate::paths::{self, ToolPaths};
use crate::recipe::{self, Archive, Checksums, Platform, Recipe, Source};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const CURRENT_FILE: &str = "current";
pub const STAMP_FILE: &str = ".roadie-install.json";
const LATEST_TTL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallStamp {
    pub version: String,
    pub archive_sha256: String,
    pub installed_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Phase {
    Downloading,
    Extracting,
    Verifying,
}

/// The latest release, resolved for this platform.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Resolved {
    pub version: String,
    pub download_url: String,
    pub asset: String,
    pub checksums_url: Option<String>,
    /// `floating:<tag>` sources: the version is the tag; updates are
    /// detected by digest, not by number.
    pub floating: bool,
}

/// 24h cache of resolutions, failures included — a flaky network must not
/// hammer upstream.
#[derive(Default)]
pub struct LatestCache {
    entries: Mutex<HashMap<String, (Instant, Option<Resolved>)>>,
}

impl LatestCache {
    pub fn get(&self, name: &str) -> Option<Option<Resolved>> {
        let map = self.entries.lock().unwrap();
        let (at, v) = map.get(name)?;
        (at.elapsed() < LATEST_TTL).then(|| v.clone())
    }
    pub fn set(&self, name: &str, v: Option<Resolved>) {
        self.entries.lock().unwrap().insert(name.to_string(), (Instant::now(), v));
    }
    pub fn invalidate(&self, name: &str) {
        self.entries.lock().unwrap().remove(name);
    }
}

fn http_client(timeout: Duration, follow: bool) -> Result<reqwest::blocking::Client, String> {
    let mut b = reqwest::blocking::Client::builder().user_agent("Roadie").timeout(timeout);
    if !follow {
        b = b.redirect(reqwest::redirect::Policy::none());
    }
    b.build().map_err(|e| format!("HTTP client error: {e}"))
}

/// Tag out of a `.../releases/tag/<tag>` redirect target.
pub fn parse_latest_tag_from_location(location: &str) -> Option<String> {
    let tag = location.rsplit_once("/releases/tag/")?.1;
    let tag = tag.split(['?', '#']).next()?.trim_end_matches('/');
    (!tag.is_empty()).then(|| tag.to_string())
}

fn redirect_location(url: &str) -> Result<String, String> {
    let resp = http_client(Duration::from_secs(30), false)?
        .head(url)
        .send()
        .map_err(|e| format!("HTTP error: {}", recipe::httpsteps::err_chain(&e)))?;
    if !resp.status().is_redirection() {
        return Err(format!("HTTP {} for {url}", resp.status()));
    }
    resp.headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .ok_or_else(|| "redirect carried no Location header".to_string())
}

fn asset_name(template: &str, version: &str, platform: &Platform) -> Result<String, String> {
    let mut ctx = recipe::template::Ctx::empty(*platform);
    ctx.version = version.to_string();
    recipe::template::expand_string(template, &ctx)
}

/// Resolve the latest release **without caching**. Uses `HEAD
/// github.com/{repo}/releases/latest` and reads the redirect — never
/// `api.github.com`, whose per-IP budget is exhausted on shared egress.
pub fn resolve_latest(recipe: &Recipe, platform: &Platform) -> Result<Resolved, String> {
    let key = platform.key();
    match recipe.source_for(platform) {
        Source::GithubRelease { repo, tag_style, assets, checksums } => {
            let template = assets
                .get(&key)
                .ok_or_else(|| format!("{} has no build for {key}", recipe.display_name))?;
            let (tag, version, floating) = if let Some(t) = tag_style.strip_prefix("floating:") {
                (t.to_string(), t.to_string(), true)
            } else {
                let location = redirect_location(&format!("https://github.com/{repo}/releases/latest"))?;
                let tag = parse_latest_tag_from_location(&location)
                    .ok_or_else(|| format!("no release tag in redirect target: {location}"))?;
                let version = if tag_style == "vPrefixed" { tag.trim_start_matches('v').to_string() } else { tag.clone() };
                (tag, version, false)
            };
            let asset = asset_name(template, &version, platform)?;
            let base = format!("https://github.com/{repo}/releases/download/{tag}");
            let checksums_url = match checksums {
                Checksums::None => None,
                Checksums::SumsFile { asset: a } => Some(format!("{base}/{a}")),
                Checksums::Sidecar { suffix } => Some(format!("{base}/{asset}{suffix}")),
            };
            Ok(Resolved { version, download_url: format!("{base}/{asset}"), asset, checksums_url, floating })
        }
        Source::HttpRedirect { latest_url, version_regex, checksums } => {
            let url = latest_url
                .get(&key)
                .ok_or_else(|| format!("{} has no build for {key}", recipe.display_name))?;
            let final_url = redirect_location(url)?;
            let re = regex::Regex::new(version_regex).map_err(|e| e.to_string())?;
            let version = re
                .captures(&final_url)
                .and_then(|c| c.get(1))
                .map(|m| m.as_str().to_string())
                .ok_or_else(|| format!("no version in `{final_url}`"))?;
            let asset = final_url.rsplit('/').next().unwrap_or("download").to_string();
            let checksums_url = match checksums {
                Checksums::None => None,
                Checksums::Sidecar { suffix } => Some(format!("{final_url}{suffix}")),
                Checksums::SumsFile { asset: a } => Some(match final_url.rsplit_once('/') {
                    Some((dir, _)) => format!("{dir}/{a}"),
                    None => a.clone(),
                }),
            };
            Ok(Resolved { version, download_url: final_url, asset, checksums_url, floating: false })
        }
        Source::HtmlIndex { page, links, checksums } => {
            let re = links
                .get(&key)
                .ok_or_else(|| format!("{} has no build for {key}", recipe.display_name))?;
            let html = String::from_utf8_lossy(&download(page, &mut |_, _, _| {})?).into_owned();
            let (download_url, version) =
                pick_index_link(&html, re, page)?.ok_or_else(|| format!("no link on {page} matches `{re}`"))?;
            let asset = download_url.rsplit('/').next().unwrap_or("download").to_string();
            let checksums_url = match checksums {
                Checksums::None => None,
                Checksums::Sidecar { suffix } => Some(format!("{download_url}{suffix}")),
                Checksums::SumsFile { asset: a } => Some(match download_url.rsplit_once('/') {
                    Some((dir, _)) => format!("{dir}/{a}"),
                    None => a.clone(),
                }),
            };
            Ok(Resolved { version, download_url, asset, checksums_url, floating: false })
        }
    }
}

/// Every `href="…"` on the page that matches `re`; the one with the highest
/// captured version wins. Relative hrefs are resolved against `page`.
pub fn pick_index_link(html: &str, re: &str, page: &str) -> Result<Option<(String, String)>, String> {
    let rx = regex::Regex::new(re).map_err(|e| format!("bad link regex: {e}"))?;
    let href_rx = regex::Regex::new(r#"href\s*=\s*["']([^"']+)["']"#).expect("static regex");
    let mut best: Option<(String, String)> = None;
    for cap in href_rx.captures_iter(html) {
        let href = &cap[1];
        let Some(m) = rx.captures(href) else { continue };
        let Some(version) = m.get(1).map(|v| v.as_str().to_string()) else { continue };
        let better = match &best {
            None => true,
            Some((_, v)) => version_lt(v, &version),
        };
        if better {
            best = Some((resolve_href(page, href), version));
        }
    }
    Ok(best)
}

fn resolve_href(page: &str, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_string();
    }
    let (scheme, rest) = page.split_once("://").unwrap_or(("https", page));
    let host = rest.split('/').next().unwrap_or(rest);
    if let Some(abs) = href.strip_prefix('/') {
        return format!("{scheme}://{host}/{abs}");
    }
    let base = page.rsplit_once('/').map(|(b, _)| b).unwrap_or(page);
    format!("{base}/{href}")
}

/// Cached resolution.
pub fn latest(recipe: &Recipe, platform: &Platform, cache: &LatestCache) -> Result<Resolved, String> {
    if let Some(cached) = cache.get(&recipe.name) {
        return cached.ok_or_else(|| format!("release lookup for {} failed recently", recipe.name));
    }
    let r = resolve_latest(recipe, platform);
    cache.set(&recipe.name, r.as_ref().ok().cloned());
    r
}

/// Compare two version strings by numeric segments.
pub fn version_lt(installed: &str, latest: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.trim()
            .trim_start_matches('v')
            .split(['.', '-'])
            .map(|s| s.parse().unwrap_or(0))
            .collect()
    };
    let a = parse(installed);
    let b = parse(latest);
    for i in 0..a.len().max(b.len()) {
        let (av, bv) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if av != bv {
            return av < bv;
        }
    }
    false
}

// --- Version directories ---

pub fn version_dir(p: &ToolPaths, version: &str) -> PathBuf {
    p.versions.join(version)
}

pub fn exe_name(rel: &str) -> String {
    if cfg!(windows) && !rel.ends_with(".exe") {
        format!("{rel}.exe")
    } else {
        rel.to_string()
    }
}

pub fn binary_path(recipe: &Recipe, p: &ToolPaths, version: &str) -> PathBuf {
    version_dir(p, version).join(exe_name(&recipe.main_binary()))
}

pub fn current_version(recipe: &Recipe, p: &ToolPaths) -> Option<String> {
    let v = std::fs::read_to_string(p.versions.join(CURRENT_FILE)).ok()?;
    let v = v.trim().to_string();
    (!v.is_empty() && binary_path(recipe, p, &v).is_file()).then_some(v)
}

pub fn set_current(p: &ToolPaths, version: &str) -> Result<(), String> {
    paths::write_atomic(&p.versions.join(CURRENT_FILE), format!("{version}\n").as_bytes(), false)
}

pub fn read_stamp(p: &ToolPaths, version: &str) -> Option<InstallStamp> {
    let text = std::fs::read_to_string(version_dir(p, version).join(STAMP_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Complete, non-current installs (a valid stamp and binary), newest first.
pub fn staged_versions(recipe: &Recipe, p: &ToolPaths) -> Vec<String> {
    let current = current_version(recipe, p);
    let mut out: Vec<String> = std::fs::read_dir(&p.versions)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .filter(|n| !n.starts_with('.'))
                .filter(|n| current.as_deref() != Some(n.as_str()))
                .filter(|n| version_dir(p, n).join(STAMP_FILE).is_file())
                .filter(|n| binary_path(recipe, p, n).is_file())
                .collect()
        })
        .unwrap_or_default();
    out.sort_by(|a, b| {
        if version_lt(a, b) {
            std::cmp::Ordering::Greater
        } else if version_lt(b, a) {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Equal
        }
    });
    out
}

/// The staged version worth applying: newer than current, or any when there
/// is no current.
pub fn staged_upgrade(recipe: &Recipe, p: &ToolPaths) -> Option<String> {
    let current = current_version(recipe, p);
    staged_versions(recipe, p).into_iter().find(|v| match &current {
        Some(c) => version_lt(c, v),
        None => true,
    })
}

pub fn prune_versions(p: &ToolPaths, keep: &[&str]) {
    let Ok(rd) = std::fs::read_dir(&p.versions) else { return };
    for e in rd.filter_map(|e| e.ok()) {
        let path = e.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        if keep.contains(&name) {
            continue;
        }
        if let Err(err) = std::fs::remove_dir_all(&path) {
            log::warn!("could not prune {}: {}", path.display(), err);
        }
    }
}

// --- Download + stage ---

pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

/// Digest for `asset` from a `<sha256>  <filename>` sums file, or the first
/// hex token of a sidecar file.
pub fn parse_checksum(text: &str, asset: &str) -> Option<String> {
    let by_name = text.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let file = parts.next()?;
        (file.trim_start_matches('*') == asset).then(|| hash.to_lowercase())
    });
    by_name.or_else(|| {
        text.split_whitespace()
            .find(|t| t.len() == 64 && t.bytes().all(|b| b.is_ascii_hexdigit()))
            .map(str::to_lowercase)
    })
}

fn download(url: &str, progress: &mut dyn FnMut(Phase, u64, Option<u64>)) -> Result<Vec<u8>, String> {
    let client = http_client(Duration::from_secs(900), true)?;
    let mut resp = client.get(url).send().map_err(|e| format!("download error: {}", recipe::httpsteps::err_chain(&e)))?;
    if !resp.status().is_success() {
        return Err(format!("download failed: HTTP {} for {url}", resp.status()));
    }
    let total = resp.content_length();
    let mut data: Vec<u8> = Vec::with_capacity(total.unwrap_or(0) as usize);
    let mut buf = [0u8; 64 * 1024];
    let mut downloaded: u64 = 0;
    let mut last = Instant::now();
    progress(Phase::Downloading, 0, total);
    loop {
        let n = resp.read(&mut buf).map_err(|e| format!("download read error: {e}"))?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
        downloaded += n as u64;
        if last.elapsed() >= Duration::from_millis(250) {
            progress(Phase::Downloading, downloaded, total);
            last = Instant::now();
        }
    }
    progress(Phase::Downloading, downloaded, total);
    Ok(data)
}

/// Download `resolved`, verify, extract into `versions/<version>/` and run
/// the binary's version flag. Does **not** touch `current`. Returns the
/// archive's sha256.
pub fn download_and_stage(
    recipe: &Recipe,
    p: &ToolPaths,
    resolved: &Resolved,
    progress: &mut dyn FnMut(Phase, u64, Option<u64>),
) -> Result<String, String> {
    std::fs::create_dir_all(&p.versions).map_err(|e| format!("create {}: {e}", p.versions.display()))?;
    let data = download(&resolved.download_url, progress)?;

    if (data.len() as u64) < recipe.min_binary_bytes.min(1024) {
        return Err(format!("download of {} is only {} bytes", resolved.asset, data.len()));
    }
    if data.first() == Some(&b'<') {
        return Err(format!("download of {} returned an HTML page, not a release", resolved.asset));
    }
    let sha = sha256_hex(&data);
    if let Some(url) = &resolved.checksums_url {
        let text = String::from_utf8_lossy(&download(url, &mut |_, _, _| {})?).into_owned();
        let expected = parse_checksum(&text, &resolved.asset)
            .ok_or_else(|| format!("no digest for {} in {url}", resolved.asset))?;
        if expected != sha {
            return Err(format!("checksum mismatch for {}: expected {expected}, got {sha}", resolved.asset));
        }
    }

    progress(Phase::Extracting, data.len() as u64, Some(data.len() as u64));
    let partial = p.versions.join(format!(".{}.partial", resolved.version));
    let _ = std::fs::remove_dir_all(&partial);
    std::fs::create_dir_all(&partial).map_err(|e| format!("create {}: {e}", partial.display()))?;
    let platform = Platform::current();
    let staged = (|| -> Result<(), String> {
        match recipe.archive_for(&platform) {
            Archive::Zip => extract_zip(&data, &partial)?,
            Archive::Tgz => return Err("tgz archives are not supported yet".to_string()),
            Archive::Bare => std::fs::write(partial.join(exe_name(&recipe.main_binary())), &data)
                .map_err(|e| format!("write error: {e}"))?,
        }
        if recipe.layout_for(&platform).strip_top_dir {
            strip_top_dir(&partial)?;
        }
        Ok(())
    })();
    if let Err(e) = staged {
        let _ = std::fs::remove_dir_all(&partial);
        return Err(e);
    }
    drop(data);

    progress(Phase::Verifying, 0, None);
    for rel in recipe.binaries() {
        let bin = partial.join(exe_name(&rel));
        let size = std::fs::metadata(&bin).map(|m| m.len()).unwrap_or(0);
        if size == 0 || (rel == recipe.main_binary() && size < recipe.min_binary_bytes) {
            let _ = std::fs::remove_dir_all(&partial);
            return Err(format!("archive did not contain a usable {} ({size} bytes)", exe_name(&rel)));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755));
        }
    }
    let main = partial.join(exe_name(&recipe.main_binary()));
    if let Err(e) = verify_version(recipe, &main, &partial, resolved) {
        let _ = std::fs::remove_dir_all(&partial);
        return Err(e);
    }

    let stamp = InstallStamp { version: resolved.version.clone(), archive_sha256: sha.clone(), installed_at: paths::now_secs() };
    std::fs::write(partial.join(STAMP_FILE), serde_json::to_string_pretty(&stamp).expect("stamp serializes"))
        .map_err(|e| format!("stamp write error: {e}"))?;
    let dest = version_dir(p, &resolved.version);
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::rename(&partial, &dest).map_err(|e| {
        let _ = std::fs::remove_dir_all(&partial);
        format!("install error (is {} in use?): {e}", recipe.main_binary())
    })?;
    log::info!("staged {} {} in {}", recipe.name, resolved.version, dest.display());
    Ok(sha)
}

/// Extract every entry under `dest`; anything escaping it is rejected.
pub fn extract_zip(data: &[u8], dest: &Path) -> Result<(), String> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(data)).map_err(|e| format!("invalid archive: {e}"))?;
    for i in 0..zip.len() {
        let mut file = zip.by_index(i).map_err(|e| format!("archive read error: {e}"))?;
        let Some(rel) = file.enclosed_name() else {
            return Err(format!("unexpected path in archive: {}", file.name()));
        };
        let out = dest.join(&rel);
        if file.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| format!("extract error: {e}"))?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("extract error: {e}"))?;
        }
        let mut contents = Vec::with_capacity(file.size() as usize);
        file.read_to_end(&mut contents).map_err(|e| format!("archive read error: {e}"))?;
        std::fs::write(&out, &contents).map_err(|e| format!("extract write error: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = file.unix_mode().unwrap_or(0o755) & 0o7777;
            let _ = std::fs::set_permissions(&out, std::fs::Permissions::from_mode(if mode == 0 { 0o755 } else { mode }));
        }
    }
    Ok(())
}

/// Archives with one versioned top folder (`ffmpeg-7.1-essentials/bin/…`):
/// move its contents up one level.
pub fn strip_top_dir(dir: &Path) -> Result<(), String> {
    let entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    let [top] = entries.as_slice() else {
        return Err(format!("expected one top-level folder, found {}", entries.len()));
    };
    if !top.is_dir() {
        return Err("expected a top-level folder".into());
    }
    for e in std::fs::read_dir(top).map_err(|e| e.to_string())?.filter_map(|e| e.ok()) {
        std::fs::rename(e.path(), dir.join(e.file_name())).map_err(|e| format!("layout error: {e}"))?;
    }
    std::fs::remove_dir(top).map_err(|e| e.to_string())
}

/// `Command::output()` with an upper bound; kills the whole process group
/// on timeout so helper children die with the probe.
pub fn output_with_timeout(cmd: &mut std::process::Command, timeout: Duration) -> std::io::Result<Option<std::process::Output>> {
    use std::process::Stdio;
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(super::process::CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn()?;
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output().map(Some);
        }
        if Instant::now() >= deadline {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Run `bin <version.args>` and read the version with the recipe's regex.
pub fn probe_version(recipe: &Recipe, bin: &Path, data_dir: &Path) -> Result<String, String> {
    let mut cmd = std::process::Command::new(bin);
    cmd.args(&recipe.version.args);
    if let Some(run) = &recipe.run {
        // The same environment the daemon gets (e.g. .NET's extraction dir).
        let mut ctx = recipe::template::Ctx::empty(Platform::current());
        ctx.data = data_dir.to_string_lossy().into_owned();
        ctx.home = paths::home_dir().to_string_lossy().into_owned();
        for (k, v) in &run.env {
            if let Ok(val) = recipe::template::expand_string(v, &ctx) {
                cmd.env(k, val);
            }
        }
    }
    let timeout = Duration::from_secs(recipe.version.timeout_sec.max(5));
    match output_with_timeout(&mut cmd, timeout) {
        Ok(Some(out)) => {
            let text = format!("{}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            let re = regex::Regex::new(&recipe.version.regex).map_err(|e| e.to_string())?;
            re.captures(&text)
                .and_then(|c| c.get(1))
                .map(|m| m.as_str().to_string())
                .ok_or_else(|| {
                    format!(
                        "{} did not report a version (exit {:?}): {}",
                        recipe.main_binary(),
                        out.status.code(),
                        text.trim().chars().take(300).collect::<String>()
                    )
                })
        }
        Ok(None) => Err(format!("{} version check did not finish within {timeout:?}", recipe.main_binary())),
        Err(e) => Err(format!("could not run {}: {e}", recipe.main_binary())),
    }
}

fn verify_version(recipe: &Recipe, bin: &Path, partial: &Path, resolved: &Resolved) -> Result<(), String> {
    let data_dir = partial.join(".verify");
    let _ = std::fs::create_dir_all(&data_dir);
    let reported = probe_version(recipe, bin, &data_dir)?;
    let _ = std::fs::remove_dir_all(&data_dir);
    if resolved.floating || resolved.version.starts_with(&reported) || reported.starts_with(&resolved.version) {
        Ok(())
    } else {
        Err(format!("{} reports version {reported}, expected {}", recipe.main_binary(), resolved.version))
    }
}

/// Outcome of trying to make a staged version (or a pending restart) live.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum ApplyOutcome {
    Applied { from: Option<String>, to: String },
    Deferred { reason: DeferReason },
    Nothing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DeferReason {
    Busy,
    Unreachable,
    RestartNotAllowed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp(name: &str) -> ToolPaths {
        let root = std::env::temp_dir().join(format!("roadie-install-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let p = ToolPaths { versions: root.join("versions"), data: root.join("data"), logs: root.join("logs"), root };
        std::fs::create_dir_all(&p.versions).unwrap();
        p
    }

    fn zip_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default();
            for (name, data) in entries {
                w.start_file(*name, opts).unwrap();
                w.write_all(data).unwrap();
            }
            w.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn extracts_nested_paths_but_rejects_traversal() {
        let p = tmp("extract");
        let d = p.root.join("x");
        let ok = zip_with(&[("slskd", b"bin"), ("wwwroot/index.html", b"<html>")]);
        extract_zip(&ok, &d).unwrap();
        assert!(d.join("slskd").is_file() && d.join("wwwroot/index.html").is_file());
        let bad = zip_with(&[("../escape", b"x")]);
        assert!(extract_zip(&bad, &d).unwrap_err().contains("unexpected path"));
        assert!(!p.root.join("escape").exists());
        let _ = std::fs::remove_dir_all(&p.root);
    }

    #[test]
    fn strips_one_top_dir_and_refuses_ambiguity() {
        let p = tmp("strip");
        let d = p.root.join("x");
        extract_zip(&zip_with(&[("ffmpeg-7.1/bin/ffmpeg", b"a"), ("ffmpeg-7.1/README", b"b")]), &d).unwrap();
        strip_top_dir(&d).unwrap();
        assert!(d.join("bin/ffmpeg").is_file() && d.join("README").is_file());
        assert!(strip_top_dir(&d).is_err(), "two entries now");
        let _ = std::fs::remove_dir_all(&p.root);
    }

    #[test]
    fn current_pointer_and_staging_order() {
        let p = tmp("current");
        let recipe = crate::recipe::load_builtin().remove(0);
        for v in ["0.25.1", "0.26.0", "0.24.0"] {
            let vd = version_dir(&p, v);
            std::fs::create_dir_all(&vd).unwrap();
            std::fs::write(vd.join(exe_name("slskd")), b"x").unwrap();
            std::fs::write(vd.join(STAMP_FILE), "{}").unwrap();
        }
        assert_eq!(current_version(&recipe, &p), None);
        set_current(&p, "0.25.1").unwrap();
        assert_eq!(current_version(&recipe, &p), Some("0.25.1".into()));
        assert_eq!(staged_versions(&recipe, &p), vec!["0.26.0".to_string(), "0.24.0".to_string()]);
        assert_eq!(staged_upgrade(&recipe, &p), Some("0.26.0".into()));
        prune_versions(&p, &["0.25.1", "0.26.0"]);
        assert!(!version_dir(&p, "0.24.0").exists() && version_dir(&p, "0.26.0").exists());
        set_current(&p, "9.9.9").unwrap();
        assert_eq!(current_version(&recipe, &p), None, "pointer to a missing binary reads as not installed");
        let _ = std::fs::remove_dir_all(&p.root);
    }

    #[test]
    fn html_index_picks_the_highest_release_link_and_resolves_it() {
        let html = r#"<a href="/download/macos/arm64/1789407207_N-126556-g639ee84952/ffmpeg.zip">snap</a>
            <a href="/download/macos/arm64/1789931890_9.0.2/ffmpeg.zip">rel</a>
            <a href="/download/macos/arm64/1789931890_9.0.2/ffmpeg.zip.sha256">sum</a>
            <a href="/download/macos/arm64/1700000000_8.1/ffmpeg.zip">old</a>"#;
        let re = r"^/download/macos/arm64/\d+_(\d+\.\d+(?:\.\d+)?)/ffmpeg\.zip$";
        let (url, v) = pick_index_link(html, re, "https://ffmpeg.martin-riedl.de/").unwrap().unwrap();
        assert_eq!(v, "9.0.2");
        assert_eq!(url, "https://ffmpeg.martin-riedl.de/download/macos/arm64/1789931890_9.0.2/ffmpeg.zip");
        assert_eq!(pick_index_link(html, r"nothing_(\d+)", "https://x/").unwrap(), None);
        assert_eq!(resolve_href("https://h/dir/index.html", "file.zip"), "https://h/dir/file.zip");
        assert_eq!(resolve_href("https://h/dir/", "https://cdn/x.zip"), "https://cdn/x.zip");
    }

    #[test]
    fn checksum_parsing_and_version_compare() {
        let sums = "abc  other\nDEADbeef *yt-dlp_macos\n";
        assert_eq!(parse_checksum(sums, "yt-dlp_macos"), Some("deadbeef".into()));
        let side = format!("{}  ffmpeg.zip\n", "a".repeat(64));
        assert_eq!(parse_checksum(&side, "whatever"), Some("a".repeat(64)));
        assert_eq!(parse_checksum("nothing", "x"), None);
        assert!(version_lt("0.25.1", "0.26.0") && !version_lt("0.26.0", "0.26.0") && version_lt("v1.2", "1.2.1"));
        assert_eq!(parse_latest_tag_from_location("https://github.com/slskd/slskd/releases/tag/0.26.0"), Some("0.26.0".into()));
        assert_eq!(parse_latest_tag_from_location("https://github.com/x/y/releases/tag/v9.0.1?x"), Some("v9.0.1".into()));
        assert_eq!(asset_name("slskd-{version}-osx-arm64.zip", "0.26.0", &Platform { os: "darwin", arch: "arm64" }).unwrap(), "slskd-0.26.0-osx-arm64.zip");
    }
}
