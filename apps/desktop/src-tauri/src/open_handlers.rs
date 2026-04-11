use std::path::{Path, PathBuf};

use tauri::AppHandle;
use tauri::Manager;

use crate::path_util::relpath_under;
use crate::state::AppState;
use crate::sync_pipeline::{process_sync_queue_internal, refresh_queue_depth_internal};

pub(crate) fn resolve_cloudsc_rel_path(state: &AppState, abs: &Path) -> Result<String, String> {
    let sync_folder_str = state
        .db
        .get_sync_folder()?
        .ok_or_else(|| "sync folder not configured".to_string())?;
    let sync_folder = PathBuf::from(&sync_folder_str);
    let abs_canon = abs
        .canonicalize()
        .map_err(|e| format!("invalid file path: {e}"))?;
    let sync_canon = sync_folder
        .canonicalize()
        .map_err(|e| format!("invalid sync folder: {e}"))?;
    let rel = relpath_under(&sync_canon, &abs_canon)?;
    let rel = rel.replace('\\', "/");
    if !rel.ends_with(".cloudsc") {
        return Err("not a .cloudsc placeholder".to_string());
    }
    Ok(rel)
}

pub(crate) fn spawn_drain_sync_queue_if_idle(app_state: AppState) {
    std::thread::spawn(move || {
        {
            let mut engine = match app_state.sync_engine.lock() {
                Ok(e) => e,
                Err(_) => return,
            };
            if engine.is_sync_running() {
                return;
            }
            engine.set_sync_running(true);
        }
        let mut safety = 0usize;
        while safety < 1000 {
            safety += 1;
            match process_sync_queue_internal(&app_state) {
                Ok(true) => continue,
                Ok(false) => break,
                Err(e) => {
                    eprintln!("process_sync_queue (cloudsc open): {e}");
                    break;
                }
            }
        }
        if let Ok(mut engine) = app_state.sync_engine.lock() {
            engine.set_sync_running(false);
        }
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
        eprintln!("handle_cloudsc_paths_from_os: AppState not available");
        return;
    };
    let app_state = state.inner().clone();
    let mut any_queued = false;
    for path in paths {
        match resolve_cloudsc_rel_path(&app_state, &path) {
            Ok(rel) => {
                if let Err(e) = app_state
                    .db
                    .enqueue_job("hydrate_cloudsc", Some(rel.as_str()), None)
                {
                    eprintln!("enqueue hydrate_cloudsc {rel}: {e}");
                    continue;
                }
                if let Err(e) = refresh_queue_depth_internal(&app_state) {
                    eprintln!("refresh_queue_depth: {e}");
                }
                eprintln!("queued hydrate_cloudsc for {rel}");
                any_queued = true;
            }
            Err(e) => {
                eprintln!(
                    "skip path {} (open/drop cloudsc): {e}",
                    path.display()
                );
            }
        }
    }
    if any_queued {
        spawn_drain_sync_queue_if_idle(app_state);
    }
}
