//! Test fixtures: copies of the catalog's first three recipes, so tests have
//! real recipes without the network. Never installable, never listed; the
//! catalog repo (`outcast1000/roadie-recipes`) holds the real ones and its CI
//! checks that every one validates.

use super::Recipe;

const FIXTURES: &[(&str, &str)] = &[
    ("slskd", include_str!("slskd.json")),
    ("yt-dlp", include_str!("yt-dlp.json")),
    ("ffmpeg", include_str!("ffmpeg.json")),
    // Revision 6, for the catalog: a shared key, install-time ports and key,
    // a configuration slskd owns after install.
    ("slskd@6", include_str!("slskd-r6.json")),
];

pub fn json(name: &str) -> &'static str {
    FIXTURES.iter().find(|(n, _)| *n == name).unwrap_or_else(|| panic!("no fixture {name}")).1
}

pub fn recipe(name: &str) -> Recipe {
    super::parse(json(name)).unwrap_or_else(|e| panic!("fixture {name} is invalid: {e:?}"))
}

/// One recipe per name (the catalog's current revisions).
pub fn all() -> Vec<Recipe> {
    FIXTURES.iter().filter(|(n, _)| !n.contains('@')).map(|(n, _)| recipe(n)).collect()
}
