fn main() {
    // `finder_extension.rs` sends a message to `FIFinderSyncController`, which lives in
    // FinderSync.framework. Without this the class is simply not registered at runtime and the
    // lookup returns None — a silent "not applicable" rather than a link error (DBSYNC-86).
    #[cfg(target_os = "macos")]
    println!("cargo:rustc-link-lib=framework=FinderSync");

    // `file_provider.rs` looks up `NSFileProviderManager` / `NSFileProviderDomain`, which live in
    // FileProvider.framework. Without this the classes are not registered and `AnyClass::get` returns
    // None, a silent failure rather than a link error (DBSYNC-95).
    #[cfg(target_os = "macos")]
    println!("cargo:rustc-link-lib=framework=FileProvider");

    // `app_group.rs` bakes the App Group id in with `option_env!("APPLE_TEAM_ID")` (DBSYNC-97). Cargo
    // already tracks `option_env!` for the crate being compiled, so this line is belt and braces: it
    // also re-runs this build script, and keeps the dependency visible in one place.
    println!("cargo:rerun-if-env-changed=APPLE_TEAM_ID");

    tauri_build::build()
}
