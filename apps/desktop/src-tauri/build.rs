fn main() {
    // `finder_extension.rs` sends a message to `FIFinderSyncController`, which lives in
    // FinderSync.framework. Without this the class is simply not registered at runtime and the
    // lookup returns None — a silent "not applicable" rather than a link error (DBSYNC-86).
    #[cfg(target_os = "macos")]
    println!("cargo:rustc-link-lib=framework=FinderSync");

    tauri_build::build()
}
