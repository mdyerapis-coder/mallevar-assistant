fn main() {
    // The three typed core operations are the ONLY custom commands this app
    // exposes. Declaring them here (and only here) is what generates their
    // allow/deny ACL entries; the capability file then lists exactly those.
    tauri_build::try_build(
        tauri_build::Attributes::new().app_manifest(
            tauri_build::AppManifest::new().commands(&["connect_core", "core_request", "subscribe_events"]),
        ),
    )
    .expect("failed to run tauri-build")
}
