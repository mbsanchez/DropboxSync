mod auth;
mod storage;
mod sync;

use auth::oauth::{complete_oauth, dropbox_redirect_uri, refresh_access_token_blocking, start_oauth};
use chrono::{Duration, Utc};
use reqwest::blocking::Client;
use serde::Deserialize;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;
use tauri::image::Image;
use tauri::Manager;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use storage::db::{ConflictRow, Db, FileIndexRow, SyncJobRow};
use storage::secure_store::SecureStore;
use sync::engine::{OauthCallbackPayload, SyncEngine, SyncStatus};
use url::Url;
use walkdir::WalkDir;

#[derive(Clone)]
struct AppState {
    secure_store: SecureStore,
    db: Arc<Db>,
    sync_engine: Arc<Mutex<SyncEngine>>,
    token_cache: Arc<Mutex<Option<storage::secure_store::TokenSession>>>,
    scheduler_started: Arc<Mutex<bool>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OauthStartResponse {
    auth_url: String,
    state: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SyncDashboard {
    status: SyncStatus,
    jobs: Vec<SyncJobRow>,
    conflicts: Vec<ConflictRow>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SyncTickResult {
    scanned_files: usize,
    enqueued_jobs: usize,
    processed_job: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TriggerSyncResponse {
    accepted: bool,
}

#[derive(Deserialize)]
struct DropboxListFolderResponse {
    entries: Vec<DropboxEntry>,
    cursor: String,
    has_more: bool,
}

#[derive(Deserialize)]
struct DropboxEntry {
    #[serde(rename = ".tag")]
    tag: String,
    path_display: Option<String>,
    content_hash: Option<String>,
    rev: Option<String>,
    server_modified: Option<String>,
    size: Option<i64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RemoteEntry {
    tag: String, // "file" | "folder"
    path_display: String,
    size: Option<i64>,
    is_synced: bool,
    is_excluded: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ListRemoteFolderResponse {
    current_path: String,
    entries: Vec<RemoteEntry>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TriggerActionResponse {
    accepted: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StartupRequirementsResponse {
    auth_ok: bool,
    sync_folder_ok: bool,
    sync_folder: Option<String>,
}

const CLOUDSC_MAGIC: &str = "CLOUDSC1";

#[derive(Serialize, Deserialize)]
struct CloudscMeta {
    version: u8,
    tag: String,                 // "file" | "folder"
    remote_path_display: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CloudscPlaceholderInfo {
    local_path_display: String, // path under sync folder (no extension changes)
    tag: String,                 // "file" | "folder"
    remote_path_display: String,
}

fn encode_cloudsc_meta(meta: &CloudscMeta) -> Result<Vec<u8>, String> {
    let json = serde_json::to_vec(meta).map_err(|e| format!("cloudsc encode json failed: {e}"))?;
    let mut out = Vec::new();
    out.extend_from_slice(CLOUDSC_MAGIC.as_bytes());
    out.push(b'\n');
    out.extend_from_slice(&json);
    Ok(out)
}

fn decode_cloudsc_meta(bytes: &[u8]) -> Result<CloudscMeta, String> {
    let magic = CLOUDSC_MAGIC.as_bytes();
    if bytes.len() < magic.len() + 1 {
        return Err("invalid .cloudsc payload (too small)".to_string());
    }
    if &bytes[..magic.len()] != magic {
        return Err("invalid .cloudsc payload (bad magic)".to_string());
    }
    let json_bytes = &bytes[magic.len() + 1..];
    serde_json::from_slice(json_bytes).map_err(|e| format!("cloudsc decode json failed: {e}"))
}

fn cloudsc_target_path(placeholder_path: &Path) -> PathBuf {
    // Remove the last extension ('.cloudsc'), keeping the original filename.
    // Example: 'a.txt.cloudsc' -> 'a.txt'
    placeholder_path.with_extension("")
}

fn should_ignore_local_path(relative: &str) -> bool {
    let p = relative.replace('\\', "/");
    p == ".DS_Store" || p.ends_with("/.DS_Store") || p.starts_with("._") || p.contains("/._")
}

fn relpath_under(sync_folder: &Path, absolute: &Path) -> Result<String, String> {
    Ok(absolute
        .strip_prefix(sync_folder)
        .map_err(|e| format!("failed to compute relative path: {e}"))?
        .to_string_lossy()
        .to_string())
}

fn normalize_dropbox_path(input: &str) -> String {
    if input.is_empty() {
        return "".to_string();
    }
    if input.starts_with('/') {
        input.to_string()
    } else {
        format!("/{}", input)
    }
}

fn parse_prefix_csv(csv: Option<String>) -> Vec<String> {
    match csv {
        None => Vec::new(),
        Some(s) => s
            .split(',')
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .map(|p| p.trim_start_matches('/').to_string())
            .collect(),
    }
}

fn is_path_allowed(relative: &str, include_prefixes: &[String], exclude_prefixes: &[String]) -> bool {
    // Include: if list is empty => allow all.
    let included = if include_prefixes.is_empty() {
        true
    } else {
        include_prefixes.iter().any(|p| relative.starts_with(p))
    };

    if !included {
        return false;
    }

    // Exclude: if any prefix matches => denied.
    if exclude_prefixes.iter().any(|p| relative.starts_with(p)) {
        return false;
    }

    true
}

fn has_stored_credentials(state: &AppState) -> bool {
    state.secure_store.get_session().is_ok() || state.secure_store.get_token().is_ok()
}

/// True when the user must sign in again (not transient network/keychain noise).
fn is_hard_auth_failure(err: &str) -> bool {
    err.contains("dropbox token expired and no refresh_token available")
        || err.contains("missing dropbox token session:")
        || err.contains("dropbox refresh token exchange failed with status")
}

fn get_access_token(state: &AppState) -> Result<String, String> {
    fn session_expired(session: &storage::secure_store::TokenSession) -> bool {
        let Some(expires_at) = session.expires_at.as_ref() else {
            return false; // sin info => asumimos válido
        };

        let expires_dt = chrono::DateTime::parse_from_rfc3339(expires_at)
            .map(|dt| dt.with_timezone(&Utc))
            .ok();

        let Some(expires_dt) = expires_dt else {
            return false; // si no parsea, asumimos válido
        };

        // Refrescamos un poco antes de expirar (skew).
        Utc::now() + Duration::seconds(60) >= expires_dt
    }

    // Evita prompts del Keychain al consultar el token en cada llamada.
    if let Ok(cache) = state.token_cache.lock() {
        if let Some(session) = cache.as_ref() {
            if !session_expired(session) {
                return Ok(session.access_token.clone());
            }
        }
    }

    let mut session = match state.secure_store.get_session() {
        Ok(s) => s,
        Err(_) => {
            // Backward compatibility: migrate legacy stored access token into session format.
            let legacy_token = state
                .secure_store
                .get_token()
                .map_err(|e| format!("missing dropbox token session: {e}"))?;
            let migrated = storage::secure_store::TokenSession {
                access_token: legacy_token,
                refresh_token: None,
                expires_at: None,
            };
            let _ = state.secure_store.store_session(&migrated);
            migrated
        }
    };

    if session_expired(&session) {
        if let Some(refresh_token) = session.refresh_token.as_deref() {
            let refreshed = refresh_access_token_blocking(refresh_token)?;

            let new_refresh_token = refreshed
                .refresh_token
                .or_else(|| session.refresh_token.clone());

            let new_expires_at = refreshed.expires_in.map(|in_s| {
                (Utc::now() + Duration::seconds(in_s)).to_rfc3339()
            });

            session = storage::secure_store::TokenSession {
                access_token: refreshed.access_token,
                refresh_token: new_refresh_token,
                expires_at: new_expires_at.or_else(|| session.expires_at.clone()),
            };

            state
                .secure_store
                .store_session(&session)
                .map_err(|e| format!("failed storing refreshed token session: {e}"))?;
        } else {
            return Err("dropbox token expired and no refresh_token available".to_string());
        }
    }

    if let Ok(mut cache) = state.token_cache.lock() {
        *cache = Some(session.clone());
    }

    Ok(session.access_token)
}

fn verify_dropbox_token_internal(state: &AppState) -> Result<bool, String> {
    let token = get_access_token(state)?;
    let response = match Client::new()
        .post("https://api.dropboxapi.com/2/users/get_current_account")
        .bearer_auth(&token)
        .json(&serde_json::json!({}))
        .send()
    {
        Ok(r) => r,
        Err(_) => {
            // Offline or transient network: keep showing the main UI if credentials exist.
            return Ok(true);
        }
    };
    if response.status().is_success() {
        return Ok(true);
    }
    if response.status().as_u16() == 401 {
        return Ok(false);
    }
    Ok(true)
}

fn current_tray_status_label(state: &AppState) -> String {
    if let Ok(engine) = state.sync_engine.lock() {
        if engine.is_sync_running() {
            return "Syncing".to_string();
        }
        if engine.current_status().last_error.is_some() {
            return "Error".to_string();
        }
    }
    "Idle".to_string()
}

fn update_tray_tooltip(app: &tauri::AppHandle, label: &str) {
    if let Some(tray) = app.tray_by_id("main") {
        let _ = tray.set_tooltip(Some(format!("DropboxSyncDesktop - {label}")));
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SelectiveSyncFilters {
    include_csv: String,
    exclude_csv: String,
}

#[tauri::command]
fn get_selective_sync_filters(state: tauri::State<AppState>) -> Result<SelectiveSyncFilters, String> {
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
fn set_selective_sync_filters(
    state: tauri::State<AppState>,
    include_csv: String,
    exclude_csv: String,
) -> Result<(), String> {
    state.db.set_include_prefixes_csv(&include_csv)?;
    state.db.set_exclude_prefixes_csv(&exclude_csv)?;
    Ok(())
}

#[tauri::command]
fn start_oauth_flow(state: tauri::State<AppState>) -> Result<OauthStartResponse, String> {
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
async fn complete_oauth_flow(
    app_state: tauri::State<'_, AppState>,
    code: String,
    state: String,
) -> Result<(), String> {
    complete_oauth_internal(app_state.inner(), code, state).await
}

async fn complete_oauth_internal(app_state: &AppState, code: String, state: String) -> Result<(), String> {
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

    let expires_at = token.expires_in.map(|in_s| (Utc::now() + Duration::seconds(in_s)).to_rfc3339());
    let session = storage::secure_store::TokenSession {
        access_token: token.access_token.clone(),
        refresh_token: token.refresh_token.clone(),
        expires_at,
    };

    app_state
        .secure_store
        .store_session(&session)
        .map_err(|e| format!("failed to store token session: {e}"))?;

    // Cache en memoria para evitar acceso repetido al Keychain (que puede pedir autorización).
    if let Ok(mut cache) = app_state.token_cache.lock() {
        *cache = Some(session);
    }

    Ok(())
}

#[tauri::command]
async fn complete_oauth_from_callback(app_state: tauri::State<'_, AppState>) -> Result<bool, String> {
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
fn poll_oauth_callback(
    state: tauri::State<AppState>,
) -> Result<Option<OauthCallbackPayload>, String> {
    let mut engine = state
        .sync_engine
        .lock()
        .map_err(|_| "sync engine lock poisoned".to_string())?;
    Ok(engine.consume_oauth_callback())
}

#[tauri::command]
fn set_sync_folder(state: tauri::State<AppState>, folder: String) -> Result<(), String> {
    fs::create_dir_all(&folder).map_err(|e| format!("failed to create sync folder: {e}"))?;
    let prev = state.db.get_sync_folder()?.unwrap_or_default();
    state.db.set_sync_folder(&folder)?;
    if prev != folder {
        // Limpia índice/cola para evitar jobs delete/updates basados en un folder anterior.
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
fn pick_sync_folder_dialog() -> Result<Option<String>, String> {
    let picked = rfd::FileDialog::new().pick_folder();
    Ok(picked.map(|p| p.to_string_lossy().to_string()))
}

#[tauri::command]
fn get_startup_requirements(state: tauri::State<AppState>) -> Result<StartupRequirementsResponse, String> {
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
fn start_background_scheduler(
    state: tauri::State<AppState>,
    app: tauri::AppHandle,
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
fn hide_main_window(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("main") {
        window.hide().map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn get_sync_status(state: tauri::State<AppState>) -> Result<SyncStatus, String> {
    refresh_queue_depth_internal(state.inner())?;
    let engine = state
        .sync_engine
        .lock()
        .map_err(|_| "sync engine lock poisoned".to_string())?;
    Ok(engine.current_status())
}

#[tauri::command]
fn get_sync_dashboard(state: tauri::State<AppState>) -> Result<SyncDashboard, String> {
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
fn scan_local_changes(state: tauri::State<AppState>) -> Result<usize, String> {
    scan_local_changes_internal(state.inner())
}

fn scan_local_changes_internal(state: &AppState) -> Result<usize, String> {
    // Fase 1: lectura corta — no mantener el lock durante walk/hash (evita congelar la UI).
    let folder = state
        .db
        .get_sync_folder()?
        .ok_or_else(|| "sync folder not configured".to_string())?;
    let known = state.db.list_local_files()?;
    let existing_jobs = state.db.list_recent_jobs(200)?;
    let pending_targets: HashSet<String> = existing_jobs
        .iter()
        .filter(|j| j.status == "queued" || j.status == "retry_wait" || j.status == "running")
        .filter_map(|j| j.target_path.clone())
        .collect();

    let tracked_root = PathBuf::from(&folder);
    let known_map: HashMap<String, FileIndexRow> = known
        .iter()
        .map(|f| (f.relative_path.clone(), f.clone()))
        .collect();

    let mut pending_targets = pending_targets;
    let mut seen_paths: HashSet<String> = HashSet::new();
    let mut enqueued_jobs = 0usize;

    // Fase 2: walk + hash sin lock de DB (I/O y CPU pesados fuera del mutex).
    for entry in WalkDir::new(&tracked_root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
    {
        let absolute = entry.path().to_path_buf();
        let relative = absolute
            .strip_prefix(&tracked_root)
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .to_string();

        // Placeholders `.cloudsc` are metadata files and must not be uploaded/deleted.
        if relative.ends_with(".cloudsc") {
            continue;
        }
        if should_ignore_local_path(&relative) {
            continue;
        }

        seen_paths.insert(relative.clone());

        let (hash, size_bytes, modified_ts) = hash_file(&absolute)?;

        match known_map.get(&relative) {
            None => {
                state.db.enqueue_job("upload", Some(&relative), Some(&relative))?;
                state
                    .db
                    .upsert_local_file(&relative, &hash, size_bytes, modified_ts)?;
                pending_targets.insert(relative.clone());
                enqueued_jobs += 1;
            }
            Some(prev) if prev.hash != hash => {
                if pending_targets.contains(&relative) {
                    let conflicted_path = create_conflicted_copy(&absolute)?;
                    let conflicted_rel = conflicted_path
                        .strip_prefix(&tracked_root)
                        .map_err(|e| e.to_string())?
                        .to_string_lossy()
                        .to_string();
                    {
                        state.db.add_conflict(
                            &relative,
                            &relative,
                            "concurrent local update while job pending",
                        )?;
                        state.db.enqueue_job(
                            "upload",
                            Some(&conflicted_rel),
                            Some(&conflicted_rel),
                        )?;
                        state
                            .db
                            .upsert_local_file(&relative, &hash, size_bytes, modified_ts)?;
                    }
                    if let Ok(mut engine) = state.sync_engine.lock() {
                        engine.record_conflict();
                    }
                    enqueued_jobs += 1;
                } else {
                    state.db.enqueue_job("upload", Some(&relative), Some(&relative))?;
                    state
                        .db
                        .upsert_local_file(&relative, &hash, size_bytes, modified_ts)?;
                    pending_targets.insert(relative.clone());
                    enqueued_jobs += 1;
                }
            }
            _ => {
                // Sin cambios de contenido: no escribir en SQLite (menos contención con la UI).
            }
        }
    }

    for prev in known {
        if prev.relative_path.ends_with(".cloudsc") {
            // `.cloudsc` son placeholders de metadata: no deben disparar borrados/subidas.
            continue;
        }
        if should_ignore_local_path(&prev.relative_path) {
            continue;
        }
        if !seen_paths.contains(&prev.relative_path) {
            state.db.enqueue_job(
                "delete",
                Some(&prev.relative_path),
                Some(&prev.relative_path),
            )?;
            state.db.remove_local_file(&prev.relative_path)?;
            enqueued_jobs += 1;
        }
    }

    let remote_enqueued = refresh_remote_index_and_enqueue_downloads_internal(state)?;

    {
        let mut engine = state
            .sync_engine
            .lock()
            .map_err(|_| "sync engine lock poisoned".to_string())?;
        engine.set_last_scan_at(Utc::now().to_rfc3339());
    }

    refresh_queue_depth_internal(state)?;
    Ok(enqueued_jobs + remote_enqueued)
}

#[tauri::command]
fn process_sync_queue(state: tauri::State<AppState>) -> Result<bool, String> {
    process_sync_queue_internal(state.inner())
}

fn process_sync_queue_internal(state: &AppState) -> Result<bool, String> {
    let next = state.db.pick_next_due_job()?;
    let Some(job) = next else {
        refresh_queue_depth_internal(state)?;
        return Ok(false);
    };

    let max_attempts = 5;
    let attempt = job.attempt_count + 1;

    let op_result: Result<(), String> = match job.job_type.as_str() {
        "upload" => job
            .source_path
            .as_deref()
            .ok_or_else(|| "upload job missing source_path".to_string())
            .and_then(|rel| upload_local_file_internal(state, rel)),
        "delete" => job
            .target_path
            .as_deref()
            .or(job.source_path.as_deref())
            .ok_or_else(|| "delete job missing target_path/source_path".to_string())
            .and_then(|rel| delete_remote_file_internal(state, rel)),
        "download" => job
            .target_path
            .as_deref()
            .or(job.source_path.as_deref())
            .ok_or_else(|| "download job missing target_path/source_path".to_string())
            .and_then(|rel| download_remote_file_internal(state, &normalize_dropbox_path(rel))),
        other => Err(format!("unknown job_type: {other}")),
    };

    match op_result {
        Ok(()) => {
            state.db.mark_job_completed(job.id)?;
            if let Ok(mut engine) = state.sync_engine.lock() {
                engine.record_job_processed();
                engine.clear_last_error();
            }
        }
        Err(err) => {
            if attempt >= max_attempts {
                state.db.mark_job_failed(job.id, attempt)?;
                if let Ok(mut engine) = state.sync_engine.lock() {
                    engine.set_last_error(format!("job {} failed: {err}", job.id));
                }
            } else {
                let wait_secs = backoff_seconds(attempt);
                let retry_at = (Utc::now() + Duration::seconds(wait_secs)).to_rfc3339();
                state.db.mark_job_retry_wait(job.id, attempt, &retry_at)?;
                if let Ok(mut engine) = state.sync_engine.lock() {
                    engine.set_last_error(format!(
                        "job {} retry scheduled in {}s (attempt {}): {err}",
                        job.id, wait_secs, attempt
                    ));
                }
            }
        }
    }

    refresh_queue_depth_internal(state)?;
    Ok(true)
}

#[tauri::command]
fn sync_tick(state: tauri::State<AppState>) -> Result<SyncTickResult, String> {
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
fn trigger_sync_tick(state: tauri::State<AppState>) -> Result<TriggerSyncResponse, String> {
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
fn list_remote_folder(state: tauri::State<AppState>, path: String) -> Result<ListRemoteFolderResponse, String> {
    let token = get_access_token(&state.inner())?;

    let folder = state
        .db
        .get_sync_folder()?
        .ok_or_else(|| "sync folder not configured".to_string())?;

    let include_prefixes = parse_prefix_csv(state.db.get_include_prefixes_csv()?);
    let exclude_prefixes = parse_prefix_csv(state.db.get_exclude_prefixes_csv()?);

    let dropbox_path = normalize_dropbox_path(&path);

    let client = Client::new();
    let response = client
        .post("https://api.dropboxapi.com/2/files/list_folder")
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "path": dropbox_path,
            "recursive": false,
            "include_deleted": false
        }))
        .send()
        .map_err(|e| format!("list_folder request failed: {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_else(|_| "<unreadable body>".to_string());
        return Err(format!("list_folder status error: {status}; body: {body}"));
    }

    let entries_resp: DropboxListFolderResponse = response
        .json()
        .map_err(|e| format!("list_folder parse failed: {e}"))?;

    let mut entries = Vec::new();
    for entry in entries_resp.entries {
        let Some(path_display) = entry.path_display else {
            continue;
        };
        let relative = path_display.trim_start_matches('/').to_string();
        let local_target = PathBuf::from(&folder).join(&relative);
        let is_synced = local_target.exists();
        let is_excluded = !is_path_allowed(&relative, &include_prefixes, &exclude_prefixes);

        entries.push(RemoteEntry {
            tag: entry.tag,
            path_display,
            size: entry.size,
            is_synced,
            is_excluded,
        });
    }

    Ok(ListRemoteFolderResponse {
        current_path: dropbox_path,
        entries,
    })
}

fn write_cloudsc_placeholder_file(placeholder_path: &Path, meta: &CloudscMeta) -> Result<(), String> {
    if let Some(parent) = placeholder_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed creating placeholder parent dir: {e}"))?;
    }
    let payload = encode_cloudsc_meta(meta)?;
    fs::write(placeholder_path, payload)
        .map_err(|e| format!("failed writing placeholder file: {e}"))?;
    Ok(())
}

fn read_cloudsc_placeholder_file(placeholder_path: &Path) -> Result<CloudscMeta, String> {
    let bytes = fs::read(placeholder_path)
        .map_err(|e| format!("failed reading placeholder file bytes: {e}"))?;
    decode_cloudsc_meta(&bytes)
}

fn index_remote_folder_children_as_cloudsc_placeholders_internal(
    state: &AppState,
    remote_folder_path_display: &str,
    local_dir: &Path,
) -> Result<usize, String> {
    let token = get_access_token(state)?;
    let include_prefixes = parse_prefix_csv(state.db.get_include_prefixes_csv()?);
    let exclude_prefixes = parse_prefix_csv(state.db.get_exclude_prefixes_csv()?);

    fs::create_dir_all(local_dir).map_err(|e| format!("failed creating local dir: {e}"))?;

    let client = Client::new();
    let response = client
        .post("https://api.dropboxapi.com/2/files/list_folder")
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "path": remote_folder_path_display,
            "recursive": false,
            "include_deleted": false
        }))
        .send()
        .map_err(|e| format!("list_folder request failed: {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_else(|_| "<unreadable body>".to_string());
        return Err(format!(
            "list_folder status error for {remote_folder_path_display}: {status}; body: {body}"
        ));
    }

    let entries_resp: DropboxListFolderResponse = response
        .json()
        .map_err(|e| format!("list_folder parse failed: {e}"))?;

    let mut created = 0usize;
    for entry in entries_resp.entries {
        let tag = entry.tag;
        let path_display = match entry.path_display {
            Some(p) => p,
            None => continue,
        };

        let relative = path_display.trim_start_matches('/').to_string();
        if !is_path_allowed(&relative, &include_prefixes, &exclude_prefixes) {
            continue;
        }

        let child_name = path_display
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(&path_display);
        let placeholder_path = local_dir.join(format!("{child_name}.cloudsc"));
        let target_path = cloudsc_target_path(&placeholder_path);
        // Si el placeholder ya existe, no lo recreamos.
        if placeholder_path.exists() {
            continue;
        }
        // Si el item ya está hidratado (archivo o carpeta real), no debe volver como placeholder.
        if target_path.exists() {
            continue;
        }

        let meta = CloudscMeta {
            version: 1,
            tag,
            remote_path_display: path_display,
        };
        write_cloudsc_placeholder_file(&placeholder_path, &meta)?;
        created += 1;
    }

    Ok(created)
}

fn index_remote_root_children_as_cloudsc_placeholders_internal(state: &AppState) -> Result<usize, String> {
    let token = get_access_token(state)?;
    let sync_folder_str = state
        .db
        .get_sync_folder()?
        .ok_or_else(|| "sync folder not configured".to_string())?;
    let sync_folder = PathBuf::from(sync_folder_str);
    fs::create_dir_all(&sync_folder).map_err(|e| format!("failed creating sync folder: {e}"))?;

    let include_prefixes = parse_prefix_csv(state.db.get_include_prefixes_csv()?);
    let exclude_prefixes = parse_prefix_csv(state.db.get_exclude_prefixes_csv()?);

    let client = Client::new();
    let response = client
        .post("https://api.dropboxapi.com/2/files/list_folder")
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "path": "",
            "recursive": false,
            "include_deleted": false
        }))
        .send()
        .map_err(|e| format!("list_folder request failed: {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_else(|_| "<unreadable body>".to_string());
        return Err(format!("list_folder status error for root: {status}; body: {body}"));
    }

    let entries_resp: DropboxListFolderResponse = response
        .json()
        .map_err(|e| format!("list_folder parse failed: {e}"))?;

    let mut created = 0usize;
    for entry in entries_resp.entries {
        let tag = entry.tag;
        let path_display = match entry.path_display {
            Some(p) => p,
            None => continue,
        };

        let relative = path_display.trim_start_matches('/').to_string();
        if !is_path_allowed(&relative, &include_prefixes, &exclude_prefixes) {
            continue;
        }

        let child_name = path_display
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(&path_display);
        let placeholder_path = sync_folder.join(format!("{child_name}.cloudsc"));
        let target_path = cloudsc_target_path(&placeholder_path);
        if placeholder_path.exists() {
            continue;
        }
        // Si el item ya está hidratado (archivo o carpeta real), no debe volver como placeholder.
        if target_path.exists() {
            continue;
        }

        let meta = CloudscMeta {
            version: 1,
            tag,
            remote_path_display: path_display,
        };
        write_cloudsc_placeholder_file(&placeholder_path, &meta)?;
        created += 1;
    }

    Ok(created)
}

#[tauri::command]
fn index_remote_root_placeholders(state: tauri::State<AppState>) -> Result<usize, String> {
    // Heavy network + filesystem work: run synchronously for now; caller can trigger via UI thread.
    index_remote_root_children_as_cloudsc_placeholders_internal(&state.inner())
}

#[tauri::command]
fn list_cloudsc_placeholders(
    state: tauri::State<AppState>,
    limit: usize,
) -> Result<Vec<CloudscPlaceholderInfo>, String> {
    let sync_folder_str = state
        .db
        .get_sync_folder()?
        .ok_or_else(|| "sync folder not configured".to_string())?;
    let sync_folder = PathBuf::from(sync_folder_str);

    let mut out: Vec<CloudscPlaceholderInfo> = Vec::new();
    for entry in WalkDir::new(&sync_folder).min_depth(1) {
        let entry = entry.map_err(|e| format!("walk dir error: {e}"))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let file_name = entry.file_name().to_string_lossy();
        if !file_name.ends_with(".cloudsc") {
            continue;
        }
        if out.len() >= limit {
            break;
        }
        let placeholder_path = entry.path();
        let meta = read_cloudsc_placeholder_file(placeholder_path)?;
        let local_rel = relpath_under(&sync_folder, placeholder_path)?;
        out.push(CloudscPlaceholderInfo {
            local_path_display: local_rel,
            tag: meta.tag,
            remote_path_display: meta.remote_path_display,
        });
    }
    Ok(out)
}

fn hydrate_cloudsc_placeholder_internal(
    state: &AppState,
    placeholder_local_rel_path: &str,
) -> Result<usize, String> {
    let sync_folder = state
        .db
        .get_sync_folder()?
        .ok_or_else(|| "sync folder not configured".to_string())?;

    let placeholder_path = PathBuf::from(&sync_folder).join(placeholder_local_rel_path);
    if !placeholder_path.exists() {
        return Err(format!("placeholder not found: {placeholder_local_rel_path}"));
    }

    let meta = read_cloudsc_placeholder_file(&placeholder_path)?;
    let target_path = cloudsc_target_path(&placeholder_path);

    if meta.tag == "file" {
        // Download and write actual content under sync folder.
        download_remote_file_internal(state, &meta.remote_path_display)?;
        // Remove placeholder regardless; if selective sync excluded it, download is a no-op.
        fs::remove_file(&placeholder_path).map_err(|e| format!("failed removing placeholder: {e}"))?;
        Ok(1)
    } else {
        // Folder placeholder: create directory and populate immediate children as placeholders.
        fs::create_dir_all(&target_path)
            .map_err(|e| format!("failed creating hydrated folder: {e}"))?;
        fs::remove_file(&placeholder_path)
            .map_err(|e| format!("failed removing folder placeholder: {e}"))?;

        // Populate immediate children placeholders (no deep traversal).
        index_remote_folder_children_as_cloudsc_placeholders_internal(
            state,
            &meta.remote_path_display,
            &target_path,
        )
    }
}

#[tauri::command]
fn trigger_hydrate_cloudsc_placeholder(
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

fn upload_local_file_internal(state: &AppState, relative: &str) -> Result<(), String> {
    if relative.ends_with(".cloudsc") {
        return Ok(());
    }

    let token = get_access_token(state)?;
    let folder = state
        .db
        .get_sync_folder()?
        .ok_or_else(|| "sync folder not configured".to_string())?;

    let local_path = PathBuf::from(&folder).join(relative);
    if !local_path.exists() {
        return Err(format!("local file missing for upload: {relative}"));
    }

    let bytes = fs::read(&local_path)
        .map_err(|e| format!("failed reading local file bytes for upload: {e}"))?;

    let dropbox_path = normalize_dropbox_path(relative);
    let client = Client::new();
    let resp = client
        .post("https://content.dropboxapi.com/2/files/upload")
        .bearer_auth(&token)
        .header(
            "Dropbox-API-Arg",
            serde_json::json!({
                "path": dropbox_path,
                "mode": { ".tag": "overwrite" }
            })
            .to_string(),
        )
        .body(bytes)
        .send()
        .map_err(|e| format!("upload request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_else(|_| "<unreadable body>".to_string());
        return Err(format!(
            "upload status error for {dropbox_path}: {status}; body: {body}"
        ));
    }

    Ok(())
}

fn delete_remote_file_internal(state: &AppState, relative: &str) -> Result<(), String> {
    if relative.ends_with(".cloudsc") {
        return Ok(());
    }

    let token = get_access_token(state)?;
    let dropbox_path = normalize_dropbox_path(relative);
    let client = Client::new();
    let resp = client
        .post("https://api.dropboxapi.com/2/files/delete_v2")
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "path": dropbox_path
        }))
        .send()
        .map_err(|e| format!("delete request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_else(|_| "<unreadable body>".to_string());
        return Err(format!(
            "delete status error for {dropbox_path}: {status}; body: {body}"
        ));
    }

    Ok(())
}

fn download_remote_file_internal(state: &AppState, path_display: &str) -> Result<(), String> {
    let token = get_access_token(state)?;
    let folder = state
        .db
        .get_sync_folder()?
        .ok_or_else(|| "sync folder not configured".to_string())?;

    let relative = path_display.trim_start_matches('/').to_string();
    let include_prefixes = parse_prefix_csv(state.db.get_include_prefixes_csv()?);
    let exclude_prefixes = parse_prefix_csv(state.db.get_exclude_prefixes_csv()?);
    if !is_path_allowed(&relative, &include_prefixes, &exclude_prefixes) {
        return Ok(());
    }

    let client = Client::new();
    let download_resp = client
        .post("https://content.dropboxapi.com/2/files/download")
        .bearer_auth(&token)
        .header(
            "Dropbox-API-Arg",
            serde_json::json!({ "path": path_display }).to_string(),
        )
        .send()
        .map_err(|e| format!("download request failed: {e}"))?;

    if !download_resp.status().is_success() {
        let status = download_resp.status();
        let body = download_resp.text().unwrap_or_else(|_| "<unreadable body>".to_string());
        return Err(format!(
            "download status error for {path_display}: {status}; body: {body}"
        ));
    }

    let bytes = download_resp
        .bytes()
        .map_err(|e| format!("download bytes failed: {e}"))?;

    let target = PathBuf::from(&folder).join(&relative);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create parent directory: {e}"))?;
    }

    fs::write(&target, &bytes).map_err(|e| format!("failed writing local file: {e}"))?;
    let (hash, size_bytes, modified_ts) = hash_file(&target)?;
    state
        .db
        .upsert_local_file(&relative, &hash, size_bytes, modified_ts)?;
    Ok(())
}

#[tauri::command]
fn trigger_download_remote_file(state: tauri::State<AppState>, path_display: String) -> Result<TriggerActionResponse, String> {
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

fn hydrate_remote_folder_internal(state: &AppState, folder_path_display: &str) -> Result<usize, String> {
    let token = get_access_token(state)?;
    let folder = state
        .db
        .get_sync_folder()?
        .ok_or_else(|| "sync folder not configured".to_string())?;

    let include_prefixes = parse_prefix_csv(state.db.get_include_prefixes_csv()?);
    let exclude_prefixes = parse_prefix_csv(state.db.get_exclude_prefixes_csv()?);

    fs::create_dir_all(&folder).map_err(|e| format!("failed to create sync folder: {e}"))?;

    let client = Client::new();
    let mut downloaded = 0usize;

    let response = client
        .post("https://api.dropboxapi.com/2/files/list_folder")
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "path": folder_path_display,
            "recursive": true,
            "include_deleted": false
        }))
        .send()
        .map_err(|e| format!("list_folder request failed: {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_else(|_| "<unreadable body>".to_string());
        return Err(format!(
            "list_folder status error for {folder_path_display}: {status}; body: {body}"
        ));
    }

    let mut entries_resp: DropboxListFolderResponse = response
        .json()
        .map_err(|e| format!("list_folder parse failed: {e}"))?;

    loop {
        for entry in &entries_resp.entries {
            if entry.tag != "file" {
                continue;
            }
            let Some(path_display) = &entry.path_display else {
                continue;
            };
            let relative = path_display.trim_start_matches('/').to_string();
            if !is_path_allowed(&relative, &include_prefixes, &exclude_prefixes) {
                continue;
            }

            let target = PathBuf::from(&folder).join(&relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("failed to create parent directory: {e}"))?;
            }

            let download_resp = client
                .post("https://content.dropboxapi.com/2/files/download")
                .bearer_auth(&token)
                .header(
                    "Dropbox-API-Arg",
                    serde_json::json!({ "path": path_display }).to_string(),
                )
                .send()
                .map_err(|e| format!("download request failed: {e}"))?;

            if !download_resp.status().is_success() {
                let status = download_resp.status();
                let body = download_resp.text().unwrap_or_else(|_| "<unreadable body>".to_string());
                return Err(format!(
                    "download status error for {path_display}: {status}; body: {body}"
                ));
            }

            let bytes = download_resp
                .bytes()
                .map_err(|e| format!("download bytes failed: {e}"))?;
            fs::write(&target, &bytes).map_err(|e| format!("failed writing local file: {e}"))?;
            let (hash, size_bytes, modified_ts) = hash_file(&target)?;
            state.db.upsert_local_file(&relative, &hash, size_bytes, modified_ts)?;
            downloaded += 1;
        }

        if !entries_resp.has_more {
            break;
        }

        entries_resp = client
            .post("https://api.dropboxapi.com/2/files/list_folder/continue")
            .bearer_auth(&token)
            .json(&serde_json::json!({ "cursor": entries_resp.cursor.clone() }))
            .send()
            .map_err(|e| format!("list_folder/continue request failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("list_folder/continue status error: {e}"))?
            .json()
            .map_err(|e| format!("list_folder/continue parse failed: {e}"))?;
    }

    Ok(downloaded)
}

#[tauri::command]
fn trigger_hydrate_remote_folder(state: tauri::State<AppState>, folder_path_display: String) -> Result<TriggerActionResponse, String> {
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let db = Db::new().expect("failed to initialize sqlite db");
    let mut sync_engine = SyncEngine::new();
    if let Ok(Some(folder)) = db.get_sync_folder() {
        sync_engine.set_tracked_path(folder);
    }
    let app_state = AppState {
        secure_store: SecureStore::new(),
        db: Arc::new(db),
        sync_engine: Arc::new(Mutex::new(sync_engine)),
        token_cache: Arc::new(Mutex::new(None)),
        scheduler_started: Arc::new(Mutex::new(false)),
    };

    tauri::Builder::default()
        .manage(app_state)
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let open_dashboard = MenuItem::with_id(app, "open_dashboard", "Open Dashboard", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Exit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open_dashboard, &quit])?;
            // Must match `tray_by_id("main")` below. Include a real PNG: title-only / no-icon
            // trays are easy to miss on macOS (layout + template rendering).
            let tray_image = Image::from_bytes(include_bytes!("../icons/32x32.png"))
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
            let tray_builder = TrayIconBuilder::with_id("main")
                .icon(tray_image)
                .icon_as_template(true)
                .menu(&menu)
                .tooltip("DropboxSyncDesktop - Idle")
                .on_menu_event(move |app, event| {
                    if event.id.as_ref() == "open_dashboard" {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.unminimize();
                            let _ = window.set_focus();
                        }
                    } else if event.id.as_ref() == "quit" {
                        app.exit(0);
                    }
                });

            let tray_ok = match tray_builder.build(app) {
                Ok(_) => true,
                Err(err) => {
                    eprintln!("failed to create tray icon: {err}");
                    false
                }
            };

            // Menubar-only behavior only when tray is actually available.
            if tray_ok {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.hide();
                }
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            start_oauth_flow,
            complete_oauth_flow,
            complete_oauth_from_callback,
            poll_oauth_callback,
            get_startup_requirements,
            pick_sync_folder_dialog,
            start_background_scheduler,
            hide_main_window,
            set_sync_folder,
            get_sync_status,
            get_sync_dashboard,
            scan_local_changes,
            process_sync_queue,
            sync_tick,
            trigger_sync_tick,
            list_remote_folder,
            trigger_download_remote_file,
            trigger_hydrate_remote_folder,
            index_remote_root_placeholders,
            list_cloudsc_placeholders,
            trigger_hydrate_cloudsc_placeholder,
            get_selective_sync_filters,
            set_selective_sync_filters
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn start_oauth_callback_listener(sync_engine: Arc<Mutex<SyncEngine>>) -> Result<(), String> {
    let redirect_uri = dropbox_redirect_uri();
    let url = Url::parse(&redirect_uri).map_err(|e| format!("invalid redirect uri: {e}"))?;
    let host = url
        .host_str()
        .ok_or_else(|| "redirect uri must contain host".to_string())?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "redirect uri must contain port".to_string())?;
    let callback_path = url.path().to_string();
    let bind_addr = format!("{host}:{port}");

    std::thread::spawn(move || {
        let listener = match TcpListener::bind(&bind_addr) {
            Ok(v) => v,
            Err(_) => return,
        };

        // Process multiple incoming connections so we don't miss callback due to
        // preflight/browser extra requests (favicon, speculative load, etc).
        for incoming in listener.incoming().take(8) {
            let Ok(mut stream) = incoming else {
                continue;
            };

            let mut reader = BufReader::new(&stream);
            let mut first_line = String::new();
            if reader.read_line(&mut first_line).is_err() {
                continue;
            }

            let request_path = first_line
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_string();

            let mut callback_captured = false;
            if request_path.starts_with(&callback_path) {
                if let Some(query) = request_path.split('?').nth(1) {
                    let mut code = None;
                    let mut state = None;
                    for pair in query.split('&') {
                        let mut parts = pair.splitn(2, '=');
                        let key = parts.next().unwrap_or_default();
                        let value = parts.next().unwrap_or_default();
                        let decoded = urlencoding::decode(value)
                            .map(|v| v.to_string())
                            .unwrap_or_default();
                        if key == "code" {
                            code = Some(decoded);
                        } else if key == "state" {
                            state = Some(decoded);
                        }
                    }
                    if let (Some(code), Some(state_value)) = (code, state) {
                        if let Ok(mut engine) = sync_engine.lock() {
                            engine.set_oauth_callback(code, state_value);
                            callback_captured = true;
                        }
                    }
                }
            }

            let body = if callback_captured {
                "<html><body><h3>Dropbox login completed</h3><p>You can return to the app.</p></body></html>"
            } else {
                "<html><body><h3>Waiting for Dropbox callback...</h3></body></html>"
            };

            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n{body}"
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();

            if callback_captured {
                break;
            }
        }
    });

    Ok(())
}

fn refresh_queue_depth_internal(state: &AppState) -> Result<(), String> {
    let queue_depth = state.db.count_active_jobs()?;

    let mut engine = state
        .sync_engine
        .lock()
        .map_err(|_| "sync engine lock poisoned".to_string())?;
    engine.set_queue_depth(queue_depth);
    Ok(())
}

#[derive(Clone)]
struct RemoteFileMeta {
    content_hash: String,
    rev: String,
    modified_ts: i64,
}

fn parse_rfc3339_ts_to_unix(input: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(input)
        .map(|v| v.with_timezone(&Utc).timestamp())
        .unwrap_or(0)
}

fn fetch_remote_file_metadata(state: &AppState, relative: &str) -> Result<Option<RemoteFileMeta>, String> {
    let token = get_access_token(state)?;
    let client = Client::new();
    let dropbox_path = normalize_dropbox_path(relative);

    let response = client
        .post("https://api.dropboxapi.com/2/files/get_metadata")
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "path": dropbox_path,
            "include_media_info": false,
            "include_deleted": false
        }))
        .send()
        .map_err(|e| format!("get_metadata request failed for {relative}: {e}"))?;

    if response.status().is_success() {
        let entry: DropboxEntry = response
            .json()
            .map_err(|e| format!("get_metadata parse failed for {relative}: {e}"))?;
        if entry.tag != "file" {
            return Ok(None);
        }
        let content_hash = entry.content_hash.unwrap_or_default();
        let rev = entry.rev.unwrap_or_default();
        let modified_ts = entry
            .server_modified
            .as_deref()
            .map(parse_rfc3339_ts_to_unix)
            .unwrap_or(0);
        if content_hash.is_empty() || rev.is_empty() {
            return Ok(None);
        }
        return Ok(Some(RemoteFileMeta {
            content_hash,
            rev,
            modified_ts,
        }));
    }

    // If file no longer exists remotely, caller can map this to delete logic later.
    let status = response.status();
    let body = response.text().unwrap_or_else(|_| "<unreadable body>".to_string());
    if status.as_u16() == 409 && (body.contains("not_found") || body.contains("path")) {
        return Ok(None);
    }
    Err(format!(
        "get_metadata status error for {relative}: {status}; body: {body}"
    ))
}

fn refresh_remote_index_and_enqueue_downloads_internal(state: &AppState) -> Result<usize, String> {
    let local_files = state.db.list_local_files()?;
    if local_files.is_empty() {
        return Ok(0);
    }

    let existing_jobs = state.db.list_recent_jobs(400)?;
    let pending_targets: HashSet<String> = existing_jobs
        .iter()
        .filter(|j| j.status == "queued" || j.status == "retry_wait" || j.status == "running")
        .filter_map(|j| j.target_path.clone().or(j.source_path.clone()))
        .collect();

    let mut enqueued = 0usize;
    for local in local_files {
        let rel = local.relative_path;
        if rel.ends_with(".cloudsc") {
            continue;
        }
        if pending_targets.contains(&rel) {
            continue;
        }

        let prev_remote = state.db.get_remote_file(&rel)?;
        let remote_meta = fetch_remote_file_metadata(state, &rel)?;
        let Some(remote_meta) = remote_meta else {
            // Missing in remote: keep current behavior (local changes win), do not auto-delete local.
            continue;
        };

        let should_download = match prev_remote {
            None => false,
            Some(prev) => prev.content_hash != remote_meta.content_hash,
        };

        state
            .db
            .upsert_remote_file(&rel, &remote_meta.content_hash, &remote_meta.rev, remote_meta.modified_ts)?;

        // If remote changed and diverges from local hash, enqueue download.
        if should_download && local.hash != remote_meta.content_hash {
            state.db.enqueue_job("download", Some(&rel), Some(&rel))?;
            enqueued += 1;
        }
    }

    Ok(enqueued)
}

fn hash_file(path: &Path) -> Result<(String, i64, i64), String> {
    let mut file = File::open(path).map_err(|e| format!("cannot open file for hash: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    let metadata = fs::metadata(path).map_err(|e| e.to_string())?;
    let size = metadata.len() as i64;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|ts| ts.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    Ok((format!("{:x}", hasher.finalize()), size, modified))
}

fn create_conflicted_copy(path: &Path) -> Result<PathBuf, String> {
    let parent = path
        .parent()
        .ok_or_else(|| "file has no parent for conflict copy".to_string())?;
    let stem = path
        .file_stem()
        .ok_or_else(|| "missing file stem".to_string())?
        .to_string_lossy();
    let ext = path.extension().map(|e| e.to_string_lossy().to_string());
    let ts = Utc::now().format("%Y%m%d%H%M%S");

    let name = match ext {
        Some(ext) => format!("{} (conflicted copy {}).{}", stem, ts, ext),
        None => format!("{} (conflicted copy {})", stem, ts),
    };

    let dest = parent.join(name);
    fs::copy(path, &dest).map_err(|e| format!("failed to create conflicted copy: {e}"))?;
    Ok(dest)
}

fn backoff_seconds(attempt: i64) -> i64 {
    let safe_attempt = attempt.clamp(1, 10) as u32;
    2_i64.pow(safe_attempt)
}

fn run_sync_tick_internal(state: &AppState) -> Result<SyncTickResult, String> {
    let enqueued_jobs = scan_local_changes_internal(state)?;
    let processed_job = process_sync_queue_internal(state)?;
    let scanned_files = state.db.list_local_files()?.len();
    Ok(SyncTickResult {
        scanned_files,
        enqueued_jobs,
        processed_job,
    })
}

#[allow(dead_code)]
fn pull_remote_snapshot_internal(state: &AppState) -> Result<usize, String> {
    let token = match get_access_token(state) {
        Ok(token) => token,
        Err(_) => return Ok(0),
    };
    let folder = state
        .db
        .get_sync_folder()?
        .ok_or_else(|| "sync folder not configured".to_string())?;
    fs::create_dir_all(&folder).map_err(|e| format!("failed to create sync folder: {e}"))?;

    let client = Client::new();
    let mut downloaded = 0usize;
    let known_map: HashMap<String, FileIndexRow> = state
        .db
        .list_local_files()?
        .into_iter()
        .map(|f| (f.relative_path.clone(), f))
        .collect();

    let list_folder_resp = client
        .post("https://api.dropboxapi.com/2/files/list_folder")
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "path": "",
            "recursive": true,
            "include_deleted": false
        }))
        .send()
        .map_err(|e| format!("list_folder request failed: {e}"))?;
    if !list_folder_resp.status().is_success() {
        let status = list_folder_resp.status();
        let body = list_folder_resp.text().unwrap_or_else(|_| "<unreadable body>".to_string());
        return Err(format!("list_folder status error: {status}; body: {body}"));
    }
    let mut response: DropboxListFolderResponse = list_folder_resp
        .json()
        .map_err(|e| format!("list_folder parse failed: {e}"))?;

    loop {
        for entry in &response.entries {
            if entry.tag != "file" {
                continue;
            }
            let Some(path_display) = &entry.path_display else {
                continue;
            };

            let relative = path_display.trim_start_matches('/').to_string();
            let target = PathBuf::from(&folder).join(&relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("failed to create parent directory: {e}"))?;
            }

            if known_map.contains_key(&relative) && target.exists() {
                continue;
            }

            let download_resp = client
                .post("https://content.dropboxapi.com/2/files/download")
                .bearer_auth(&token)
                .header(
                    "Dropbox-API-Arg",
                    serde_json::json!({ "path": path_display }).to_string(),
                )
                .send()
                .map_err(|e| format!("download request failed: {e}"))?;
            if !download_resp.status().is_success() {
                let status = download_resp.status();
                let body = download_resp.text().unwrap_or_else(|_| "<unreadable body>".to_string());
                return Err(format!(
                    "download status error for {path_display}: {status}; body: {body}"
                ));
            }
            let bytes = download_resp
                .bytes()
                .map_err(|e| format!("download bytes failed: {e}"))?;

            fs::write(&target, &bytes).map_err(|e| format!("failed writing local file: {e}"))?;
            let (hash, size_bytes, modified_ts) = hash_file(&target)?;
            state
                .db
                .upsert_local_file(&relative, &hash, size_bytes, modified_ts)?;
            downloaded += 1;
        }

        if !response.has_more {
            break;
        }

        let continue_resp = client
            .post("https://api.dropboxapi.com/2/files/list_folder/continue")
            .bearer_auth(&token)
            .json(&serde_json::json!({ "cursor": response.cursor.clone() }))
            .send()
            .map_err(|e| format!("list_folder/continue request failed: {e}"))?;
        if !continue_resp.status().is_success() {
            let status = continue_resp.status();
            let body = continue_resp.text().unwrap_or_else(|_| "<unreadable body>".to_string());
            return Err(format!(
                "list_folder/continue status error: {status}; body: {body}"
            ));
        }
        response = continue_resp
            .json()
            .map_err(|e| format!("list_folder/continue parse failed: {e}"))?;
    }

    Ok(downloaded)
}

#[cfg(test)]
mod tests {
    use super::backoff_seconds;

    #[test]
    fn backoff_grows_exponentially() {
        assert_eq!(backoff_seconds(1), 2);
        assert_eq!(backoff_seconds(2), 4);
        assert_eq!(backoff_seconds(3), 8);
    }
}
