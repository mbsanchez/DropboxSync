use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use tauri::AppHandle;
use tauri::Manager;

use crate::error::{AppError, AppResult};
use crate::path_util::key_from_disk;
use crate::state::AppState;
use crate::sync_pipeline::{process_sync_queue_internal, refresh_queue_depth_internal};

pub(crate) fn resolve_cloudsc_rel_path(state: &AppState, abs: &Path) -> AppResult<String> {
    let sync_folder_str = state
        .db
        .get_sync_folder()?
        .ok_or_else(|| AppError::Sync("sync folder not configured".to_string()))?;
    let sync_folder = PathBuf::from(&sync_folder_str);
    let abs_canon = abs
        .canonicalize()
        .map_err(|e| AppError::Io(format!("invalid file path: {e}")))?;
    let sync_canon = sync_folder
        .canonicalize()
        .map_err(|e| AppError::Io(format!("invalid sync folder: {e}")))?;
    let rel = key_from_disk(&sync_canon, &abs_canon)?;
    if !rel.ends_with(".cloudsc") {
        return Err(AppError::Sync("not a .cloudsc placeholder".to_string()));
    }
    Ok(rel)
}

pub(crate) fn spawn_drain_sync_queue_if_idle(app_state: AppState) {
    std::thread::spawn(move || {
        if app_state
            .sync_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        crate::auth_session::refresh_tray_tooltip(&app_state);
        let mut safety = 0usize;
        while safety < 1000 {
            safety += 1;
            match process_sync_queue_internal(&app_state) {
                Ok(true) => continue,
                Ok(false) => break,
                Err(e) => {
                    tracing::error!(error = %e, "process_sync_queue failed (cloudsc open)");
                    break;
                }
            }
        }
        app_state.sync_running.store(false, Ordering::Release);
        crate::auth_session::refresh_tray_tooltip(&app_state);
    });
}

/// Collects existing `.cloudsc` file paths from a process argument list (argv), skipping the executable.
pub(crate) fn cloudsc_paths_from_argv(args: &[String]) -> Vec<PathBuf> {
    args.iter()
        .skip(1)
        .map(PathBuf::from)
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("cloudsc"))
        })
        .collect()
}

/// Same as [`cloudsc_paths_from_argv`] but uses `args_os` for the current process (Windows/Linux cold start).
pub(crate) fn cloudsc_paths_from_current_exe_args() -> Vec<PathBuf> {
    std::env::args_os()
        .skip(1)
        .map(PathBuf::from)
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("cloudsc"))
        })
        .collect()
}

pub(crate) fn handle_cloudsc_paths_from_os(app_handle: &AppHandle, paths: Vec<PathBuf>) {
    let Some(state) = app_handle.try_state::<AppState>() else {
        tracing::warn!("handle_cloudsc_paths_from_os: AppState not available");
        return;
    };
    let app_state = state.inner().clone();
    let mut any_queued = false;
    for path in paths {
        match resolve_cloudsc_rel_path(&app_state, &path) {
            Ok(rel) => {
                if let Err(e) =
                    app_state
                        .db
                        .enqueue_job("hydrate_cloudsc", Some(rel.as_str()), None)
                {
                    tracing::error!(file_path = %rel, error = %e, "enqueue hydrate_cloudsc failed");
                    continue;
                }
                if let Err(e) = refresh_queue_depth_internal(&app_state) {
                    tracing::error!(error = %e, "refresh_queue_depth failed");
                }
                tracing::info!(file_path = %rel, "queued hydrate_cloudsc");
                any_queued = true;
            }
            Err(e) => {
                tracing::warn!(
                    file_path = %path.display(),
                    error = %e,
                    "skip path (open/drop cloudsc)"
                );
            }
        }
    }
    if any_queued {
        spawn_drain_sync_queue_if_idle(app_state);
    }
}

#[cfg(test)]
mod tests {
    /// DBSYNC-103. The rel path of a double-clicked placeholder becomes a `hydrate_cloudsc`
    /// job key, so it must be NFC even when the placeholder's name is NFD on disk.
    ///
    /// Proof this test can fail: reverting `resolve_cloudsc_rel_path` to `relpath_under`
    /// returns the NFD name.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_placeholder_with_an_nfd_name_resolves_to_an_nfc_key() {
        use std::sync::atomic::AtomicBool;
        use std::sync::{Arc, Mutex};

        let root = tempfile::tempdir().expect("root");
        let db_dir = tempfile::tempdir().expect("db dir");
        let db = crate::storage::db::Db::new_at(&db_dir.path().join("app.db")).expect("db");
        db.set_sync_folder(&root.path().to_string_lossy())
            .expect("set folder");
        let state = crate::state::AppState {
            secure_store: crate::storage::secure_store::SecureStore::new(),
            db: Arc::new(db),
            sync_engine: Arc::new(Mutex::new(crate::sync::engine::SyncEngine::new())),
            token_cache: Arc::new(Mutex::new(None)),
            scheduler_started: Arc::new(Mutex::new(false)),
            oauth_listener: Arc::new(Mutex::new(None)),
            sync_running: Arc::new(AtomicBool::new(false)),
            token_refresh_lock: Arc::new(Mutex::new(())),
            http_client: crate::state::build_http_client(),
        };
        let placeholder = root.path().join("cafe\u{301}.txt.cloudsc");
        std::fs::write(&placeholder, b"{}").unwrap();

        assert_eq!(
            super::super::open_handlers::resolve_cloudsc_rel_path(&state, &placeholder).unwrap(),
            "caf\u{e9}.txt.cloudsc"
        );
    }
}
