//! Command-line modes, intercepted at the very top of `run()` before the
//! Tauri builder:
//!
//! - `roadie --serve [--data-dir <dir>]` — the background service
//!   (`service.rs`). Login items and the window start it this way.
//! - `roadie --start-tool <name> --data-dir <dir>` — what login items from
//!   an older Roadie run. It now just makes sure the service is up (the
//!   service starts the daemons marked "start at login") and exits.

use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Serve { data_dir: Option<PathBuf> },
    LegacyStartTool { data_dir: Option<PathBuf> },
}

pub fn maybe_run(args: &[String]) -> Option<i32> {
    let mode = parse(args)?;
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    Some(match mode {
        Mode::Serve { data_dir } => crate::service::run(data_dir.unwrap_or_else(crate::paths::default_data_root)),
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
    })
}

pub fn parse(args: &[String]) -> Option<Mode> {
    let mut serve = false;
    let mut start_tool = false;
    let mut dir = None;
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--serve" => serve = true,
            "--start-tool" => {
                start_tool = true;
                let _ = it.next();
            }
            "--data-dir" => dir = it.next().map(PathBuf::from),
            _ => {}
        }
    }
    if serve {
        Some(Mode::Serve { data_dir: dir })
    } else if start_tool {
        Some(Mode::LegacyStartTool { data_dir: dir })
    } else {
        None
    }
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
        let unrelated = vec!["roadie".into(), "roadie://open/slskd".into()];
        assert_eq!(parse(&unrelated), None);
        assert_eq!(maybe_run(&unrelated), None);
    }
}
