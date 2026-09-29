//! The recipe catalog: `outcast1000/roadie-recipes`, Roadie's only source of
//! recipes it does not have yet. Its `index.json` lists each recipe with the
//! sha256 of its file; Roadie fetches both from raw.githubusercontent.com
//! (never `api.github.com`) and caches them under `<data>/catalog/`.
//!
//! The catalog is not signed. The sha256 checks a file against the index,
//! which is consistency, not authenticity, so a catalog recipe is never
//! trusted by arriving: the store lists it as `Origin::Catalog`, and it is
//! installed the way a recipe an app brings is, after the user reviews it.
//!
//! A failed fetch keeps the cache and says so (`Status::error`). Fetching is
//! short and bounded: the CLI is a one-shot process an app may be waiting on.

use super::Recipe;
use crate::paths;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

pub const BASE_URL: &str = "https://raw.githubusercontent.com/outcast1000/roadie-recipes/main/";
pub const INDEX_VERSION: u32 = 1;
/// A cache younger than this is used without asking the network.
pub const TTL_SECS: u64 = 6 * 60 * 60;
/// After a failed fetch, wait this long before an automatic retry, so an
/// offline machine doesn't pay the timeout on every CLI call.
const RETRY_SECS: u64 = 10 * 60;
const TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub name: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub revision: u32,
    #[serde(default)]
    pub platforms: Vec<String>,
    #[serde(default = "zero_version")]
    pub min_roadie: String,
    pub path: String,
    pub sha256: String,
}

fn zero_version() -> String {
    "0.0.0".into()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Index {
    index_version: u32,
    #[serde(default)]
    recipes: Vec<Entry>,
}

/// What the cache knows about the last fetch (`<data>/catalog/meta.json`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Meta {
    /// The last fetch that succeeded (unix seconds).
    fetched_at: Option<u64>,
    /// The last attempt, successful or not.
    attempted_at: Option<u64>,
    /// Why the last attempt failed; `None` after a success.
    error: Option<String>,
}

/// The catalog's state for clients (`catalog` in `tool list`, `/v1/catalog`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub url: String,
    pub fetched_at: Option<u64>,
    /// Never fetched, older than the TTL, or the last fetch failed.
    pub stale: bool,
    pub error: Option<String>,
    /// Recipes usable by this Roadie (after the sha256 and `minRoadie` checks).
    pub recipes: usize,
}

/// A catalog recipe that passed every check, with its index entry.
#[derive(Debug, Clone)]
pub struct Listed {
    pub entry: Entry,
    pub recipe: Recipe,
}

pub type Fetch<'a> = &'a dyn Fn(&str) -> Result<Vec<u8>, String>;

fn dir() -> Result<PathBuf, String> {
    Ok(paths::data_root()?.join("catalog"))
}

fn read_meta() -> Meta {
    dir()
        .ok()
        .and_then(|d| std::fs::read_to_string(d.join("meta.json")).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn write_meta(m: &Meta) -> Result<(), String> {
    let text = serde_json::to_string_pretty(m).map_err(|e| e.to_string())?;
    paths::write_atomic(&dir()?.join("meta.json"), text.as_bytes(), false)
}

/// `have >= need` for `x.y.z` versions (a pre-release suffix is ignored).
/// An unreadable `need` is never met, so a malformed entry stays hidden.
pub fn version_at_least(have: &str, need: &str) -> bool {
    fn parts(v: &str) -> Option<[u64; 3]> {
        let core = v.trim().split(['-', '+']).next()?;
        let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
        let out = [it.next()??, it.next()??, it.next()??];
        it.next().is_none().then_some(out)
    }
    match (parts(have), parts(need)) {
        (Some(h), Some(n)) => h >= n,
        _ => false,
    }
}

/// The recipes in the cache this Roadie can use. An entry is skipped, with
/// a warning, when its file is missing, does not match its sha256, fails
/// validation or is named differently; it is hidden when it needs a newer
/// Roadie (`minRoadie`), because an older Roadie ignores fields it does not
/// know and would silently mis-run the recipe. Entries for other platforms
/// are kept: the store lists them as unsupported, as for any recipe.
pub fn load() -> Vec<Listed> {
    let Ok(d) = dir() else { return vec![] };
    let Ok(text) = std::fs::read_to_string(d.join("index.json")) else { return vec![] };
    let index = match parse_index(&text) {
        Ok(i) => i,
        Err(e) => {
            log::warn!("recipe catalog cache: {e}");
            return vec![];
        }
    };
    let mut out = Vec::new();
    for entry in index.recipes {
        if !version_at_least(env!("CARGO_PKG_VERSION"), &entry.min_roadie) {
            log::info!("catalog recipe {} needs Roadie {}; hidden", entry.name, entry.min_roadie);
            continue;
        }
        let path = d.join("recipes").join(format!("{}.json", entry.name));
        let Ok(bytes) = std::fs::read(&path) else {
            log::warn!("catalog recipe {} is listed but not cached; skipped", entry.name);
            continue;
        };
        if let Some(recipe) = check(&entry, &bytes) {
            out.push(Listed { entry, recipe });
        }
    }
    out
}

/// Every name the cached index lists, usable or not; `None` before the
/// first successful fetch. A trusted catalog recipe missing from it has
/// been delisted.
pub fn index_names() -> Option<Vec<String>> {
    let text = std::fs::read_to_string(dir().ok()?.join("index.json")).ok()?;
    parse_index(&text).ok().map(|i| i.recipes.into_iter().map(|e| e.name).collect())
}

fn parse_index(text: &str) -> Result<Index, String> {
    let index: Index = serde_json::from_str(text).map_err(|e| format!("index.json is not a catalog index: {e}"))?;
    if index.index_version != INDEX_VERSION {
        return Err(format!("index.json is format {}, this Roadie reads {INDEX_VERSION}; update Roadie", index.index_version));
    }
    Ok(index)
}

/// The file as a recipe, if it is the one the entry describes.
fn check(entry: &Entry, bytes: &[u8]) -> Option<Recipe> {
    let sha = crate::tools::install::sha256_hex(bytes);
    if !sha.eq_ignore_ascii_case(&entry.sha256) {
        log::warn!("catalog recipe {}: sha256 {sha} does not match the index's {}; skipped", entry.name, entry.sha256);
        return None;
    }
    let text = String::from_utf8_lossy(bytes);
    match super::parse(&text) {
        Ok(r) if r.name == entry.name => Some(r),
        Ok(r) => {
            log::warn!("catalog entry {} holds a recipe named {}; skipped", entry.name, r.name);
            None
        }
        Err(e) => {
            log::warn!("catalog recipe {} is invalid for this Roadie: {e:?}", entry.name);
            None
        }
    }
}

/// Fetch the index and every recipe it lists, and replace the cache. A
/// failure leaves the cache as it was and is recorded for `status()`.
/// Returns how many recipes were cached.
pub fn refresh_with(fetch: Fetch) -> Result<usize, String> {
    let mut meta = read_meta();
    meta.attempted_at = Some(paths::now_secs());
    let result = fetch_all(fetch);
    match &result {
        Ok(_) => {
            meta.fetched_at = meta.attempted_at;
            meta.error = None;
        }
        Err(e) => {
            log::warn!("recipe catalog: {e}");
            meta.error = Some(e.clone());
        }
    }
    write_meta(&meta)?;
    result
}

fn fetch_all(fetch: Fetch) -> Result<usize, String> {
    let index_url = format!("{BASE_URL}index.json");
    let index_bytes = fetch(&index_url)?;
    let index = parse_index(&String::from_utf8_lossy(&index_bytes))?;
    let d = dir()?;
    let recipes_dir = d.join("recipes");
    std::fs::create_dir_all(&recipes_dir).map_err(|e| format!("create {}: {e}", recipes_dir.display()))?;
    let mut kept = Vec::new();
    for entry in &index.recipes {
        // The cache file is named after the entry, never after its `path`,
        // so an index cannot make Roadie write outside the cache.
        if !super::is_valid_name(&entry.name) || entry.path != format!("recipes/{}.json", entry.name) {
            log::warn!("catalog entry {:?} (path {:?}) is malformed; skipped", entry.name, entry.path);
            continue;
        }
        let bytes = match fetch(&format!("{BASE_URL}{}", entry.path)) {
            Ok(b) => b,
            Err(e) => {
                log::warn!("catalog recipe {}: {e}; keeping the cached copy if any", entry.name);
                kept.push(entry.name.clone());
                continue;
            }
        };
        if check(entry, &bytes).is_some() {
            paths::write_atomic(&recipes_dir.join(format!("{}.json", entry.name)), &bytes, false)?;
        }
        kept.push(entry.name.clone());
    }
    // A recipe that left the index leaves the cache.
    if let Ok(rd) = std::fs::read_dir(&recipes_dir) {
        for e in rd.flatten() {
            let p = e.path();
            let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_string();
            if !kept.contains(&stem) {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
    paths::write_atomic(&d.join("index.json"), &index_bytes, false)?;
    Ok(load().len())
}

/// Fetch from the real catalog.
pub fn refresh() -> Result<usize, String> {
    refresh_with(&http_get)
}

/// Refresh when the cache is older than the TTL and no attempt failed in
/// the last few minutes. Returns whether it fetched.
pub fn refresh_if_stale() -> bool {
    let m = read_meta();
    let now = paths::now_secs();
    let fresh = m.fetched_at.is_some_and(|t| now.saturating_sub(t) < TTL_SECS);
    let retried = m.error.is_some() && m.attempted_at.is_some_and(|t| now.saturating_sub(t) < RETRY_SECS);
    if fresh || retried {
        return false;
    }
    let _ = refresh();
    true
}

pub fn status() -> Status {
    let m = read_meta();
    let now = paths::now_secs();
    let old = m.fetched_at.is_none_or(|t| now.saturating_sub(t) >= TTL_SECS);
    Status { url: format!("{BASE_URL}index.json"), fetched_at: m.fetched_at, stale: old || m.error.is_some(), error: m.error, recipes: load().len() }
}

fn http_get(url: &str) -> Result<Vec<u8>, String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("Roadie")
        .timeout(TIMEOUT)
        .build()
        .map_err(|e| format!("HTTP client error: {e}"))?;
    let resp = client.get(url).send().map_err(|e| format!("fetch {url}: {}", super::httpsteps::err_chain(&e)))?;
    if !resp.status().is_success() {
        return Err(format!("fetch {url}: HTTP {}", resp.status()));
    }
    resp.bytes().map(|b| b.to_vec()).map_err(|e| format!("read {url}: {}", super::httpsteps::err_chain(&e)))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Tests share the process-wide data root and so the catalog cache.
    pub fn serial() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Serve `recipes` as the whole catalog and reload the store. Hold
    /// `serial()` while the test relies on it.
    pub fn publish(recipes: &[Recipe]) {
        crate::recipe::store::test_root();
        let files: HashMap<String, Vec<u8>> = recipes.iter().map(|r| (format!("recipes/{}.json", r.name), serde_json::to_vec_pretty(r).unwrap())).collect();
        let entries: Vec<Entry> = recipes.iter().map(|r| entry(&r.name, &files[&format!("recipes/{}.json", r.name)])).collect();
        let mut files = files;
        files.insert("index.json".into(), index(&entries));
        refresh_with(&serve(files)).unwrap();
        crate::recipe::store::load_all();
    }

    fn index(entries: &[Entry]) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({ "indexVersion": 1, "recipes": entries })).unwrap()
    }

    fn entry(name: &str, bytes: &[u8]) -> Entry {
        Entry {
            name: name.into(),
            summary: String::new(),
            kind: "cli".into(),
            revision: 1,
            platforms: vec![],
            min_roadie: "0.0.0".into(),
            path: format!("recipes/{name}.json"),
            sha256: crate::tools::install::sha256_hex(bytes),
        }
    }

    fn serve(files: HashMap<String, Vec<u8>>) -> impl Fn(&str) -> Result<Vec<u8>, String> {
        move |url: &str| files.get(url.strip_prefix(BASE_URL).unwrap_or(url)).cloned().ok_or_else(|| format!("fetch {url}: HTTP 404"))
    }

    #[test]
    fn versions_compare_by_number() {
        assert!(version_at_least("0.6.0", "0.0.0"));
        assert!(version_at_least("0.10.0", "0.9.9"));
        assert!(version_at_least("1.2.3-beta", "1.2.3"));
        assert!(!version_at_least("0.5.2", "0.6.0"));
        assert!(!version_at_least("0.6.0", "soon"), "an unreadable minRoadie is never met");
    }

    #[test]
    fn refresh_caches_checks_hides_and_survives_failure() {
        let _s = serial();
        crate::recipe::store::test_root();
        let ytdlp = crate::recipe::fixtures::json("yt-dlp").as_bytes().to_vec();
        let ffmpeg = crate::recipe::fixtures::json("ffmpeg").as_bytes().to_vec();
        let slskd = crate::recipe::fixtures::json("slskd").as_bytes().to_vec();
        let mut future = entry("slskd", &slskd);
        future.min_roadie = "999.0.0".into();
        let mut tampered = entry("ffmpeg", &ffmpeg);
        tampered.sha256 = "0".repeat(64);
        let files = HashMap::from([
            ("index.json".to_string(), index(&[entry("yt-dlp", &ytdlp), tampered, future])),
            ("recipes/yt-dlp.json".to_string(), ytdlp.clone()),
            ("recipes/ffmpeg.json".to_string(), ffmpeg),
            ("recipes/slskd.json".to_string(), slskd),
        ]);
        assert_eq!(refresh_with(&serve(files)), Ok(1), "only yt-dlp passes");
        let listed: Vec<String> = load().into_iter().map(|l| l.recipe.name).collect();
        assert_eq!(listed, vec!["yt-dlp"], "sha mismatch skipped, minRoadie hides");
        let st = status();
        assert_eq!((st.stale, st.error.as_deref(), st.recipes), (false, None, 1));

        assert!(refresh_with(&|url: &str| Err(format!("fetch {url}: offline"))).is_err());
        assert_eq!(load().len(), 1, "a failed fetch keeps the cache");
        let st = status();
        assert!(st.stale && st.error.as_deref().is_some_and(|e| e.contains("offline")), "{st:?}");

        let files = HashMap::from([("index.json".to_string(), index(&[]))]);
        assert_eq!(refresh_with(&serve(files)), Ok(0));
        assert!(load().is_empty(), "a recipe that left the index leaves the cache");
    }

    #[test]
    fn an_index_cannot_write_outside_the_cache() {
        let _s = serial();
        crate::recipe::store::test_root();
        let bytes = crate::recipe::fixtures::json("yt-dlp").as_bytes().to_vec();
        let mut evil = entry("yt-dlp", &bytes);
        evil.path = "../../escape.json".into();
        let files = HashMap::from([("index.json".to_string(), index(&[evil])), ("../../escape.json".to_string(), bytes)]);
        assert_eq!(refresh_with(&serve(files)), Ok(0));
        assert!(load().is_empty());
        let newer = br#"{"indexVersion": 2, "recipes": []}"#.to_vec();
        let err = refresh_with(&serve(HashMap::from([("index.json".to_string(), newer)]))).unwrap_err();
        assert!(err.contains("update Roadie"), "{err}");
    }

    #[test]
    #[ignore = "network: the real catalog"]
    fn the_real_catalog_parses() {
        let _s = serial();
        crate::recipe::store::test_root();
        let n = refresh().expect("fetch the catalog");
        assert!(n >= 3, "{n} recipes");
        assert!(load().iter().any(|l| l.recipe.name == "slskd"));
    }
}
