//! Roadie: installs, configures and runs the tools other apps need, from
//! declarative recipes. See `recipes/SCHEMA.md` for the format and
//! `api/` for the local API other apps and assistants talk to.
//!
//! Two processes share this binary. `roadie --serve` is the **service**
//! (`service.rs`): API, engine, request queue, updates, no window. Plain
//! `roadie` is the **window**: a Tauri client that starts the service if
//! needed, proves itself over the owner channel, relays the user's clicks
//! (`commands.rs` → `client.rs`) and mirrors the service's events.

pub mod actions;
pub mod api;
pub mod cli;
pub mod client;
pub mod commands;
pub mod consent;
#[cfg(test)]
mod e2e;
pub mod events;
pub mod mcp_setup;
pub mod owner;
pub mod paths;
pub mod recipe;
pub mod requests;
pub mod scheme;
pub mod service;
pub mod tools;

use tauri::Manager;

pub fn run() {
    let args: Vec<String> = std::env::args().collect();
    // Service / legacy launcher: no window, no plugins, exit with a code.
    if let Some(code) = cli::maybe_run(&args) {
        std::process::exit(code);
    }
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            // A second launch (deep link on Windows/Linux, or the user
            // opening the app again) lands here: forward and focus.
            for a in argv.iter().filter(|a| a.starts_with("roadie://")) {
                scheme::handle(app, a);
            }
            scheme::focus_window(app);
        }))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(commands::handler())
        .setup(move |app| {
            let data_root = match cli::data_dir_arg(&args) {
                Some(d) => d,
                None => app.path().app_data_dir().map_err(|e| format!("app data dir: {e}"))?,
            };
            std::fs::create_dir_all(&data_root)?;
            paths::init(data_root.clone());

            let handle = app.handle().clone();
            // Events from the service (and the few the window raises itself)
            // reach the webview through one emitter.
            {
                let h = handle.clone();
                events::set_emitter(Box::new(move |name, payload| {
                    use tauri::Emitter;
                    match name {
                        "focus-request" => scheme::focus_window(&h),
                        "open-url" => {
                            if let Some(url) = payload.get("url").and_then(|u| u.as_str()) {
                                use tauri_plugin_opener::OpenerExt;
                                let _ = h.opener().open_url(url.to_string(), None::<&str>);
                            }
                        }
                        _ => {}
                    }
                    let _ = h.emit(name, payload);
                }));
            }

            // Connect to (or start) the service, then keep its events flowing.
            match client::connect(&data_root) {
                Ok(c) => log::info!("connected to the Roadie service {} (pid {}) on port {}", c.service_version, c.service_pid, c.port),
                Err(e) => log::error!("could not reach the Roadie service: {e}"),
            }
            client::run_event_pump(|name, payload| events::emit(name, payload));

            // Deep links that arrive while running.
            {
                use tauri_plugin_deep_link::DeepLinkExt;
                let h = handle.clone();
                app.deep_link().on_open_url(move |event| {
                    for url in event.urls() {
                        scheme::handle(&h, url.as_str());
                    }
                });
            }
            // The one that launched us (Windows/Linux pass it in argv).
            for a in args.iter().skip(1).filter(|a| a.starts_with("roadie://")) {
                scheme::handle(&handle, a);
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building Roadie")
        .run(|app, event| {
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Opened { urls } = &event {
                for url in urls {
                    scheme::handle(app, url.as_str());
                }
            }
            let _ = (app, &event);
        });
}
