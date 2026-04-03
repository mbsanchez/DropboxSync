use std::fs;
use std::time::Duration as StdDuration;

use chrono::{Duration, Utc};
use tauri::AppHandle;
use tauri::Manager;

use crate::auth::oauth::{complete_oauth, start_oauth};
use crate::auth_session::{
    current_tray_status_label, has_stored_credentials, is_hard_auth_failure,
    update_tray_tooltip, verify_dropbox_token_internal,
};
use crate::cloudsc_ops::{
    hydrate_cloudsc_placeholder_internal, index_remote_root_children_as_cloudsc_placeholders_internal,
};
use crate::dropbox_transfer::{download_remote_file_internal, hydrate_remote_folder_internal};
use crate::models::*;
use crate::oauth_listener::start_oauth_callback_listener;
use crate::state::AppState;
use crate::storage;
use crate::sync::engine::{OauthCallbackPayload, SyncStatus};
use crate::sync_pipeline::{
    process_sync_queue_internal, refresh_queue_depth_internal, run_sync_tick_internal,
    scan_local_changes_internal,
};

async fn complete_oauth_internal(
    app_state: &AppState,
    code: String,
    state: String,
) -> Result<(), String> {
    let (expected_state, verifier) = {
        let engine = app_state
            .sync_engine
            .lock()
            .map_err(|_| "sync engine lock poisoned".to_string())?;
        (
            engine.pending_oauth_state().unwrap_or_default(),
            engine.pending_pkce_verifier().unwrap_or_default(),
        )
    };

    if expected_state != state {
        return Err("invalid oauth state".to_string());
    }

    let token = complete_oauth(code, verifier).await?;

    let expires_at = token
        .expires_in
        .map(|in_s| (Utc::now() + Duration::seconds(in_s)).to_rfc3339());
    let session = storage::secure_store::TokenSession {
        access_token: token.access_token.clone(),
        refresh_token: token.refresh_token.clone(),
        expires_at,
    };

    app_state
        .secure_store
        .store_session(&session)
        .map_err(|e| format!("failed to store token session: {e}"))?;

    if let Ok(mut cache) = app_state.token_cache.lock() {
        *cache = Some(session);
    }

    Ok(())
}

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
pub fn start_oauth_flow(state: tauri::State<AppState>) -> Result<OauthStartResponse, String> {
    let (auth_url, oauth_state, verifier) = start_oauth()?;
    {
        let mut engine = state
            .sync_engine
            .lock()
            .map_err(|_| "sync engine lock poisoned".to_string())?;
        engine.set_oauth_context(oauth_state.clone(), verifier);
    }
    start_oauth_callback_listener(state.sync_engine.clone())?;

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
pub async fn complete_oauth_from_callback(
    app_state: tauri::State<'_, AppState>,
) -> Result<bool, String> {
    let callback = {
        let mut engine = app_state
            .sync_engine
            .lock()
            .map_err(|_| "sync engine lock poisoned".to_string())?;
        engine.consume_oauth_callback()
    };
    let Some(payload) = callback else {
        return Ok(false);
    };
    complete_oauth_internal(app_state.inner(), payload.code, payload.state).await?;
    Ok(true)
}

#[tauri::command]
pub fn poll_oauth_callback(
    state: tauri::State<AppState>,
) -> Result<Option<OauthCallbackPayload>, String> {
    let mut engine = state
        .sync_engine
        .lock()
        .map_err(|_| "sync engine lock poisoned".to_string())?;
    Ok(engine.consume_oauth_callback())
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

#[tauri::command]
pub fn get_startup_requirements(
    state: tauri::State<AppState>,
) -> Result<StartupRequirementsResponse, String> {
    let sync_folder = state.db.get_sync_folder()?;
    let sync_folder_ok = sync_folder
        .as_ref()
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);

    let has_creds = has_stored_credentials(state.inner());
    let auth_ok = if !has_creds {
        false
    } else {
        match verify_dropbox_token_internal(state.inner()) {
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
        if ready {
            let can_run = app_state
                .sync_engine
                .lock()
                .map(|e| !e.is_sync_running())
                .unwrap_or(false);
            if can_run {
                if let Ok(mut engine) = app_state.sync_engine.lock() {
                    engine.set_sync_running(true);
                }
                let _ = index_remote_root_children_as_cloudsc_placeholders_internal(&app_state);
                let _ = run_sync_tick_internal(&app_state);
                if let Ok(mut engine) = app_state.sync_engine.lock() {
                    engine.set_sync_running(false);
                }
                update_tray_tooltip(&app, &current_tray_status_label(&app_state));
            }
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
pub fn get_sync_status(state: tauri::State<AppState>) -> Result<SyncStatus, String> {
    refresh_queue_depth_internal(state.inner())?;
    let engine = state
        .sync_engine
        .lock()
        .map_err(|_| "sync engine lock poisoned".to_string())?;
    Ok(engine.current_status())
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
        .current_status();

    Ok(SyncDashboard {
        status,
        jobs,
        conflicts,
    })
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
    let enqueued_jobs = scan_local_changes_internal(state.inner())?;
    let processed_job = process_sync_queue_internal(state.inner())?;

    let scanned_files = state.db.list_local_files()?.len();

    Ok(SyncTickResult {
        scanned_files,
        enqueued_jobs,
        processed_job,
    })
}

#[tauri::command]
pub fn trigger_sync_tick(state: tauri::State<AppState>) -> Result<TriggerSyncResponse, String> {
    {
        let mut engine = state
            .sync_engine
            .lock()
            .map_err(|_| "sync engine lock poisoned".to_string())?;
        if engine.is_sync_running() {
            return Ok(TriggerSyncResponse { accepted: false });
        }
        engine.set_sync_running(true);
    }

    let app_state = state.inner().clone();
    std::thread::spawn(move || {
        let result = run_sync_tick_internal(&app_state);
        if let Err(err) = result {
            if let Ok(mut engine) = app_state.sync_engine.lock() {
                engine.set_last_error(format!("sync tick failed: {err}"));
            }
        }
        if let Ok(mut engine) = app_state.sync_engine.lock() {
            engine.set_sync_running(false);
        }
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
    index_remote_root_children_as_cloudsc_placeholders_internal(state.inner())
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
    {
        let mut engine = state
            .sync_engine
            .lock()
            .map_err(|_| "sync engine lock poisoned".to_string())?;
        if engine.is_sync_running() {
            return Ok(TriggerActionResponse { accepted: false });
        }
        engine.set_sync_running(true);
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
            engine.set_sync_running(false);
        }
    });

    Ok(TriggerActionResponse { accepted: true })
}

#[tauri::command]
pub fn trigger_download_remote_file(
    state: tauri::State<AppState>,
    path_display: String,
) -> Result<TriggerActionResponse, String> {
    {
        let mut engine = state
            .sync_engine
            .lock()
            .map_err(|_| "sync engine lock poisoned".to_string())?;
        if engine.is_sync_running() {
            return Ok(TriggerActionResponse { accepted: false });
        }
        engine.set_sync_running(true);
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
            engine.set_sync_running(false);
        }
    });

    Ok(TriggerActionResponse { accepted: true })
}

#[tauri::command]
pub fn trigger_hydrate_remote_folder(
    state: tauri::State<AppState>,
    folder_path_display: String,
) -> Result<TriggerActionResponse, String> {
    {
        let mut engine = state
            .sync_engine
            .lock()
            .map_err(|_| "sync engine lock poisoned".to_string())?;
        if engine.is_sync_running() {
            return Ok(TriggerActionResponse { accepted: false });
        }
        engine.set_sync_running(true);
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
            engine.set_sync_running(false);
        }
    });

    Ok(TriggerActionResponse { accepted: true })
}
