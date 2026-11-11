//! Which sync model this build runs (DBSYNC-98, ADR-0004).
//!
//! On macOS the File Provider domain replaces the sync folder: the system owns the files under
//! `~/Library/CloudStorage`, the extension reports every change through the exchange, and nothing
//! of the folder pipeline may run next to it. That pipeline is the filesystem watcher, the local
//! scan and its rename heuristics, the DBSYNC-111 placeholder folders, `.cloudsc` sidecars and the
//! `hydrate_cloudsc` jobs. Windows keeps it.
//!
//! Each entry point of that pipeline asks [`sidecar_pipeline`] first and returns at once when it is
//! off. The answer is explicit and never inferred from `sync_folder` being unset, so a stray value
//! left by an older build cannot switch any of it back on.
//!
//! Unit tests run on the development Mac and exercise the pipeline there, so under `cfg(test)` it is
//! on unless a test turns it off with [`without_sidecar_pipeline`] to check a gate.

/// What a refused sync-folder operation answers when the pipeline is off.
pub(crate) const NO_SIDECARS: &str = "this build has no sync folder: Finder manages the files";

/// Whether this build runs the sync-folder pipeline. Off in a macOS build, on everywhere else.
pub(crate) fn sidecar_pipeline() -> bool {
    #[cfg(all(target_os = "macos", not(test)))]
    {
        false
    }
    #[cfg(test)]
    {
        !tests::OFF.with(std::cell::Cell::get)
    }
    #[cfg(not(any(target_os = "macos", test)))]
    {
        true
    }
}

/// Forgets a sync folder when this build has no pipeline to use it: an install from before the cutover
/// (DBSYNC-98). What the folder pipeline knew goes with it, so nothing synced from the folder is taken
/// for the domain's. The domain's own state stays: the projection, the pins and the delta cursor, which
/// a Mac that ran both before the cutover still needs. The folder itself stays on disk, untouched: no
/// migration or copy (DBSYNC-98). The folder's mass-deletion breaker goes on every launch, folder or
/// not, because nothing else clears it without the pipeline. `true` when there was a folder.
pub(crate) fn forget_sync_folder(db: &crate::storage::db::Db) -> crate::error::AppResult<bool> {
    if sidecar_pipeline() {
        return Ok(false);
    }
    crate::sync_pipeline::forget_sync_folder_breaker(db)?;
    if db.get_sync_folder()?.is_none() {
        return Ok(false);
    }
    db.forget_folder_state()?;
    db.clear_sync_folder()?;
    Ok(true)
}

/// Runs `f` with the pipeline off on this thread, as a macOS build has it.
#[cfg(test)]
pub(crate) fn without_sidecar_pipeline<R>(f: impl FnOnce() -> R) -> R {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            tests::OFF.with(|off| off.set(self.0));
        }
    }
    let _restore = Restore(tests::OFF.with(|off| off.replace(true)));
    f()
}

#[cfg(test)]
mod tests {
    thread_local! {
        pub(super) static OFF: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    /// Proof this test can fail: dropping the `sidecar_pipeline()` check forgets the Windows folder;
    /// dropping `forget_folder_state` keeps the row; calling `reset_sync_state` instead drops the cursor.
    #[test]
    fn a_build_without_the_pipeline_forgets_the_old_folder_and_what_was_synced_from_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = crate::storage::db::Db::new_at(&tmp.path().join("app.db")).expect("db");
        db.set_sync_folder("/Users/me/DropboxSync").unwrap();
        db.upsert_local_file("a.txt", "h", 1, 0).unwrap();
        db.set_app_config(crate::remote_index::REMOTE_DELTA_CURSOR_KEY, "c")
            .unwrap();

        assert!(
            !super::forget_sync_folder(&db).unwrap(),
            "with the pipeline: kept"
        );
        assert!(db.get_sync_folder().unwrap().is_some());

        super::without_sidecar_pipeline(|| {
            assert!(super::forget_sync_folder(&db).unwrap());
            assert!(!super::forget_sync_folder(&db).unwrap(), "once");
        });
        assert_eq!(db.get_sync_folder().unwrap(), None);
        assert!(db.list_local_files().unwrap().is_empty());
        assert_eq!(
            db.get_app_config(crate::remote_index::REMOTE_DELTA_CURSOR_KEY)
                .unwrap()
                .as_deref(),
            Some("c"),
            "the domain's cursor stays"
        );
    }

    #[test]
    fn tests_run_the_pipeline_unless_one_turns_it_off_and_it_comes_back_after() {
        assert!(super::sidecar_pipeline());
        super::without_sidecar_pipeline(|| assert!(!super::sidecar_pipeline()));
        assert!(super::sidecar_pipeline());
    }
}
