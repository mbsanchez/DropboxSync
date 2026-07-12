use std::fs;
use std::sync::atomic::Ordering;
use std::time::Duration as StdDuration;

use chrono::Utc;
use tauri::{AppHandle, Manager};

use crate::auth::oauth::start_oauth;
use crate::auth::oauth_complete::complete_oauth_internal;
use crate::auth_session::{
    current_tray_status_label, has_stored_credentials, is_hard_auth_failure,
    update_tray_tooltip, verify_dropbox_token_internal,
};
use crate::cloudsc_ops::{
    hydrate_cloudsc_placeholder_internal,
    index_materialized_folders_as_cloudsc_placeholders_internal,
};
use crate::dropbox_transfer::{download_remote_file_internal, hydrate_remote_folder_internal};
use crate::models::*;
use crate::oauth_listener::{start_oauth_callback_listener, stop_oauth_listener};
use crate::state::AppState;
use crate::sync::engine::SyncStatus;
use crate::sync_pipeline::{
    process_sync_queue_internal, refresh_queue_depth_internal, run_sync_tick_internal,
    scan_local_changes_internal,
};

#[tauri::command]
pub fn get_selective_sync_filters(
    state: tauri::State<AppState>,
) -> Result<SelectiveSyncFilters, String> {
    let include_csv = state
        .db
        .get_include_prefixes_csv()?
        .unwrap_or_default();
    let exclude_csv = state
        .db
        .get_exclude_prefixes_csv()?
        .unwrap_or_default();
    Ok(SelectiveSyncFilters {
        include_csv,
        exclude_csv,
    })
}

#[tauri::command]
pub fn set_selective_sync_filters(
    state: tauri::State<AppState>,
    include_csv: String,
    exclude_csv: String,
) -> Result<(), String> {
    state.db.set_include_prefixes_csv(&include_csv)?;
    state.db.set_exclude_prefixes_csv(&exclude_csv)?;
    Ok(())
}

#[tauri::command]
pub fn start_oauth_flow(
    app: AppHandle,
    state: tauri::State<AppState>,
) -> Result<OauthStartResponse, String> {
    let (auth_url, oauth_state, verifier) = start_oauth()?;
    {
        let mut engine = state
            .sync_engine
            .lock()
            .map_err(|_| "sync engine lock poisoned".to_string())?;
        engine.set_oauth_context(oauth_state.clone(), verifier);
    }
    start_oauth_callback_listener(
        app,
        state.inner().clone(),
        state.oauth_listener.clone(),
    )?;

    Ok(OauthStartResponse {
        auth_url,
        state: oauth_state,
    })
}

#[tauri::command]
pub async fn complete_oauth_flow(
    app_state: tauri::State<'_, AppState>,
    code: String,
    state: String,
) -> Result<(), String> {
    complete_oauth_internal(app_state.inner(), code, state).await
}

#[tauri::command]
pub fn cancel_oauth_flow(state: tauri::State<AppState>) -> Result<(), String> {
    stop_oauth_listener(&state.oauth_listener)?;
    let mut engine = state
        .sync_engine
        .lock()
        .map_err(|_| "sync engine lock poisoned".to_string())?;
    engine.clear_oauth_pending();
    Ok(())
}

#[tauri::command]
pub fn set_sync_folder(state: tauri::State<AppState>, folder: String) -> Result<(), String> {
    fs::create_dir_all(&folder).map_err(|e| format!("failed to create sync folder: {e}"))?;
    let prev = state.db.get_sync_folder()?.unwrap_or_default();
    state.db.set_sync_folder(&folder)?;
    if prev != folder {
        state.db.reset_sync_state()?;
    }
    let mut engine = state
        .sync_engine
        .lock()
        .map_err(|_| "sync engine lock poisoned".to_string())?;
    engine.set_tracked_path(folder);
    Ok(())
}

#[tauri::command]
pub fn pick_sync_folder_dialog() -> Result<Option<String>, String> {
    let picked = rfd::FileDialog::new().pick_folder();
    Ok(picked.map(|p| p.to_string_lossy().to_string()))
}

/// Same logic as [`get_startup_requirements`] for use from Rust (e.g. window visibility).
pub(crate) fn compute_startup_requirements(state: &AppState) -> Result<StartupRequirementsResponse, String> {
    let sync_folder = state.db.get_sync_folder()?;
    let sync_folder_ok = sync_folder
        .as_ref()
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);

    // In-memory session (e.g. just completed OAuth): trust it for onboarding without a network probe.
    // Keychain reads can lag on some platforms; do not require `has_stored_credentials` here.
    if let Ok(cache) = state.token_cache.lock() {
        if cache.is_some() {
            return Ok(StartupRequirementsResponse {
                auth_ok: true,
                sync_folder_ok,
                sync_folder,
            });
        }
    }

    let has_creds = has_stored_credentials(state);
    let auth_ok = if !has_creds {
        false
    } else {
        match verify_dropbox_token_internal(state) {
            Ok(v) => v,
            Err(e) => !is_hard_auth_failure(&e),
        }
    };

    Ok(StartupRequirementsResponse {
        auth_ok,
        sync_folder_ok,
        sync_folder,
    })
}

/// When `true`, the main window should be visible (Connect Dropbox / folder setup). When `false`, the
/// app can stay tray-only (e.g. `.cloudsc` open or background sync without flashing an empty UI).
pub(crate) fn should_show_main_window_for_onboarding(state: &AppState) -> bool {
    match compute_startup_requirements(state) {
        Ok(r) => !r.auth_ok || !r.sync_folder_ok,
        Err(_) => true,
    }
}

#[tauri::command]
pub fn get_startup_requirements(
    state: tauri::State<AppState>,
) -> Result<StartupRequirementsResponse, String> {
    compute_startup_requirements(state.inner())
}

#[tauri::command]
pub fn start_background_scheduler(
    state: tauri::State<AppState>,
    app: AppHandle,
) -> Result<bool, String> {
    {
        let mut started = state
            .scheduler_started
            .lock()
            .map_err(|_| "scheduler lock poisoned".to_string())?;
        if *started {
            return Ok(false);
        }
        *started = true;
    }

    let app_state = state.inner().clone();
    std::thread::spawn(move || loop {
        update_tray_tooltip(&app, &current_tray_status_label(&app_state));
        let ready = app_state
            .db
            .get_sync_folder()
            .ok()
            .flatten()
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false);
        if ready
            && app_state
                .sync_running
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            // Run the sync tick first so a local folder deletion is detected,
            // its `delete` job enqueued, and drained within this same tick
            // (deleting the folder remotely) before discovery runs. Otherwise
            // discovery would still see the (now-orphaned) remote folder and
            // re-create its `.cloudsc` placeholder.
            let _ = run_sync_tick_internal(&app_state);
            match index_materialized_folders_as_cloudsc_placeholders_internal(&app_state) {
                Ok(n) if n > 0 => tracing::info!(count = n, "indexed new remote placeholder(s)"),
                Ok(_) => {}
                Err(e) => tracing::error!(error = %e, "remote placeholder indexing failed"),
            }
            app_state.sync_running.store(false, Ordering::Release);
            update_tray_tooltip(&app, &current_tray_status_label(&app_state));
        }
        std::thread::sleep(StdDuration::from_secs(60));
    });

    Ok(true)
}

#[tauri::command]
pub fn hide_main_window(app: AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("main") {
        window.hide().map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
pub fn show_main_window(app: AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("main") {
        window.show().map_err(|e| e.to_string())?;
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
    Ok(())
}

#[tauri::command]
pub fn get_sync_status(state: tauri::State<AppState>) -> Result<SyncStatus, String> {
    refresh_queue_depth_internal(state.inner())?;
    let engine = state
        .sync_engine
        .lock()
        .map_err(|_| "sync engine lock poisoned".to_string())?;
    Ok(engine.current_status(state.sync_running.load(Ordering::Acquire)))
}

#[tauri::command]
pub fn get_sync_dashboard(state: tauri::State<AppState>) -> Result<SyncDashboard, String> {
    refresh_queue_depth_internal(state.inner())?;
    let jobs = state.db.list_recent_jobs(25)?;
    let conflicts = state.db.list_recent_conflicts(10)?;

    let status = state
        .sync_engine
        .lock()
        .map_err(|_| "sync engine lock poisoned".to_string())?
        .current_status(state.sync_running.load(Ordering::Acquire));

    Ok(SyncDashboard {
        status,
        jobs,
        conflicts,
    })
}

/// Reset all permanently-failed jobs back to `queued` so the next tick retries them.
#[tauri::command]
pub fn retry_failed_jobs(state: tauri::State<AppState>) -> Result<usize, String> {
    let requeued = state.db.requeue_failed_jobs()?;
    refresh_queue_depth_internal(state.inner())?;
    Ok(requeued)
}

#[tauri::command]
pub fn scan_local_changes(state: tauri::State<AppState>) -> Result<usize, String> {
    scan_local_changes_internal(state.inner())
}

#[tauri::command]
pub fn process_sync_queue(state: tauri::State<AppState>) -> Result<bool, String> {
    process_sync_queue_internal(state.inner())
}

#[tauri::command]
pub fn sync_tick(state: tauri::State<AppState>) -> Result<SyncTickResult, String> {
    // Delegate to the shared implementation so this IPC entry point drains the
    // queue in batches too (DBSYNC-10), instead of only one job per call.
    run_sync_tick_internal(state.inner())
}

#[tauri::command]
pub fn trigger_sync_tick(state: tauri::State<AppState>) -> Result<TriggerSyncResponse, String> {
    if state
        .sync_running
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Ok(TriggerSyncResponse { accepted: false });
    }

    let app_state = state.inner().clone();
    std::thread::spawn(move || {
        let result = run_sync_tick_internal(&app_state);
        if let Err(err) = result {
            if let Ok(mut engine) = app_state.sync_engine.lock() {
                engine.set_last_error(format!("sync tick failed: {err}"));
            }
        }
        app_state.sync_running.store(false, Ordering::Release);
    });

    Ok(TriggerSyncResponse { accepted: true })
}

#[tauri::command]
pub fn list_remote_folder(
    state: tauri::State<AppState>,
    path: String,
) -> Result<ListRemoteFolderResponse, String> {
    crate::dropbox_transfer::list_remote_folder(state.inner(), path)
}

#[tauri::command]
pub fn index_remote_root_placeholders(state: tauri::State<AppState>) -> Result<usize, String> {
    // Now recurses into every materialized (real) folder, not just the root, so
    // new remote content in already-hydrated folders is discovered too.
    index_materialized_folders_as_cloudsc_placeholders_internal(state.inner())
}

#[tauri::command]
pub fn list_cloudsc_placeholders(
    state: tauri::State<AppState>,
    limit: usize,
) -> Result<Vec<CloudscPlaceholderInfo>, String> {
    crate::cloudsc_ops::list_cloudsc_placeholders(state.inner(), limit)
}

#[tauri::command]
pub fn trigger_hydrate_cloudsc_placeholder(
    state: tauri::State<AppState>,
    placeholder_local_rel_path: String,
) -> Result<TriggerActionResponse, String> {
    if state
        .sync_running
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Ok(TriggerActionResponse { accepted: false });
    }

    let app_state = state.inner().clone();
    std::thread::spawn(move || {
        let result = hydrate_cloudsc_placeholder_internal(&app_state, &placeholder_local_rel_path);
        if let Err(err) = result {
            if let Ok(mut engine) = app_state.sync_engine.lock() {
                engine.set_last_error(format!("hydrate failed: {err}"));
            }
        } else if let Ok(mut engine) = app_state.sync_engine.lock() {
            engine.clear_last_error();
        }
        if let Ok(mut engine) = app_state.sync_engine.lock() {
            engine.set_last_scan_at(Utc::now().to_rfc3339());
        }
        app_state.sync_running.store(false, Ordering::Release);
    });

    Ok(TriggerActionResponse { accepted: true })
}

#[tauri::command]
pub fn trigger_download_remote_file(
    state: tauri::State<AppState>,
    path_display: String,
) -> Result<TriggerActionResponse, String> {
    if state
        .sync_running
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Ok(TriggerActionResponse { accepted: false });
    }

    let app_state = state.inner().clone();
    std::thread::spawn(move || {
        let result = download_remote_file_internal(&app_state, &path_display);
        if let Err(err) = result {
            if let Ok(mut engine) = app_state.sync_engine.lock() {
                engine.set_last_error(format!("download failed: {err}"));
            }
        }
        if let Ok(mut engine) = app_state.sync_engine.lock() {
            engine.set_last_scan_at(Utc::now().to_rfc3339());
        }
        app_state.sync_running.store(false, Ordering::Release);
    });

    Ok(TriggerActionResponse { accepted: true })
}

#[tauri::command]
pub fn trigger_hydrate_remote_folder(
    state: tauri::State<AppState>,
    folder_path_display: String,
) -> Result<TriggerActionResponse, String> {
    if state
        .sync_running
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Ok(TriggerActionResponse { accepted: false });
    }

    let app_state = state.inner().clone();
    std::thread::spawn(move || {
        let result = hydrate_remote_folder_internal(&app_state, &folder_path_display);
        if let Err(err) = result {
            if let Ok(mut engine) = app_state.sync_engine.lock() {
                engine.set_last_error(format!("hydrate failed: {err}"));
            }
        } else if let Ok(mut engine) = app_state.sync_engine.lock() {
            engine.set_last_error("".to_string());
        }
        if let Ok(mut engine) = app_state.sync_engine.lock() {
            engine.set_last_scan_at(Utc::now().to_rfc3339());
        }
        app_state.sync_running.store(false, Ordering::Release);
    });

    Ok(TriggerActionResponse { accepted: true })
}
