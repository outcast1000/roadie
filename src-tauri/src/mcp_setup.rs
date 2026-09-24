//! What the Settings MCP card needs: where the bundled server script is and
//! an absolute `node` that can run it. GUI apps launch without the shell's
//! PATH, so a bare `"command": "node"` in a client config often resolves to
//! nothing — the card copies absolute paths.

use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpSetupInfo {
    pub script_path: Option<String>,
    pub node_path: Option<String>,
    pub node_version: Option<String>,
    pub node_ok: bool,
    pub data_dir: String,
    pub problem: Option<String>,
}

pub fn script_path(resource_dir: Option<PathBuf>) -> Option<PathBuf> {
    let bundled = resource_dir.map(|d| d.join("mcp").join("roadie-mcp.mjs")).filter(|p| p.is_file());
    bundled.or_else(|| {
        // Dev: the repo checkout next to src-tauri.
        let dev = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("mcp").join("roadie-mcp.mjs");
        std::fs::canonicalize(dev).ok().filter(|p| p.is_file())
    })
}

fn candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            out.push(dir.join(if cfg!(windows) { "node.exe" } else { "node" }));
        }
    }
    let home = crate::paths::home_dir();
    for fixed in ["/opt/homebrew/bin/node", "/usr/local/bin/node", "/usr/bin/node", "/opt/local/bin/node"] {
        out.push(PathBuf::from(fixed));
    }
    out.push(home.join(".volta/bin/node"));
    out.push(home.join(".asdf/shims/node"));
    for mgr in [".nvm/versions/node", ".fnm/node-versions", "Library/Application Support/fnm/node-versions"] {
        if let Ok(rd) = std::fs::read_dir(home.join(mgr)) {
            let mut versions: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
            versions.sort_by_key(|p| std::cmp::Reverse(version_key(p.file_name().and_then(|n| n.to_str()).unwrap_or(""))));
            for v in versions {
                out.push(v.join("bin/node"));
                out.push(v.join("installation/bin/node"));
            }
        }
    }
    #[cfg(windows)]
    {
        if let Some(pf) = std::env::var_os("ProgramFiles") {
            out.push(PathBuf::from(pf).join("nodejs").join("node.exe"));
        }
    }
    out
}

fn version_key(name: &str) -> Vec<u64> {
    name.trim_start_matches('v').split('.').map(|s| s.parse().unwrap_or(0)).collect()
}

fn node_version(path: &Path) -> Option<String> {
    let out = std::process::Command::new(path).arg("--version").output().ok()?;
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    v.starts_with('v').then_some(v)
}

pub fn find_node() -> Option<(PathBuf, String)> {
    let mut seen = std::collections::HashSet::new();
    for c in candidates() {
        if !c.is_file() {
            continue;
        }
        let canon = std::fs::canonicalize(&c).unwrap_or(c.clone());
        if !seen.insert(canon.clone()) {
            continue;
        }
        if let Some(v) = node_version(&canon) {
            return Some((canon, v));
        }
    }
    None
}

pub fn info(resource_dir: Option<PathBuf>) -> McpSetupInfo {
    let script = script_path(resource_dir);
    let node = find_node();
    let node_ok = node.as_ref().is_some_and(|(_, v)| version_key(v).first().copied().unwrap_or(0) >= 18);
    let problem = match (&script, &node) {
        (None, _) => Some("The MCP server script was not found in this build.".to_string()),
        (_, None) => Some("Node.js (18 or newer) was not found. Install it from nodejs.org, then reopen this page.".to_string()),
        (_, Some((_, v))) if !node_ok => Some(format!("Node.js {v} is too old; the MCP server needs 18 or newer.")),
        _ => None,
    };
    McpSetupInfo {
        script_path: script.map(|p| p.to_string_lossy().into_owned()),
        node_path: node.as_ref().map(|(p, _)| p.to_string_lossy().into_owned()),
        node_version: node.map(|(_, v)| v),
        node_ok,
        data_dir: crate::paths::data_root().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
        problem,
    }
}
