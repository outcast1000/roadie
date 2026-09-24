// Prevents an extra console window on Windows in release builds of the
// desktop app. The CLI-only build stays a console program so it can print.
#![cfg_attr(all(not(debug_assertions), feature = "window"), windows_subsystem = "windows")]

fn main() {
    roadie_lib::run()
}
