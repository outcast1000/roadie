//! One-time migration from Roadie 0.5.2 and earlier, which carried slskd,
//! yt-dlp and ffmpeg built in and never wrote them to disk: an installed
//! slskd ran off the compiled-in copy. This binary ships no recipes, so it
//! would find `tools/slskd/data/state.json` and no recipe for it.
//!
//! `legacy/*.json` are those recipes exactly as 0.5.2 shipped them, frozen.
//! For each, when the tool has state and no user recipe is on disk, the copy
//! is written as a user recipe from the catalog. The user trusted it as a
//! built-in already, so this is not a new trust decision; from then on the
//! catalog's newer revisions arrive as recipe updates for review. The copies
//! are never listed, never installable and never used without state.
//!
//! Delete this module (and `legacy/`) in the release RELEASING.md names.

use crate::paths;
use std::path::Path;

const LEGACY: &[(&str, &str)] = &[
    ("slskd", include_str!("legacy/slskd.json")),
    ("yt-dlp", include_str!("legacy/yt-dlp.json")),
    ("ffmpeg", include_str!("legacy/ffmpeg.json")),
];

/// Migrate what `root` needs. Returns the names written.
pub fn migrate(root: &Path) -> Vec<String> {
    let mut done = Vec::new();
    for (name, json) in LEGACY {
        let has_state = root.join("tools").join(name).join("data").join("state.json").is_file();
        let recipe_file = root.join("recipes").join(format!("{name}.json"));
        if !has_state || recipe_file.exists() {
            continue;
        }
        let recipe: serde_json::Value = match serde_json::from_str(json) {
            Ok(v) => v,
            Err(e) => {
                log::error!("legacy recipe {name} does not parse: {e}");
                continue;
            }
        };
        let envelope = serde_json::json!({ "source": "catalog", "recipe": recipe });
        let text = serde_json::to_string_pretty(&envelope).unwrap_or_default();
        match paths::write_atomic(&recipe_file, text.as_bytes(), false) {
            Ok(()) => {
                log::info!("{name}: installed with a recipe Roadie used to ship built in; kept it as a recipe from the catalog");
                done.push(name.to_string());
            }
            Err(e) => log::error!("{name}: could not keep its former built-in recipe: {e}"),
        }
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_legacy_recipe_is_valid_and_frozen_at_its_revision() {
        let revs: Vec<(String, u32)> = LEGACY.iter().map(|(n, j)| (n.to_string(), crate::recipe::parse(j).unwrap_or_else(|e| panic!("{n}: {e:?}")).revision)).collect();
        assert_eq!(revs, vec![("slskd".into(), 5), ("yt-dlp".into(), 2), ("ffmpeg".into(), 2)], "the catalog starts at these revisions");
    }

    #[test]
    fn migrates_once_only_where_a_tool_has_state_and_no_recipe() {
        let root = std::env::temp_dir().join(format!("roadie-legacy-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        assert!(migrate(&root).is_empty(), "no state: nothing");

        for name in ["slskd", "ffmpeg"] {
            let data = root.join("tools").join(name).join("data");
            std::fs::create_dir_all(&data).unwrap();
            std::fs::write(data.join("state.json"), "{}").unwrap();
        }
        std::fs::create_dir_all(root.join("recipes")).unwrap();
        std::fs::write(root.join("recipes").join("ffmpeg.json"), "mine").unwrap();

        assert_eq!(migrate(&root), vec!["slskd"], "an existing user recipe is left alone");
        let text = std::fs::read_to_string(root.join("recipes").join("slskd.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!((v["source"].as_str(), v["recipe"]["name"].as_str(), v["recipe"]["revision"].as_u64()), (Some("catalog"), Some("slskd"), Some(5)));
        assert_eq!(std::fs::read_to_string(root.join("recipes").join("ffmpeg.json")).unwrap(), "mine");
        assert!(migrate(&root).is_empty(), "once");
        let _ = std::fs::remove_dir_all(&root);
    }
}
