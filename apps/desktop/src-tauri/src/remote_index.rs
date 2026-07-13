use std::collections::{HashMap, HashSet};

use chrono::Utc;

use crate::auth_session::get_access_token;
use crate::error::{AppError, AppResult};
use crate::models::{DropboxEntry, DropboxListFolderResponse};
use crate::path_util::normalize_dropbox_path;
use crate::state::AppState;

#[derive(Clone)]
pub(crate) struct RemoteFileMeta {
    pub content_hash: String,
    pub rev: String,
    pub modified_ts: i64,
}

/// `app_config` key holding the persisted `list_folder` cursor for cursor-delta
/// remote change detection (DBSYNC-30). Cleared by `reset_sync_state` (folder
/// change) and on (re)login so the loop reseeds against the new state.
pub(crate) const REMOTE_DELTA_CURSOR_KEY: &str = "remote_delta_cursor";

/// What a single `list_folder`/`continue` delta entry means for the local index.
#[derive(Debug, PartialEq)]
pub(crate) enum DeltaAction {
    /// A file was added or modified remotely.
    Upsert(String, RemoteFileMeta),
    /// A file/folder was removed remotely.
    Remove(String),
    /// A folder entry or an unusable entry — nothing to apply.
    Ignore,
}

impl PartialEq for RemoteFileMeta {
    fn eq(&self, other: &Self) -> bool {
        self.content_hash == other.content_hash
            && self.rev == other.rev
            && self.modified_ts == other.modified_ts
    }
}

impl std::fmt::Debug for RemoteFileMeta {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteFileMeta")
            .field("content_hash", &self.content_hash)
            .field("rev", &self.rev)
            .field("modified_ts", &self.modified_ts)
            .finish()
    }
}

/// Classify a `list_folder`/`continue` delta entry. A `deleted` entry carries no
/// hash/rev; a `file` entry needs both. Pure — unit-testable without network.
pub(crate) fn delta_action_from_entry(entry: &DropboxEntry) -> DeltaAction {
    let Some(path_display) = entry.path_display.as_deref() else {
        return DeltaAction::Ignore;
    };
    let rel = path_display.trim_start_matches('/').to_string();
    match entry.tag.as_str() {
        "file" => {
            let content_hash = entry.content_hash.clone().unwrap_or_default();
            let rev = entry.rev.clone().unwrap_or_default();
            if content_hash.is_empty() || rev.is_empty() {
                return DeltaAction::Ignore;
            }
            let modified_ts = entry
                .server_modified
                .as_deref()
                .map(parse_rfc3339_ts_to_unix)
                .unwrap_or(0);
            DeltaAction::Upsert(
                rel,
                RemoteFileMeta {
                    content_hash,
                    rev,
                    modified_ts,
                },
            )
        }
        "deleted" => DeltaAction::Remove(rel),
        _ => DeltaAction::Ignore,
    }
}

/// True if a `list_folder/continue` response signals an invalidated cursor
/// (HTTP 409 with a `reset` error) — the caller must re-snapshot. Pure.
pub(crate) fn is_reset_error(status: u16, body: &str) -> bool {
    status == 409 && (body.contains("\"reset\"") || body.contains("reset/"))
}

pub(crate) fn parse_rfc3339_ts_to_unix(input: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(input)
        .map(|v| v.with_timezone(&Utc).timestamp())
        .unwrap_or(0)
}

pub(crate) fn fetch_remote_file_metadata(
    state: &AppState,
    relative: &str,
) -> AppResult<Option<RemoteFileMeta>> {
    let token = get_access_token(state)?;
    let client = &state.http_client;
    let dropbox_path = normalize_dropbox_path(relative)?;

    let response = client
        .post("https://api.dropboxapi.com/2/files/get_metadata")
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({
            "path": dropbox_path,
            "include_media_info": false,
            "include_deleted": false
        }))
        .send()
        .map_err(|e| AppError::Network(format!("get_metadata request failed for {relative}: {e}")))?;

    if response.status().is_success() {
        let entry: DropboxEntry = response
            .json()
            .map_err(|e| AppError::Other(format!("get_metadata parse failed for {relative}: {e}")))?;
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

    let status = response.status();
    let body = response.text().unwrap_or_else(|_| "<unreadable body>".to_string());
    if status.as_u16() == 409 && (body.contains("not_found") || body.contains("path")) {
        return Ok(None);
    }
    Err(AppError::Dropbox {
        status: status.as_u16(),
        message: format!("get_metadata for {relative}: {body}"),
    })
}

/// Maps a single `list_folder`/`list_folder/continue` entry to the
/// `(lowercased path_display, RemoteFileMeta)` pair used to key the batched
/// remote index, or `None` when the entry isn't an indexable file (folders,
/// deleted entries, or files missing `content_hash`/`rev`/`path_display`).
fn remote_meta_from_entry(entry: &DropboxEntry) -> Option<(String, RemoteFileMeta)> {
    if entry.tag != "file" {
        return None;
    }
    let path_display = entry.path_display.as_deref()?;
    let content_hash = entry.content_hash.clone().unwrap_or_default();
    let rev = entry.rev.clone().unwrap_or_default();
    if content_hash.is_empty() || rev.is_empty() {
        return None;
    }
    let modified_ts = entry
        .server_modified
        .as_deref()
        .map(parse_rfc3339_ts_to_unix)
        .unwrap_or(0);
    Some((
        path_display.to_lowercase(),
        RemoteFileMeta {
            content_hash,
            rev,
            modified_ts,
        },
    ))
}

/// Fetches the metadata of every remote file in one recursive `list_folder`
/// sweep (paginated via `list_folder/continue`), keyed by lowercased
/// `path_display`.
///
/// This replaces one `get_metadata` HTTP request per local file with a
/// constant number of `list_folder` requests per sync tick.
///
/// SAFETY (mass-delete): the caller treats a path absent from the returned
/// map as "deleted remotely". A partial listing would therefore spuriously
/// mark still-present remote files as deleted and enqueue local deletions.
/// To prevent that, any request or parse failure aborts with `Err` instead
/// of returning whatever was collected so far.
pub(crate) fn fetch_all_remote_file_metadata(
    state: &AppState,
) -> AppResult<(HashMap<String, RemoteFileMeta>, String)> {
    let token = get_access_token(state)?;
    let client = &state.http_client;

    let mut remote_by_path: HashMap<String, RemoteFileMeta> = HashMap::new();

    let mut entries_resp: DropboxListFolderResponse = {
        let response = client
            .post("https://api.dropboxapi.com/2/files/list_folder")
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({
                "path": "",
                "recursive": true,
                "include_deleted": false
            }))
            .send()
            .map_err(|e| AppError::Network(format!("list_folder request failed: {e}")))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().unwrap_or_else(|_| "<unreadable body>".to_string());
            return Err(AppError::Dropbox {
                status: status.as_u16(),
                message: format!("list_folder for account root: {body}"),
            });
        }
        response
            .json()
            .map_err(|e| AppError::Other(format!("list_folder parse failed: {e}")))?
    };

    loop {
        for entry in &entries_resp.entries {
            if let Some((path_key, meta)) = remote_meta_from_entry(entry) {
                remote_by_path.insert(path_key, meta);
            }
        }

        if !entries_resp.has_more {
            break;
        }
        let response = client
            .post("https://api.dropboxapi.com/2/files/list_folder/continue")
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({ "cursor": entries_resp.cursor }))
            .send()
            .map_err(|e| AppError::Network(format!("list_folder/continue request failed: {e}")))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().unwrap_or_else(|_| "<unreadable body>".to_string());
            return Err(AppError::Dropbox {
                status: status.as_u16(),
                message: format!("list_folder/continue for account root: {body}"),
            });
        }
        entries_resp = response
            .json()
            .map_err(|e| AppError::Other(format!("list_folder/continue parse failed: {e}")))?;
    }

    // The final cursor points at the exact remote state this snapshot captured;
    // it is the correct seed for cursor-delta longpoll (DBSYNC-30).
    Ok((remote_by_path, entries_resp.cursor))
}

pub(crate) fn refresh_remote_index_and_enqueue_downloads_internal(
    state: &AppState,
) -> AppResult<usize> {
    let local_files = state.db.list_local_files()?;
    if local_files.is_empty() {
        return Ok(0);
    }

    let (remote_by_path, _cursor) = fetch_all_remote_file_metadata(state)?;

    let pending_targets = pending_job_targets(state)?;

    let mut enqueued = 0usize;
    for local in local_files {
        let rel = local.relative_path;
        if rel.ends_with(".cloudsc") || pending_targets.contains(&rel) {
            continue;
        }

        match remote_by_path.get(&normalize_dropbox_path(&rel)?.to_lowercase()) {
            Some(remote_meta) => enqueued += reconcile_remote_present(state, &rel, remote_meta)?,
            None => enqueued += reconcile_remote_absent(state, &rel)?,
        }
    }

    // Summarise a remote-index refresh that enqueued work (DBSYNC-47); a no-op
    // refresh stays silent so the periodic sweep doesn't spam the log.
    if enqueued > 0 {
        tracing::info!(
            enqueued,
            "remote index refresh enqueued download/delete jobs"
        );
    }
    Ok(enqueued)
}

/// The set of relative paths with an in-flight job, so we don't enqueue a
/// duplicate download/delete for a file already being processed.
fn pending_job_targets(state: &AppState) -> AppResult<HashSet<String>> {
    Ok(state
        .db
        .list_recent_jobs(400)?
        .iter()
        .filter(|j| j.status == "queued" || j.status == "retry_wait" || j.status == "running")
        .filter_map(|j| j.target_path.clone().or(j.source_path.clone()))
        .collect())
}

/// Reconcile a path that is PRESENT on the remote: record its metadata and, when
/// the remote content changed vs the last-synced state and the local copy
/// differs, enqueue a download. Returns jobs enqueued (0/1). Shared by the full
/// sweep and the cursor-delta path (DBSYNC-30) so both behave identically.
pub(crate) fn reconcile_remote_present(
    state: &AppState,
    rel: &str,
    remote_meta: &RemoteFileMeta,
) -> AppResult<usize> {
    let prev_remote = state.db.get_remote_file(rel)?;
    let should_download = match &prev_remote {
        None => false,
        Some(prev) => prev.content_hash != remote_meta.content_hash,
    };

    state.db.upsert_remote_file(
        rel,
        &remote_meta.content_hash,
        &remote_meta.rev,
        remote_meta.modified_ts,
    )?;

    if should_download {
        if let Some(local) = state.db.get_local_file(rel)? {
            if local.hash != remote_meta.content_hash {
                state.db.enqueue_job("download", Some(rel), Some(rel))?;
                return Ok(1);
            }
        }
    }
    Ok(0)
}

/// Reconcile a path that is ABSENT from the remote (deleted remotely). Propagates
/// a remote-wins local delete ONLY when the local copy still matches the
/// last-synced remote content; a diverged local copy is kept and flagged as a
/// conflict (never lost). Returns jobs enqueued (0/1). Shared by the full sweep
/// and the cursor-delta path.
pub(crate) fn reconcile_remote_absent(state: &AppState, rel: &str) -> AppResult<usize> {
    let Some(prev) = state.db.get_remote_file(rel)? else {
        // Never indexed remotely — the file may simply never have been uploaded.
        return Ok(0);
    };
    let Some(local) = state.db.get_local_file(rel)? else {
        // No local file to delete (already gone / dehydrated / never downloaded):
        // just drop the stale remote index row.
        state.db.remove_remote_file(rel)?;
        return Ok(0);
    };

    if local.hash == prev.content_hash {
        // Local matches the last-synced remote content: safe remote-wins delete.
        // The local_delete job clears both index rows.
        state.db.enqueue_job("local_delete", Some(rel), Some(rel))?;
        Ok(1)
    } else {
        // Local was modified while the remote was deleted: keep it, flag conflict.
        state
            .db
            .add_conflict(rel, rel, "remote deleted while local had unsynced changes")?;
        state.db.remove_remote_file(rel)?;
        if let Ok(mut engine) = state.sync_engine.lock() {
            engine.record_conflict();
        }
        Ok(0)
    }
}

/// Full recursive remote snapshot: reconcile the index against every local file
/// and persist the resulting cursor as the seed for cursor-delta longpoll. The
/// cursor MUST come from this same sweep so it points at exactly the state the
/// index now reflects (not a later `get_latest_cursor`). Returns the cursor.
pub(crate) fn seed_remote_delta_cursor(state: &AppState) -> AppResult<String> {
    let (remote_by_path, cursor) = fetch_all_remote_file_metadata(state)?;

    let local_files = state.db.list_local_files()?;
    if !local_files.is_empty() {
        let pending_targets = pending_job_targets(state)?;
        for local in local_files {
            let rel = local.relative_path;
            if rel.ends_with(".cloudsc") || pending_targets.contains(&rel) {
                continue;
            }
            match remote_by_path.get(&normalize_dropbox_path(&rel)?.to_lowercase()) {
                Some(meta) => {
                    reconcile_remote_present(state, &rel, meta)?;
                }
                None => {
                    reconcile_remote_absent(state, &rel)?;
                }
            }
        }
    }

    state
        .db
        .set_app_config(REMOTE_DELTA_CURSOR_KEY, &cursor)?;
    Ok(cursor)
}

/// Apply the remote changes since the persisted cursor (DBSYNC-30): call
/// `list_folder/continue`, apply each delta entry to `remote_file_index` via the
/// shared guarded reconcilers, and advance + persist the cursor per page. On an
/// invalidated cursor (`reset`), discard it and re-snapshot. Returns jobs
/// enqueued. The caller drains the queue.
pub(crate) fn apply_remote_delta(state: &AppState) -> AppResult<usize> {
    let mut cursor = match state.db.get_app_config(REMOTE_DELTA_CURSOR_KEY)? {
        Some(c) if !c.is_empty() => c,
        // No cursor yet: seed a fresh snapshot; the next longpoll continues from it.
        _ => {
            seed_remote_delta_cursor(state)?;
            return Ok(0);
        }
    };

    let token = get_access_token(state)?;
    let client = &state.http_client;
    let mut enqueued = 0usize;

    loop {
        let pending_targets = pending_job_targets(state)?;

        let response = client
            .post("https://api.dropboxapi.com/2/files/list_folder/continue")
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({ "cursor": cursor }))
            .send()
            .map_err(|e| AppError::Network(format!("list_folder/continue request failed: {e}")))?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response
                .text()
                .unwrap_or_else(|_| "<unreadable body>".to_string());
            if is_reset_error(status, &body) {
                // Cursor invalidated: discard it and re-snapshot from scratch.
                tracing::info!("remote delta cursor reset; re-snapshotting");
                state.db.set_app_config(REMOTE_DELTA_CURSOR_KEY, "")?;
                seed_remote_delta_cursor(state)?;
                return Ok(enqueued);
            }
            return Err(AppError::Dropbox {
                status,
                message: format!("list_folder/continue delta: {body}"),
            });
        }

        let resp: DropboxListFolderResponse = response
            .json()
            .map_err(|e| AppError::Other(format!("list_folder/continue parse failed: {e}")))?;

        for entry in &resp.entries {
            match delta_action_from_entry(entry) {
                DeltaAction::Upsert(rel, meta) => {
                    if !rel.ends_with(".cloudsc") && !pending_targets.contains(&rel) {
                        enqueued += reconcile_remote_present(state, &rel, &meta)?;
                    }
                }
                DeltaAction::Remove(rel) => {
                    if !rel.ends_with(".cloudsc") && !pending_targets.contains(&rel) {
                        enqueued += reconcile_remote_absent(state, &rel)?;
                    }
                }
                DeltaAction::Ignore => {}
            }
        }

        // Advance + persist per page so a crash mid-stream resumes cleanly.
        cursor = resp.cursor;
        state.db.set_app_config(REMOTE_DELTA_CURSOR_KEY, &cursor)?;

        if !resp.has_more {
            break;
        }
    }

    if enqueued > 0 {
        tracing::info!(enqueued, "longpoll delta enqueued download/delete jobs");
    }
    Ok(enqueued)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_entry(
        path_display: Option<&str>,
        content_hash: Option<&str>,
        rev: Option<&str>,
        server_modified: Option<&str>,
    ) -> DropboxEntry {
        DropboxEntry {
            tag: "file".to_string(),
            path_display: path_display.map(str::to_string),
            content_hash: content_hash.map(str::to_string),
            rev: rev.map(str::to_string),
            server_modified: server_modified.map(str::to_string),
            size: None,
        }
    }

    #[test]
    fn file_entry_maps_to_lowercased_key_and_parsed_ts() {
        let entry = file_entry(
            Some("/Docs/Report.TXT"),
            Some("hash123"),
            Some("rev1"),
            Some("2024-01-02T03:04:05Z"),
        );

        let result = remote_meta_from_entry(&entry);

        let (key, meta) = result.expect("file entry with hash+rev should map");
        assert_eq!(key, "/docs/report.txt");
        assert_eq!(meta.content_hash, "hash123");
        assert_eq!(meta.rev, "rev1");
        assert_eq!(meta.modified_ts, parse_rfc3339_ts_to_unix("2024-01-02T03:04:05Z"));
    }

    #[test]
    fn folder_entry_maps_to_none() {
        let mut entry = file_entry(Some("/Docs"), Some("hash123"), Some("rev1"), None);
        entry.tag = "folder".to_string();

        assert!(remote_meta_from_entry(&entry).is_none());
    }

    #[test]
    fn file_entry_with_empty_content_hash_maps_to_none() {
        let entry = file_entry(Some("/Docs/Report.txt"), Some(""), Some("rev1"), None);

        assert!(remote_meta_from_entry(&entry).is_none());
    }

    #[test]
    fn file_entry_missing_path_display_maps_to_none() {
        let entry = file_entry(None, Some("hash123"), Some("rev1"), None);

        assert!(remote_meta_from_entry(&entry).is_none());
    }

    // ---------------------------------------------------------------------------
    // Cursor-delta remote change detection (DBSYNC-30)
    // ---------------------------------------------------------------------------

    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    fn build_state() -> AppState {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("app.db");
        // Leak the tempdir so the DB file outlives the test body.
        std::mem::forget(dir);
        let db = crate::storage::db::Db::new_at(&db_path).expect("db init");
        AppState {
            secure_store: crate::storage::secure_store::SecureStore::new(),
            db: Arc::new(db),
            sync_engine: Arc::new(Mutex::new(crate::sync::engine::SyncEngine::new())),
            token_cache: Arc::new(Mutex::new(None)),
            scheduler_started: Arc::new(Mutex::new(false)),
            oauth_listener: Arc::new(Mutex::new(None)),
            sync_running: Arc::new(AtomicBool::new(false)),
            token_refresh_lock: Arc::new(Mutex::new(())),
            http_client: crate::state::build_http_client(),
        }
    }

    fn job_targets(state: &AppState, job_type: &str) -> Vec<String> {
        state
            .db
            .list_recent_jobs(200)
            .unwrap()
            .into_iter()
            .filter(|j| j.job_type == job_type)
            .filter_map(|j| j.target_path)
            .collect()
    }

    #[test]
    fn delta_action_classifies_file_deleted_folder_and_invalid() {
        match delta_action_from_entry(&file_entry(Some("/A/b.txt"), Some("h"), Some("r"), None)) {
            DeltaAction::Upsert(rel, meta) => {
                assert_eq!(rel, "A/b.txt");
                assert_eq!(meta.content_hash, "h");
                assert_eq!(meta.rev, "r");
            }
            other => panic!("expected Upsert, got {other:?}"),
        }

        let mut deleted = file_entry(Some("/A/gone.txt"), None, None, None);
        deleted.tag = "deleted".to_string();
        assert_eq!(
            delta_action_from_entry(&deleted),
            DeltaAction::Remove("A/gone.txt".to_string())
        );

        let mut folder = file_entry(Some("/A"), None, None, None);
        folder.tag = "folder".to_string();
        assert_eq!(delta_action_from_entry(&folder), DeltaAction::Ignore);

        // file with missing hash/rev, or missing path_display → Ignore
        assert_eq!(
            delta_action_from_entry(&file_entry(Some("/A/x"), None, Some("r"), None)),
            DeltaAction::Ignore
        );
        assert_eq!(
            delta_action_from_entry(&file_entry(None, Some("h"), Some("r"), None)),
            DeltaAction::Ignore
        );
    }

    #[test]
    fn is_reset_error_detects_reset_only() {
        assert!(is_reset_error(
            409,
            r#"{"error_summary":"reset/...","error":{".tag":"reset"}}"#
        ));
        assert!(!is_reset_error(409, r#"{"error_summary":"path/not_found/.."}"#));
        assert!(!is_reset_error(200, "reset/whatever"));
    }

    #[test]
    fn reconcile_remote_absent_deletes_when_local_matches_last_synced() {
        let state = build_state();
        state.db.upsert_remote_file("a.txt", "H", "rev", 0).unwrap();
        state.db.upsert_local_file("a.txt", "H", 3, 0).unwrap();

        let n = reconcile_remote_absent(&state, "a.txt").unwrap();
        assert_eq!(n, 1);
        assert_eq!(job_targets(&state, "local_delete"), vec!["a.txt".to_string()]);
    }

    #[test]
    fn reconcile_remote_absent_keeps_diverged_local_as_conflict() {
        let state = build_state();
        state.db.upsert_remote_file("b.txt", "H", "rev", 0).unwrap();
        state.db.upsert_local_file("b.txt", "DIFFERENT", 3, 0).unwrap();

        let n = reconcile_remote_absent(&state, "b.txt").unwrap();
        assert_eq!(n, 0, "a diverged local file must NOT be deleted");
        assert!(job_targets(&state, "local_delete").is_empty());
        assert!(state.db.get_remote_file("b.txt").unwrap().is_none());
    }

    #[test]
    fn reconcile_remote_absent_no_local_just_drops_remote_row() {
        let state = build_state();
        state.db.upsert_remote_file("c.txt", "H", "rev", 0).unwrap();

        let n = reconcile_remote_absent(&state, "c.txt").unwrap();
        assert_eq!(n, 0);
        assert!(state.db.get_remote_file("c.txt").unwrap().is_none());
        assert!(job_targets(&state, "local_delete").is_empty());
    }

    #[test]
    fn reconcile_remote_absent_never_indexed_is_noop() {
        let state = build_state();
        let n = reconcile_remote_absent(&state, "d.txt").unwrap();
        assert_eq!(n, 0);
        assert!(state.db.list_recent_jobs(50).unwrap().is_empty());
    }

    #[test]
    fn reconcile_remote_present_enqueues_download_on_remote_change() {
        let state = build_state();
        state.db.upsert_remote_file("e.txt", "OLD", "rev0", 0).unwrap();
        state.db.upsert_local_file("e.txt", "OLD", 3, 0).unwrap();

        let meta = RemoteFileMeta {
            content_hash: "NEW".to_string(),
            rev: "rev1".to_string(),
            modified_ts: 0,
        };
        let n = reconcile_remote_present(&state, "e.txt", &meta).unwrap();
        assert_eq!(n, 1);
        assert_eq!(job_targets(&state, "download"), vec!["e.txt".to_string()]);
        assert_eq!(
            state.db.get_remote_file("e.txt").unwrap().unwrap().content_hash,
            "NEW"
        );
    }

    #[test]
    fn reconcile_remote_present_no_download_when_unchanged() {
        let state = build_state();
        state.db.upsert_remote_file("f.txt", "SAME", "rev0", 0).unwrap();
        state.db.upsert_local_file("f.txt", "SAME", 3, 0).unwrap();

        let meta = RemoteFileMeta {
            content_hash: "SAME".to_string(),
            rev: "rev0".to_string(),
            modified_ts: 0,
        };
        let n = reconcile_remote_present(&state, "f.txt", &meta).unwrap();
        assert_eq!(n, 0);
        assert!(job_targets(&state, "download").is_empty());
    }

    #[test]
    fn reset_sync_state_clears_the_delta_cursor() {
        let state = build_state();
        state
            .db
            .set_app_config(REMOTE_DELTA_CURSOR_KEY, "cursor-abc")
            .unwrap();
        state.db.reset_sync_state().unwrap();
        assert_eq!(
            state.db.get_app_config(REMOTE_DELTA_CURSOR_KEY).unwrap(),
            None
        );
    }
}
