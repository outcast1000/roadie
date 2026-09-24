//! End-to-end probe against the real network and a real tool: install the
//! latest release into a temp data root, start it, wait for health, stop it
//! through the ladder, uninstall. `#[ignore]`d because it downloads tens of
//! megabytes and spawns a daemon:
//!
//!   cd src-tauri && cargo test --lib tools::probe -- --ignored --nocapture
//!
//! For slskd it fails with `standaloneRunning` if another slskd is running
//! on this machine (its singleton mutex is machine-wide).

use super::*;

#[test]
#[ignore]
fn probe_slskd_install_start_stop() {
    let root = std::env::temp_dir().join(format!("roadie-probe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    paths::init(root.clone());
    let recipe = recipe::load_builtin().remove(0);

    let t0 = Instant::now();
    let st = install(&recipe, &mut |phase, done, total| eprintln!("  {phase:?} {done} / {total:?}")).expect("install");
    eprintln!("installed {:?} in {:?}", st.version, t0.elapsed());
    assert!(st.installed && st.version.is_some() && !st.running);

    let mut patch = Map::new();
    patch.insert("downloadsDir".into(), Value::String(root.join("downloads").to_string_lossy().into_owned()));
    let st = configure(&recipe, &patch).expect("configure");
    assert!(st.config["downloadsDir"].as_str().unwrap().contains("roadie-probe"));

    let st = start(&recipe, "probe").expect("start");
    eprintln!("started: running={} healthy={} starting={} pid={:?}", st.running, st.healthy, st.starting, st.pid);
    assert!(st.running, "conflict: {:?} {:?}", st.conflict, st.conflict_detail);
    assert!(root.join("downloads").join(".incomplete").is_dir());
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut healthy = st.healthy;
    while !healthy && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(500));
        healthy = status(&recipe).healthy;
    }
    let full = status(&recipe);
    assert!(healthy, "daemon never answered:\n{}", log_tail(&recipe, 30).unwrap());
    eprintln!("healthy: url={:?} reported={:?} details={:?}", full.url, full.reported_version, full.details);
    assert!(full.url.as_deref().unwrap().starts_with("http://127.0.0.1:"));

    let st = stop(&recipe).expect("stop");
    assert!(!st.running);
    assert!(!process::pid_path(&paths::tool_paths("slskd").unwrap().data).exists());

    uninstall(&recipe, false).expect("uninstall");
    assert!(!paths::tool_paths("slskd").unwrap().root.exists());
    let _ = std::fs::remove_dir_all(&root);
}
