fn main() {
    // The CLI-only build (`--no-default-features`) has no window to bundle.
    #[cfg(feature = "window")]
    tauri_build::build();
}
