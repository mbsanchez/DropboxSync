use std::collections::{HashMap, HashSet};

use chrono::Utc;

use crate::auth_session::get_access_token;
use crate::error::{AppError, AppResult};
use crate::models::{DropboxEntry, DropboxListFolderResponse};
use crate::path_util::normalize_dropbox_path;
use crate::state::AppState;
use crate::storage::db::FileIndexRow;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RemoteFileMeta {
    pub content_hash: String,
    pub rev: String,
    pub modified_ts: i64,
    /// Dropbox's stable identifier for this item (DBSYNC-99).
    ///
    /// It rides in the **value**, deliberately. The obvious move was to rekey
    /// `remote_by_path` by identifier, which would propagate to every consumer and every
    /// iteration of the map; comparing ids across two snapshots detects a move just as
    /// well and costs nothing structural. If File Provider's enumeration turns out to
    /// need identifier-keyed lookup, that is DBSYNC-95's cost to carry, not this one's.
    ///
    /// `Option` is defensive only. A missing id must never disqualify an entry — see
    /// `remote_meta_from_entry`.
    pub id: Option<String>,
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
                    id: entry.id.clone(),
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
        .map_err(|e| {
            AppError::Network(format!("get_metadata request failed for {relative}: {e}"))
        })?;

    if response.status().is_success() {
        let entry: DropboxEntry = response.json().map_err(|e| {
            AppError::Other(format!("get_metadata parse failed for {relative}: {e}"))
        })?;
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
            id: entry.id,
        }));
    }

    let status = response.status();
    let body = response
        .text()
        .unwrap_or_else(|_| "<unreadable body>".to_string());
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
    // SAFETY (mass-delete): a missing `id` is deliberately NOT a reason to return None,
    // unlike a missing hash or rev. A path absent from the returned map is read by the
    // caller as "deleted remotely", so disqualifying entries on a field this function
    // merely records would enqueue local deletions for files that are still there.
    Some((
        path_display.to_lowercase(),
        RemoteFileMeta {
            content_hash,
            rev,
            modified_ts,
            id: entry.id.clone(),
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
            let body = response
                .text()
                .unwrap_or_else(|_| "<unreadable body>".to_string());
            return Err(AppError::Dropbox {
                status: status.as_u16(),
                message: format!("list_folder for account root: {body}"),
            });
        }
        response
            .json()
            .map_err(|e| AppError::Other(format!("list_folder parse failed: {e}")))?
    };

    // Pagination completeness (mass-delete safety, see doc comment above): this loop
    // only exits via `break` after a page with `has_more == false`. Every
    // `list_folder/continue` call above is guarded by `?` on both the HTTP request and
    // the JSON parse, so a failure on any page propagates as `Err` and unwinds out of
    // this function — the caller never receives a map that silently stops short of the
    // full snapshot.
    loop {
        for entry in &entries_resp.entries {
            if let Some((path_key, meta)) = remote_meta_from_entry(entry) {
                remote_by_path.insert(path_key, meta);
                continue;
            }
            // DBSYNC-99: folders never enter the file index — `remote_meta_from_entry`
            // rejects them, and they must keep being rejected, because a path absent from
            // this map is read as "deleted remotely". But folders carry identifiers too
            // (13 of 13 in the captured listing) and `known_folders` is where a folder
            // rename will have to preserve one. Record it on the row we already have.
            //
            // Best-effort and behaviour-neutral by construction: the writer can only fill
            // a NULL id on an existing row. It cannot insert, delete, or touch any column
            // the pipeline reads today, so a failure here changes nothing.
            // This is the ONLY writer, and there is no reader yet — see
            // `Db::set_known_folder_dropbox_id`, which records why that is deliberate and
            // the two coverage holes any future reader has to allow for (DBSYNC-106).
            if entry.tag == "folder" {
                if let (Some(path_display), Some(id)) =
                    (entry.path_display.as_deref(), entry.id.as_deref())
                {
                    let rel = path_display.trim_start_matches('/');
                    if let Err(e) = state.db.set_known_folder_dropbox_id(rel, id) {
                        tracing::warn!(rel, error = %e, "could not record folder identifier");
                    }
                }
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
            let body = response
                .text()
                .unwrap_or_else(|_| "<unreadable body>".to_string());
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

    let enqueued = reconcile_remote_snapshot_with_breaker(
        state,
        &local_files,
        &remote_by_path,
        &pending_targets,
    )?;

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

/// Reconciles a fetched remote snapshot against the local index, gating inferred
/// deletions behind the DBSYNC-64 mass-deletion circuit breaker (remote→local
/// direction). PRESENT files are reconciled immediately and are NEVER gated (the
/// breaker only guards inferred deletions). For ABSENT files: a full snapshot
/// INFERS "deleted remotely" from a path's absence, unlike the cursor-delta's
/// explicit `.tag=="deleted"` entries (`apply_remote_delta`, always authoritative
/// and never gated) — so a wrong/incomplete snapshot could misclassify
/// still-present files as absent and mass-delete local copies. This computes how
/// many absent files would actually enqueue a `local_delete` (an unmodified local
/// copy — a diverged one becomes a conflict, never a delete, so it doesn't count)
/// and blocks the WHOLE absent batch this pass if that looks like a catastrophe
/// rather than an intentional bulk delete.
///
/// Shared by BOTH remote-snapshot callers that reconcile against the full local
/// index — the periodic full sweep
/// (`refresh_remote_index_and_enqueue_downloads_internal`) and
/// `seed_remote_delta_cursor` — so the cursor-reset re-snapshot path (which runs
/// with the full local index still intact) gets the exact same guard as the
/// periodic sweep (DBSYNC-64 CTO fix). Returns jobs enqueued.
fn reconcile_remote_snapshot_with_breaker(
    state: &AppState,
    local_files: &[FileIndexRow],
    remote_by_path: &HashMap<String, RemoteFileMeta>,
    pending_targets: &HashSet<String>,
) -> AppResult<usize> {
    let mut enqueued = 0usize;

    // PRESENT files: reconcile immediately, never gated by the breaker.
    for local in local_files {
        let rel = &local.relative_path;
        if rel.ends_with(".cloudsc")
            || crate::sync_pipeline::covered_by_active_job(rel, pending_targets)
        {
            continue;
        }
        // Skip-and-log, not `?` (DBSYNC-104): propagating here aborted the whole batch,
        // so one path the helper could not normalise stopped downloads and delete
        // reconciliation for every other file. Same pattern as `cloudsc_ops`.
        let key = match normalize_dropbox_path(rel) {
            Ok(p) => p.to_lowercase(),
            Err(error) => {
                tracing::warn!(rel = %rel, error = %error, "skipping unnormalizable path in remote sweep");
                continue;
            }
        };
        if let Some(remote_meta) = remote_by_path.get(&key) {
            enqueued += reconcile_remote_present(state, rel, remote_meta)?;
        }
    }

    // ABSENT files + mass-deletion circuit breaker.
    let (absent, delete_candidates) =
        remote_sweep_delete_candidates(state, local_files, remote_by_path, pending_targets)?;
    let tracked = local_files.len();

    let overridden =
        delete_candidates > 0 && crate::sync_pipeline::consume_mass_delete_override(state)?;

    if crate::sync_pipeline::is_mass_deletion(delete_candidates, tracked) && !overridden {
        crate::sync_pipeline::block_mass_deletion(
            state,
            delete_candidates,
            tracked,
            crate::sync_pipeline::MassDeleteSource::RemoteSweep,
        );
    } else {
        // Not a mass deletion this pass (or the user overrode it) → sync isn't paused.
        crate::sync_pipeline::clear_mass_delete_blocked(
            state,
            crate::sync_pipeline::MassDeleteSource::RemoteSweep,
        );
        for rel in &absent {
            enqueued += reconcile_remote_absent(state, rel)?;
        }
    }

    Ok(enqueued)
}

/// Pure decision helper for the DBSYNC-64 mass-deletion circuit breaker
/// (remote→local direction): given the local index and a fetched remote snapshot,
/// returns the relative paths ABSENT from the remote (candidates for
/// `reconcile_remote_absent`) alongside how many of them would actually enqueue a
/// `local_delete` — i.e. the local copy still matches the last-synced remote
/// content (`get_remote_file(rel).content_hash == local.hash`), exactly the
/// condition `reconcile_remote_absent` itself uses. A diverged local file becomes a
/// conflict instead of a delete, so it is intentionally NOT counted as a
/// mass-delete candidate. No network I/O — takes the already-fetched snapshot, so
/// it's unit-testable independent of `fetch_all_remote_file_metadata`.
fn remote_sweep_delete_candidates(
    state: &AppState,
    local_files: &[FileIndexRow],
    remote_by_path: &HashMap<String, RemoteFileMeta>,
    pending_targets: &HashSet<String>,
) -> AppResult<(Vec<String>, usize)> {
    let mut absent = Vec::new();
    let mut delete_candidates = 0usize;

    for local in local_files {
        let rel = &local.relative_path;
        if rel.ends_with(".cloudsc")
            || crate::sync_pipeline::covered_by_active_job(rel, pending_targets)
        {
            continue;
        }
        // SAFETY (mass-delete): this skip MUST stay above the `absent.push` below.
        // `absent` is read by the caller as "deleted remotely" and enqueues a
        // `local_delete`, so a path we cannot normalize must leave the loop here, not
        // fall through. Skipping costs one unreconciled file; treating it as absent
        // would delete a file that is present on both sides.
        //
        // It used to be `?`, which aborted the whole sweep for every other file
        // (DBSYNC-104).
        let key = match normalize_dropbox_path(rel) {
            Ok(p) => p.to_lowercase(),
            Err(error) => {
                tracing::warn!(rel = %rel, error = %error, "skipping unnormalizable path in delete sweep");
                continue;
            }
        };
        if remote_by_path.contains_key(&key) {
            continue; // present — handled by the caller's other loop.
        }

        // `local` is already the `FileIndexRow` for `rel` from `list_local_files`,
        // so its `.hash` is the current local content — no need to re-fetch it via
        // `get_local_file` (DBSYNC-64 review nit).
        if let Some(prev) = state.db.get_remote_file(rel)? {
            if local.hash == prev.content_hash {
                delete_candidates += 1;
            }
        }
        absent.push(rel.clone());
    }

    Ok((absent, delete_candidates))
}

/// The set of relative paths with an in-flight job, so we don't enqueue a
/// duplicate download/delete for a file already being processed.
/// The paths every active job names.
///
/// Consumers must ask about them with `covered_by_active_job`, never `.contains()`: a queued
/// folder move names only the two folder paths while everything underneath is equally in
/// flight, and four checks in this module asked the exact question until round 5 of the
/// DBSYNC-99 review. The sharpest was the delta's removal arm — a `Remove` for a descendant of
/// a pending move went unfiltered and enqueued a `local_delete`, costing that descendant the
/// identity this ticket exists to preserve.
fn pending_job_targets(state: &AppState) -> AppResult<HashSet<String>> {
    // DBSYNC-31: single indexed SQL query instead of scanning list_recent_jobs(400).
    state.db.active_job_paths()
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

    // DBSYNC-99: this is the path every remote observation flows through — the full
    // snapshot and the cursor delta both land here — so writing the identifier here is
    // the whole of the back-fill. No migration is needed: the next sweep fills every row.
    state.db.upsert_remote_file(
        rel,
        &remote_meta.content_hash,
        &remote_meta.rev,
        remote_meta.modified_ts,
        remote_meta.id.as_deref(),
    )?;

    if should_download {
        if let Some(local) = state.db.get_local_file(rel)? {
            // DBSYNC-56: a marked row makes this comparison trivially true, so a download is
            // enqueued even when the disk may already hold the new remote content. That is
            // wasteful but CORRECT, and deferring here would be a bug: `upsert_remote_file`
            // above has already advanced the row, so `should_download` would be false on
            // every subsequent sweep and the download would be lost outright. The redundant
            // case is caught at the download site, which compares the on-disk hash against the
            // recorded remote hash — not the fetched bytes, which are not in hand at that point.
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

    // DBSYNC-56: a row marked for rescan cannot answer the question this function asks.
    // Propagating the delete could destroy an edit that never reached Dropbox, so the safe
    // move is to do nothing this tick and leave the remote row in place, so the next sweep
    // asks again rather than forgetting the path.
    //
    // **What happens next is a race, and an earlier version of this comment claimed it was
    // a resolution.** It said the next scan "resolves with real data one tick later". It
    // does not. The scan clears the marker and enqueues an upload, and that upload's
    // skip-if-identical check does a LIVE `fetch_remote_file_metadata` request, which
    // returns `None` for a deleted file — so the skip does not fire and the upload can
    // RESURRECT a file the user deleted remotely. Only if the next remote sweep wins the
    // race does the conflict arm below run instead.
    //
    // (The remote row is kept for a different reason than that check: so the next sweep
    // still asks about the path rather than forgetting it. A second version of this comment
    // wrongly linked the two.)
    //
    // Deferring is still better than the alternatives — both other arms act on a hash we
    // know is untrustworthy — but it trades a guaranteed wrong answer for a likely one, and
    // that is worth knowing rather than being told it resolves cleanly. Closing it properly
    // means teaching the upload path to check remote-absence first, which is a different
    // module with its own blast radius: **DBSYNC-93**.
    if local.hash == crate::storage::db::Db::HASH_NEEDS_RESCAN {
        return Ok(0);
    }

    if local.hash == prev.content_hash {
        // Local matches the last-synced remote content: safe remote-wins delete.
        // The local_delete job clears both index rows.
        state.db.enqueue_job("local_delete", Some(rel), Some(rel))?;
        Ok(1)
    } else {
        // Local was modified while the remote was deleted: keep it, flag conflict.
        state.db.add_conflict(
            rel,
            rel,
            "remote deleted while local had unsynced changes",
            None,
            true,
        )?;
        state.db.remove_remote_file(rel)?;
        if let Ok(mut engine) = state.sync_engine.lock() {
            engine.record_conflict();
        }
        crate::sharing::notify_conflict(rel);
        Ok(0)
    }
}

/// Full recursive remote snapshot: reconcile the index against every local file
/// and persist the resulting cursor as the seed for cursor-delta longpoll. The
/// cursor MUST come from this same sweep so it points at exactly the state the
/// index now reflects (not a later `get_latest_cursor`). Returns the cursor.
///
/// DBSYNC-64 (CTO fix): this reconciles against the SAME kind of absence-inferred
/// snapshot as the periodic full sweep, and it is reachable with a FULL local
/// index intact — not just on first login/after `reset_sync_state` (which wipes
/// `local_file_index` first, so an empty index makes the breaker a no-op there),
/// but also via the Dropbox cursor-RESET path: `apply_remote_delta` catches a
/// `reset` error, clears only the cursor, and calls this function while
/// `local_file_index` is untouched. A wrong/short snapshot on that path would
/// otherwise mass-delete local files ungated — so this goes through the exact
/// same `reconcile_remote_snapshot_with_breaker` gate as the periodic sweep.
pub(crate) fn seed_remote_delta_cursor(state: &AppState) -> AppResult<String> {
    let (remote_by_path, cursor) = fetch_all_remote_file_metadata(state)?;

    let local_files = state.db.list_local_files()?;
    if !local_files.is_empty() {
        let pending_targets = pending_job_targets(state)?;
        reconcile_remote_snapshot_with_breaker(
            state,
            &local_files,
            &remote_by_path,
            &pending_targets,
        )?;
    }

    state.db.set_app_config(REMOTE_DELTA_CURSOR_KEY, &cursor)?;
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
                    if !rel.ends_with(".cloudsc")
                        && !crate::sync_pipeline::covered_by_active_job(&rel, &pending_targets)
                    {
                        enqueued += reconcile_remote_present(state, &rel, &meta)?;
                        // DBSYNC-59: surface a newly-appeared remote file as a native
                        // placeholder within seconds (targeted — just this file) instead
                        // of waiting for the 5-min indexer.
                        #[cfg(windows)]
                        {
                            let path_display = entry.path_display.clone().unwrap_or_default();
                            crate::cloudsc_ops::materialize_remote_only_file_if_absent(
                                state,
                                &rel,
                                &path_display,
                                &meta.content_hash,
                                &meta.rev,
                                entry.size.unwrap_or(0),
                                meta.modified_ts,
                            );
                        }
                    }
                }
                DeltaAction::Remove(rel) => {
                    if !rel.ends_with(".cloudsc")
                        && !crate::sync_pipeline::covered_by_active_job(&rel, &pending_targets)
                    {
                        enqueued += reconcile_remote_absent(state, &rel)?;
                        // DBSYNC-59: purge a legacy `.cloudsc` sidecar for the removed
                        // file now (CfAPI placeholders are removed by the local_delete
                        // job above) instead of waiting for the 5-min prune.
                        #[cfg(windows)]
                        crate::cloudsc_ops::prune_cloudsc_sidecar_for(state, &rel);
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
    use crate::sync_pipeline::{consume_mass_delete_override, is_mass_deletion};

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
            id: None,
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
        assert_eq!(
            meta.modified_ts,
            parse_rfc3339_ts_to_unix("2024-01-02T03:04:05Z")
        );
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
    // Stable identity (DBSYNC-99 slice 2)
    // ---------------------------------------------------------------------------

    /// A real `files/list_folder` entry, captured against a live account on 2026-09-11
    /// (DBSYNC-99 slice 1) from a throwaway file since deleted. Kept verbatim, extra
    /// fields included, because the defect being fixed is precisely that `DropboxEntry`
    /// declared six fields and carried no `deny_unknown_fields`, so serde dropped the
    /// identifier without a word. A hand-trimmed literal would not exercise that.
    const CAPTURED_FILE_ENTRY: &str = r#"{
        ".tag": "file",
        "client_modified": "2026-09-11T16:08:30Z",
        "content_hash": "300e2819c817c3c5b767493bcef06813052d3526c9353756b99b445015ed2e18",
        "id": "id:eTyPGjL6NDAAAAAAAAABwg",
        "is_downloadable": true,
        "name": "a.txt",
        "path_display": "/dbsync99-probe/a.txt",
        "path_lower": "/dbsync99-probe/a.txt",
        "property_groups": [],
        "rev": "65b374ba88e8456342f44",
        "server_modified": "2026-09-11T16:08:30Z",
        "size": 22
    }"#;

    /// The whole ticket rests on the identifier getting from the wire to the index.
    /// `models.rs` is the single parse point for Dropbox metadata, so this walks the
    /// real path: JSON → `DropboxEntry` → `RemoteFileMeta` → the stored row.
    #[test]
    fn a_dropbox_id_survives_the_parse_point_into_the_remote_index() {
        let entry: DropboxEntry = serde_json::from_str(CAPTURED_FILE_ENTRY).expect("parse");
        let (rel, meta) = remote_meta_from_entry(&entry).expect("indexable file");

        assert_eq!(meta.id.as_deref(), Some("id:eTyPGjL6NDAAAAAAAAABwg"));

        let state = build_state();
        state
            .db
            .upsert_remote_file(
                &rel,
                &meta.content_hash,
                &meta.rev,
                meta.modified_ts,
                meta.id.as_deref(),
            )
            .expect("upsert");

        let row = state.db.get_remote_file(&rel).expect("get").expect("row");
        assert_eq!(row.dropbox_id.as_deref(), Some("id:eTyPGjL6NDAAAAAAAAABwg"));
    }

    /// An identifier, once learned, must never be erased by a later write that does not
    /// carry one. Several paths write the same row — the delta loop, `get_metadata`, the
    /// placeholder sweep — and they do not all have an id in hand. Losing it silently
    /// would make an item look brand new, which is the exact defect this ticket exists
    /// to remove.
    #[test]
    fn an_id_less_upsert_never_erases_a_known_dropbox_id() {
        let state = build_state();
        state
            .db
            .upsert_remote_file("a.txt", "H", "rev1", 0, Some("id:ABC"))
            .expect("seed");

        // The pre-existing four-argument writer: no id to offer.
        state
            .db
            .upsert_remote_file("a.txt", "H2", "rev2", 5, None)
            .expect("update");

        let row = state
            .db
            .get_remote_file("a.txt")
            .expect("get")
            .expect("row");
        assert_eq!(row.content_hash, "H2", "the content update must still land");
        assert_eq!(
            row.dropbox_id.as_deref(),
            Some("id:ABC"),
            "the id must survive"
        );
    }

    /// Folders carry identifiers too — 13 of 13 in the captured listing — and a folder
    /// rename is the expensive case this ticket exists to fix. The writer is an UPDATE
    /// rather than an upsert on purpose: it must be incapable of creating a
    /// `known_folders` row, because that table drives local deletion detection.
    #[test]
    fn a_folder_identifier_fills_an_existing_row_and_never_creates_one() {
        let state = build_state();
        state.db.upsert_known_folder("dir").expect("known");

        assert!(
            state
                .db
                .set_known_folder_dropbox_id("dir", "id:FOLDER")
                .expect("set"),
            "a known folder must take the identifier"
        );
        assert!(
            !state
                .db
                .set_known_folder_dropbox_id("not-here", "id:GHOST")
                .expect("set"),
            "an unknown folder must be left alone, not invented"
        );
        assert_eq!(
            state.db.list_known_folders().expect("list"),
            vec!["dir".to_string()],
            "the folder set must be untouched"
        );

        // Already identified: a second sweep must not overwrite it.
        assert!(
            !state
                .db
                .set_known_folder_dropbox_id("dir", "id:DIFFERENT")
                .expect("set"),
            "a stored identifier stays authoritative"
        );
    }

    /// Dropbox cannot name an item it has never seen. A file created locally has no
    /// remote row until its upload succeeds, and that window is unbounded when uploads
    /// keep failing — so identity cannot be a thin mirror of Dropbox's id. Every local
    /// row gets its own identifier at index time, allocated locally and **never derived
    /// from the path**: Apple's SDK header warns an identifier may be recorded in system
    /// logs, and a path is user data.
    #[test]
    fn every_local_row_gets_an_identity_dropbox_has_never_seen() {
        let state = build_state();
        state.db.upsert_local_file("a.txt", "h", 1, 0).expect("a");
        state.db.upsert_local_file("b.txt", "h", 1, 0).expect("b");

        let a = state.db.get_local_file("a.txt").expect("get").unwrap();
        let b = state.db.get_local_file("b.txt").expect("get").unwrap();

        let (Some(a_id), Some(b_id)) = (a.item_id, b.item_id) else {
            panic!("every local row must carry an item_id");
        };
        assert_ne!(a_id, b_id, "identifiers must be distinct");

        // Re-indexing the same path must not mint a new identity — that is what makes it
        // an identity rather than a version.
        state.db.upsert_local_file("a.txt", "h2", 2, 9).expect("re");
        let again = state.db.get_local_file("a.txt").expect("get").unwrap();
        assert_eq!(
            again.item_id,
            Some(a_id),
            "identity must be stable in place"
        );
        assert_eq!(again.hash, "h2", "the content update must still land");
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

    /// DBSYNC-99. Confirming a move has two shapes, and using the file one on a folder is not
    /// cosmetic: `move_index_row` matches an exact path, so on a folder it touches nothing and
    /// the whole subtree is left stranded at paths that no longer exist on disk. That mistake
    /// was made once in this ticket and cost a data-loss defect, so the branch gets a test.
    #[test]
    fn confirming_a_move_picks_the_shape_from_the_source() {
        let state = build_state();

        // A folder: the subtree must travel.
        state.db.upsert_known_folder("d").unwrap();
        state.db.upsert_local_file("d/one.txt", "H", 1, 0).unwrap();
        state
            .db
            .upsert_remote_file("d/one.txt", "H", "rev", 0, Some("id:ONE"))
            .unwrap();
        let identity = state
            .db
            .get_local_file("d/one.txt")
            .unwrap()
            .unwrap()
            .item_id;

        crate::dropbox_transfer::apply_confirmed_move(&state, "d", "e").unwrap();

        assert_eq!(state.db.list_known_folders().unwrap(), vec!["e"]);
        assert!(
            state.db.get_local_file("d/one.txt").unwrap().is_none(),
            "the descendant must not be stranded at the old path"
        );
        assert_eq!(
            state
                .db
                .get_local_file("e/one.txt")
                .unwrap()
                .unwrap()
                .item_id,
            identity,
            "and it keeps its identity"
        );

        // A file: the single row travels and no folder is invented.
        state.db.upsert_local_file("solo.txt", "H", 1, 0).unwrap();
        crate::dropbox_transfer::apply_confirmed_move(&state, "solo.txt", "renamed.txt").unwrap();
        assert!(state.db.get_local_file("solo.txt").unwrap().is_none());
        assert!(state.db.get_local_file("renamed.txt").unwrap().is_some());
        assert_eq!(state.db.list_known_folders().unwrap(), vec!["e"]);
    }

    /// DBSYNC-99, found by manual QA against a live account rather than by any test here.
    ///
    /// Nothing wrote a `remote_file_index` row on the upload success path, so after a real
    /// upload there was no remote row until the next full sweep — and the content-agreement
    /// guard needs one, so a rename inside that window fell back to a delete plus a full
    /// re-upload and minted a fresh identity. The window is exactly when a user renames
    /// something: just after creating it.
    #[test]
    fn an_upload_records_what_dropbox_says_it_now_holds() {
        let state = build_state();
        // The real shape of a `files/upload` 200, captured on 2026-09-11.
        let raw = r#"{
            "client_modified": "2026-09-11T16:08:30Z",
            "content_hash": "300e2819c817c3c5b767493bcef06813052d3526c9353756b99b445015ed2e18",
            "id": "id:eTyPGjL6NDAAAAAAAAABwg",
            "name": "a.txt",
            "path_display": "/qa/a.txt",
            "rev": "65b374ba88e8456342f44",
            "server_modified": "2026-09-11T16:08:30Z",
            "size": 22
        }"#;
        // No `.tag` — this is the real shape, and parsing it as a `DropboxEntry` fails.
        let entry: crate::models::UploadCommitResponse = serde_json::from_str(raw).expect("parse");
        crate::dropbox_transfer::record_upload_result(&state, "qa/a.txt", Some(entry));

        let row = state
            .db
            .get_remote_file("qa/a.txt")
            .expect("get")
            .expect("the row must exist the moment the upload commits");
        assert_eq!(
            row.content_hash,
            "300e2819c817c3c5b767493bcef06813052d3526c9353756b99b445015ed2e18"
        );
        assert_eq!(row.rev, "65b374ba88e8456342f44");
        assert_eq!(row.dropbox_id.as_deref(), Some("id:eTyPGjL6NDAAAAAAAAABwg"));

        // An unparseable response must not write a row and must not panic — the bytes are
        // already on Dropbox, so failing here would re-upload a file that is already there.
        crate::dropbox_transfer::record_upload_result(&state, "qa/b.txt", None);
        assert!(state.db.get_remote_file("qa/b.txt").expect("get").is_none());
    }

    /// DBSYNC-99 round 5. The sweep's skip test asked `.contains()` where the hazard is
    /// prefix-shaped: a queued `move d → e` names only `d` and `e`, while every descendant is
    /// equally mid-relocation. Reconciling one of them against a snapshot that does not list
    /// it enqueues work against a path the move is about to vacate.
    ///
    /// Mutating this guard left the suite green until this test existed.
    #[test]
    fn the_sweep_skips_descendants_of_a_pending_move() {
        let state = build_state();
        state.db.upsert_local_file("d/one.txt", "H", 3, 0).unwrap();
        state
            .db
            .upsert_remote_file("d/one.txt", "H", "rev", 0, Some("id:ONE"))
            .unwrap();

        // The snapshot DOES list it, with different content — so without the guard the
        // present-branch would reconcile and enqueue a download against a path the move is
        // about to vacate. A first version of this test left the snapshot empty, which never
        // reaches the present branch at all: it asserted nothing about the guard it names,
        // and said so only under mutation.
        let mut remote_by_path: HashMap<String, RemoteFileMeta> = HashMap::new();
        remote_by_path.insert(
            "/d/one.txt".to_string(),
            RemoteFileMeta {
                content_hash: "DIFFERENT".to_string(),
                rev: "rev2".to_string(),
                modified_ts: 9,
                id: Some("id:ONE".to_string()),
            },
        );
        // A queued folder move names the two folder paths, never the descendants.
        let pending: HashSet<String> = ["d".to_string(), "e".to_string()].into_iter().collect();

        let enqueued = reconcile_remote_snapshot_with_breaker(
            &state,
            &state.db.list_local_files().unwrap(),
            &remote_by_path,
            &pending,
        )
        .unwrap();

        assert_eq!(
            enqueued, 0,
            "nothing may be enqueued for a path mid-relocation"
        );
        assert!(
            job_targets(&state, "local_delete").is_empty(),
            "and least of all a local delete, which costs the descendant its identity"
        );
    }

    /// A refused move is re-derived here and now, in an order that matters.
    ///
    /// Three review rounds were spent leaving this to the next scan. Between the refusal and
    /// that scan the materialization sweep plants a `.cloudsc` sidecar over the source, and
    /// `process_local_file_deletion` reads a placeholder as a dehydration — dropping the
    /// delete along with the index row that remembers the path. The source stayed on Dropbox
    /// forever and the user got a duplicate instead of a rename.
    ///
    /// **The assertion that matters is that the delete does not exist yet.** The previous
    /// version of this test asserted `upload.id < delete.id` — the order the two jobs were
    /// *enqueued* in. That is a proxy, and the property it stood for is about the order they
    /// *drain* in, which id order does not decide: `pick_next_due_job` orders by id among the
    /// jobs that are DUE, and a `retry_wait` job with a future `next_retry_at` is not due.
    /// The test passed with that defect fully present. `refused_move_survives_a_transient_
    /// upload_failure` below is the falsifier it should have been.
    #[test]
    fn a_refused_file_move_enqueues_the_upload_and_owes_the_delete() {
        let state = build_state();
        state.db.upsert_local_file("old.txt", "H", 5, 0).unwrap();
        state
            .db
            .upsert_remote_file("old.txt", "H", "rev1", 0, Some("id:OLD"))
            .unwrap();

        crate::dropbox_transfer::rederive_refused_move(&state, "old.txt", "new.txt", true).unwrap();

        assert_eq!(
            job_targets(&state, "upload"),
            vec!["new.txt".to_string()],
            "the destination must be uploaded"
        );
        assert!(
            job_targets(&state, "delete").is_empty(),
            "and the source must NOT yet be queued for deletion — it is owed by the upload, \
             not scheduled beside it"
        );

        // The LOCAL row goes, so a later scan does not derive its own delete of the source —
        // which would be unordered against the upload.
        assert!(state.db.get_local_file("old.txt").unwrap().is_none());
        // The REMOTE row stays, and that is load-bearing: it is the app's only record that
        // Dropbox still holds the old name, and `unsettled_source_deletion` requires it.
        // Dropping it made the stranded notice unreachable for every refused move ever made,
        // because nothing can restore it — every sweep that could is driven from the local
        // index, which no longer has the path either.
        assert!(
            state.db.get_remote_file("old.txt").unwrap().is_some(),
            "the record that Dropbox still holds the old name must survive"
        );

        let upload = state
            .db
            .list_recent_jobs(50)
            .unwrap()
            .into_iter()
            .find(|j| j.job_type == "upload")
            .unwrap();

        // `source_path` is what actually gets uploaded (`process_sync_queue_internal`'s upload
        // arm dispatches on it) AND what the landing gate is evaluated against. Nothing
        // asserted it before, so binding it to the delete path instead — which uploads the old
        // name and gates the wrong destination, a total inversion of the feature — survived
        // the whole suite.
        assert_eq!(
            upload.source_path.as_deref(),
            Some("new.txt"),
            "the upload's source_path is the DESTINATION: it is the path uploaded and the path \
             the landing gate checks"
        );

        // The upload owes the deletion, carrying the rev captured before the row was dropped.
        assert_eq!(
            state.db.peek_deferred_source_delete(upload.id).unwrap(),
            Some(("old.txt".to_string(), Some("rev1".to_string())))
        );
        // Peeking does NOT consume it. Read-and-clear in one step meant every way the caller
        // can decline destroyed the debt on first sight.
        assert_eq!(
            state.db.peek_deferred_source_delete(upload.id).unwrap(),
            Some(("old.txt".to_string(), Some("rev1".to_string()))),
            "a peek must leave the debt intact — only a settled deletion clears it"
        );
        state.db.clear_deferred_source_delete(upload.id).unwrap();
        assert_eq!(
            state.db.peek_deferred_source_delete(upload.id).unwrap(),
            None
        );
    }

    /// A directory that has lost its `known_folders` row is still a directory.
    ///
    /// The shape used to be decided by that single lookup, and the wrong answer is expensive:
    /// the file branch would create an upload whose `source_path` is a directory — `File::open`
    /// on a directory fails, so five attempts burn and the job sticks — while the two
    /// `remove_*_file` calls are no-ops that strand every child row at the old prefix, and a
    /// RECURSIVE deletion of the source is owed to an upload that can never succeed.
    ///
    /// Index rows under the prefix answer the question on their own.
    #[test]
    fn a_directory_without_a_folder_row_does_not_take_the_file_branch() {
        let state = build_state();
        // Deliberately NO `upsert_known_folder("Docs")` — a partial prune, a half-applied
        // subtree move, any path that leaves the rows behind but not the folder.
        state.db.upsert_local_file("Docs/a.txt", "H", 5, 0).unwrap();
        state
            .db
            .upsert_remote_file("Docs/a.txt", "H", "rev1", 0, Some("id:A"))
            .unwrap();

        crate::dropbox_transfer::rederive_refused_move(&state, "Docs", "Papers", true).unwrap();

        assert!(
            job_targets(&state, "upload").is_empty(),
            "an upload of a directory path cannot work and owes a recursive delete"
        );
        assert!(
            state
                .db
                .list_refused_moves()
                .unwrap()
                .contains(&("Docs".to_string(), "Papers".to_string())),
            "it must take the directory branch, refusal recorded and all"
        );
        assert!(state.db.get_remote_file("Docs/a.txt").unwrap().is_some());
    }

    /// The DESTINATION on disk decides the shape, even with no index evidence at all.
    ///
    /// It is the only path that still exists — the source vanished, that is why we are here —
    /// so it is the only non-stale signal available. With every index clause silent, removing
    /// this one sent a real folder into the file branch: an upload whose `source_path` is a
    /// directory, and a recursive delete owed to it.
    #[test]
    fn the_destination_being_a_directory_on_disk_decides_the_shape() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = build_state();
        state
            .db
            .set_sync_folder(dir.path().to_string_lossy().as_ref())
            .unwrap();
        // The destination exists on disk as a directory. Nothing else says so: no
        // `known_folders` row, no rows under the prefix, source gone from disk.
        std::fs::create_dir_all(dir.path().join("Papers")).unwrap();

        crate::dropbox_transfer::rederive_refused_move(&state, "Docs", "Papers", true).unwrap();

        assert!(
            job_targets(&state, "upload").is_empty(),
            "no upload: `File::open` on a directory cannot work and the delete owed would be \
             recursive"
        );
        assert!(state
            .db
            .list_refused_moves()
            .unwrap()
            .contains(&("Docs".to_string(), "Papers".to_string())));
        std::mem::drop(dir);
    }

    /// `AlreadyGone` clears the remote row; `RevConflict` keeps it. Same job outcome, opposite
    /// statements about Dropbox — and conflating them is what created the phantom.
    #[test]
    fn only_a_genuine_not_found_clears_the_remote_row() {
        use crate::dropbox_transfer::{apply_delete_outcome, DeleteOutcome};

        let state = build_state();
        state
            .db
            .upsert_remote_file("gone.txt", "H", "rev", 0, None)
            .unwrap();
        state
            .db
            .upsert_remote_file("gone.txt/child.txt", "H", "rev", 0, None)
            .unwrap();
        assert!(apply_delete_outcome(&state, "gone.txt", DeleteOutcome::AlreadyGone).unwrap());
        assert!(
            state.db.get_remote_file("gone.txt").unwrap().is_none(),
            "Dropbox does not have it, so the row is a phantom and must go"
        );
        assert!(
            state
                .db
                .get_remote_file("gone.txt/child.txt")
                .unwrap()
                .is_none(),
            "and so must the subtree, since delete_v2 on a folder is recursive"
        );

        state
            .db
            .upsert_remote_file("back.txt", "H", "rev", 0, None)
            .unwrap();
        assert!(apply_delete_outcome(&state, "back.txt", DeleteOutcome::RevConflict).unwrap());
        assert!(
            state.db.get_remote_file("back.txt").unwrap().is_some(),
            "the file is back on the server under a new rev, so the row is TRUE and stays"
        );

        assert!(
            !apply_delete_outcome(&state, "back.txt", DeleteOutcome::Error).unwrap(),
            "a real error does not settle the job"
        );
    }

    /// When the refusal says the SOURCE is not on Dropbox, its remote row must go.
    ///
    /// `from_lookup/not_found` is the first permanent marker and means exactly that: a stale
    /// index made the correlator propose a move of a path Dropbox no longer holds. Keeping the
    /// remote row then left a phantom nothing could clear — every deletion path is driven from
    /// the local index, which no longer has it either — and a phantom permanently refuses any
    /// future rename INTO that path, because both correlators guard on
    /// `get_remote_file(destination).is_some()`.
    #[test]
    fn a_source_dropbox_does_not_have_leaves_no_phantom_row() {
        let state = build_state();
        state.db.upsert_local_file("old.txt", "H", 5, 0).unwrap();
        state
            .db
            .upsert_remote_file("old.txt", "H", "rev1", 0, Some("id:OLD"))
            .unwrap();

        // `source_may_still_exist = false` — the refusal was `from_lookup/not_found`.
        crate::dropbox_transfer::rederive_refused_move(&state, "old.txt", "new.txt", false)
            .unwrap();

        assert!(
            state.db.get_remote_file("old.txt").unwrap().is_none(),
            "a row for a path Dropbox does not have is a phantom that blocks every future \
             rename into it"
        );
    }

    /// An EMPTY tracked folder is still a directory.
    ///
    /// The `known_folders` clause is the original mechanism and the one that fires for every
    /// real folder refusal — and it was entirely unpinned, because every other directory test
    /// also seeds child index rows, so the prefix clause shadowed it. Both could be dead at
    /// once with the suite green. This case has nothing under it and nothing on disk, so only
    /// the folder row can answer.
    #[test]
    fn an_empty_tracked_folder_is_still_a_directory() {
        let state = build_state();
        state.db.upsert_known_folder("Empty").unwrap();

        crate::dropbox_transfer::rederive_refused_move(&state, "Empty", "Renamed", true).unwrap();

        assert!(
            job_targets(&state, "upload").is_empty(),
            "no upload: the path is a directory and `File::open` on one cannot work"
        );
        assert!(state
            .db
            .list_refused_moves()
            .unwrap()
            .contains(&("Empty".to_string(), "Renamed".to_string())));
    }

    /// ...and a tracked FILE is not a directory, whatever is left under its name.
    ///
    /// Leftover rows under `Notes/` — a directory that used to live at that name, a partial
    /// prune — made the prefix clause answer "directory" for a tracked file at `Notes`. The
    /// rename then silently degraded to delete-plus-upload, which is the whole regression this
    /// ticket removes, under a `warn!` claiming a folder move was refused. An exact
    /// `local_file_index` row settles it: a directory never has one.
    #[test]
    fn a_tracked_file_is_not_a_directory_however_stale_the_rows_beneath_it() {
        let state = build_state();
        state.db.upsert_local_file("Notes", "H", 5, 0).unwrap();
        state
            .db
            .upsert_remote_file("Notes", "H", "rev1", 0, Some("id:N"))
            .unwrap();
        // Stale descendants of a directory that once lived at this name.
        state
            .db
            .upsert_local_file("Notes/old.txt", "H2", 5, 0)
            .unwrap();

        crate::dropbox_transfer::rederive_refused_move(&state, "Notes", "Notes2", true).unwrap();

        // The ambiguous state resolves to DIRECTORY, on purpose, and this test was inverted to
        // say so. It previously asserted the file answer, on the premise that a directory
        // never has an exact `local_file_index` row — which is false, and the pipeline
        // produces the counterexample, so a real folder took the file branch.
        //
        // The two mistakes are not symmetrical. Answering "file" for a directory enqueues an
        // upload of a directory path that burns five attempts and sticks, strands every child
        // row, and owes a RECURSIVE delete. Answering "directory" for a file costs one
        // wasteful delete-plus-upload. When the evidence is genuinely ambiguous, take the
        // answer whose failure is cheap.
        assert!(
            job_targets(&state, "upload").is_empty(),
            "ambiguous evidence must not reach the file branch, which is the destructive one"
        );
        assert!(state
            .db
            .list_refused_moves()
            .unwrap()
            .contains(&("Notes".to_string(), "Notes2".to_string())));
        assert!(
            state.db.get_local_file("Notes").unwrap().is_some(),
            "and nothing is dropped: the directory branch writes no index changes"
        );
    }

    /// One upload can settle one source. A second refused move onto the same destination must
    /// not silently replace the first debt.
    ///
    /// The `DO UPDATE` used to overwrite it. A→X refused, then B→X refused, left the upload
    /// owing only B while A's index rows were already gone — an orphan on Dropbox that nothing
    /// would ever delete or index. A's rows must therefore survive the refusal.
    #[test]
    fn a_second_refused_move_onto_one_destination_does_not_replace_the_first_debt() {
        let state = build_state();
        for (path, id) in [("a.txt", "id:A"), ("b.txt", "id:B")] {
            state.db.upsert_local_file(path, "H", 5, 0).unwrap();
            state
                .db
                .upsert_remote_file(path, "H", "rev1", 0, Some(id))
                .unwrap();
        }

        crate::dropbox_transfer::rederive_refused_move(&state, "a.txt", "x.txt", true).unwrap();
        crate::dropbox_transfer::rederive_refused_move(&state, "b.txt", "x.txt", true).unwrap();

        let upload_id = state
            .db
            .list_recent_jobs(50)
            .unwrap()
            .into_iter()
            .find(|j| j.job_type == "upload")
            .unwrap()
            .id;
        assert_eq!(
            state
                .db
                .peek_deferred_source_delete(upload_id)
                .unwrap()
                .map(|(p, _)| p),
            Some("a.txt".to_string()),
            "the first debt stands"
        );
        assert!(
            state.db.get_remote_file("b.txt").unwrap().is_some(),
            "and the source whose deletion was NOT taken stays indexed, or it becomes an \
             orphan on Dropbox that nothing deletes and nothing knows about"
        );
        assert!(state.db.get_local_file("b.txt").unwrap().is_some());
    }

    /// The failure that broke the id-order design, reproduced.
    ///
    /// One transient upload failure — a 429, a 5xx, a file locked by another process — parks
    /// the upload in `retry_wait` with a future `next_retry_at`. It is then not in the due set
    /// at all, so its lower id decides nothing. Under the previous design the delete was
    /// already `queued` and became the next due job: Dropbox lost the source while the only
    /// other copy was still on its way up, for as long as the backoff lasted.
    ///
    /// Nothing may be due here. The source stays on Dropbox until the bytes have landed.
    #[test]
    fn refused_move_survives_a_transient_upload_failure() {
        let state = build_state();
        state.db.upsert_local_file("old.txt", "H", 5, 0).unwrap();
        state
            .db
            .upsert_remote_file("old.txt", "H", "rev1", 0, Some("id:OLD"))
            .unwrap();
        crate::dropbox_transfer::rederive_refused_move(&state, "old.txt", "new.txt", true).unwrap();

        let upload = state.db.pick_next_due_job().unwrap().unwrap();
        assert_eq!(upload.job_type, "upload");
        let far_future = (chrono::Utc::now() + chrono::Duration::seconds(120)).to_rfc3339();
        state
            .db
            .mark_job_retry_wait(upload.id, 1, &far_future, Some("429"))
            .unwrap();

        let due = state.db.pick_next_due_job().unwrap();
        assert!(
            due.is_none(),
            "while the upload is backing off, NOTHING may be due — a delete of the source \
             here removes the one copy Dropbox still has. Got: {:?}",
            due.map(|j| (j.job_type, j.target_path))
        );
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
        assert!(!is_reset_error(
            409,
            r#"{"error_summary":"path/not_found/.."}"#
        ));
        assert!(!is_reset_error(200, "reset/whatever"));
    }

    #[test]
    fn reconcile_remote_absent_deletes_when_local_matches_last_synced() {
        let state = build_state();
        state
            .db
            .upsert_remote_file("a.txt", "H", "rev", 0, None)
            .unwrap();
        state.db.upsert_local_file("a.txt", "H", 3, 0).unwrap();

        let n = reconcile_remote_absent(&state, "a.txt").unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            job_targets(&state, "local_delete"),
            vec!["a.txt".to_string()]
        );
    }

    /// DBSYNC-56. With the row marked for rescan, neither arm of this function can answer
    /// honestly: propagating the delete could destroy an edit that never reached Dropbox,
    /// and the conflict arm would hand the user a conflict record for an event they never
    /// saw. So it does nothing and lets the next scan resolve it with real data.
    ///
    /// Note what is asserted alongside: the REMOTE row survives. Dropping it would make the
    /// next sweep forget the path entirely, which is how "do nothing for now" quietly turns
    /// into "do nothing ever".
    #[test]
    fn reconcile_remote_absent_does_nothing_while_the_row_is_marked_for_rescan() {
        let state = build_state();
        state
            .db
            .upsert_remote_file("c.txt", "H", "rev", 0, None)
            .unwrap();
        // Seeded the way production does it: a real row, then marked. `upsert_local_file`
        // now refuses an empty hash in debug, so this is the only route.
        state.db.upsert_local_file("c.txt", "H2", 3, 0).unwrap();
        state.db.mark_local_file_for_rescan("c.txt").unwrap();

        let n = reconcile_remote_absent(&state, "c.txt").unwrap();

        assert_eq!(n, 0);
        assert!(
            job_targets(&state, "local_delete").is_empty(),
            "must not propagate a delete on a hash it cannot trust"
        );
        assert!(
            state.db.get_remote_file("c.txt").unwrap().is_some(),
            "the remote row must survive so the next sweep asks again"
        );
    }

    #[test]
    fn reconcile_remote_absent_keeps_diverged_local_as_conflict() {
        let state = build_state();
        state
            .db
            .upsert_remote_file("b.txt", "H", "rev", 0, None)
            .unwrap();
        state
            .db
            .upsert_local_file("b.txt", "DIFFERENT", 3, 0)
            .unwrap();

        let n = reconcile_remote_absent(&state, "b.txt").unwrap();
        assert_eq!(n, 0, "a diverged local file must NOT be deleted");
        assert!(job_targets(&state, "local_delete").is_empty());
        assert!(state.db.get_remote_file("b.txt").unwrap().is_none());
    }

    #[test]
    fn reconcile_remote_absent_no_local_just_drops_remote_row() {
        let state = build_state();
        state
            .db
            .upsert_remote_file("c.txt", "H", "rev", 0, None)
            .unwrap();

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
        state
            .db
            .upsert_remote_file("e.txt", "OLD", "rev0", 0, None)
            .unwrap();
        state.db.upsert_local_file("e.txt", "OLD", 3, 0).unwrap();

        let meta = RemoteFileMeta {
            content_hash: "NEW".to_string(),
            rev: "rev1".to_string(),
            modified_ts: 0,
            id: None,
        };
        let n = reconcile_remote_present(&state, "e.txt", &meta).unwrap();
        assert_eq!(n, 1);
        assert_eq!(job_targets(&state, "download"), vec!["e.txt".to_string()]);
        assert_eq!(
            state
                .db
                .get_remote_file("e.txt")
                .unwrap()
                .unwrap()
                .content_hash,
            "NEW"
        );
    }

    #[test]
    fn reconcile_remote_present_no_download_when_unchanged() {
        let state = build_state();
        state
            .db
            .upsert_remote_file("f.txt", "SAME", "rev0", 0, None)
            .unwrap();
        state.db.upsert_local_file("f.txt", "SAME", 3, 0).unwrap();

        let meta = RemoteFileMeta {
            content_hash: "SAME".to_string(),
            rev: "rev0".to_string(),
            modified_ts: 0,
            id: None,
        };
        let n = reconcile_remote_present(&state, "f.txt", &meta).unwrap();
        assert_eq!(n, 0);
        assert!(job_targets(&state, "download").is_empty());
    }

    // ── DBSYNC-104: one unnormalizable row must not delete itself or stop the sweep ──

    /// The row is planted directly, deliberately.
    ///
    /// Review M1 rejected an earlier version of this comment, which claimed the pipeline
    /// could not write such a key — at the time it could: `a\..\c.txt` is one legal macOS
    /// filename that `has_traversal` split into a traversal, so an ordinary file became a
    /// permanently-unnormalizable row. That is fixed at the source (DBSYNC-104 H3), and on
    /// Unix the remaining rejections — a `..` component between `/`s, an embedded NUL —
    /// are not producible as filenames. So the claim is true NOW, and only because the
    /// underlying defect was fixed rather than worked around here.
    ///
    /// Building this through the pipeline would therefore build a different scenario, and
    /// on Unix could not build this one at all.
    fn plant_poisoned_row(state: &AppState, rel: &str) {
        state
            .db
            .upsert_remote_file(rel, "H", "rev", 0, None)
            .unwrap();
        state.db.upsert_local_file(rel, "H", 3, 0).unwrap();
        assert!(
            normalize_dropbox_path(rel).is_err(),
            "{rel:?} must be unnormalizable or this test proves nothing"
        );
    }

    /// The whole safety argument of the skip is its POSITION: it sits above the
    /// `absent.push`, so an unnormalizable path leaves the loop instead of being read
    /// as "deleted remotely". If it ever moves below that line, this test goes red —
    /// which is the point, because the alternative is deleting a file that is present
    /// on both sides.
    #[test]
    fn an_unnormalizable_row_is_never_a_delete_candidate() {
        let state = build_state();
        plant_poisoned_row(&state, "a/../escape.txt");

        let local_files = state.db.list_local_files().unwrap();
        let remote_by_path: HashMap<String, RemoteFileMeta> = HashMap::new();
        let pending_targets: HashSet<String> = HashSet::new();

        let (absent, delete_candidates) =
            remote_sweep_delete_candidates(&state, &local_files, &remote_by_path, &pending_targets)
                .expect("an unnormalizable path must not abort the sweep");

        assert!(
            absent.is_empty(),
            "the unnormalizable path must not be reported absent — `absent` enqueues local deletes, got {absent:?}"
        );
        assert_eq!(
            delete_candidates, 0,
            "nothing may be counted as a delete candidate"
        );
    }

    /// The other half: the poisoned row must not take its neighbours down with it.
    /// Before the fix the `?` propagated, so one such row returned `Err` for the whole
    /// batch — no downloads, no delete reconciliation, no remote-present updates, for
    /// every other file, indefinitely.
    #[test]
    fn a_neighbour_still_reconciles_beside_an_unnormalizable_row() {
        let state = build_state();
        plant_poisoned_row(&state, "a/../escape.txt");
        state
            .db
            .upsert_remote_file("ok.txt", "H", "rev", 0, None)
            .unwrap();
        state.db.upsert_local_file("ok.txt", "H", 3, 0).unwrap();

        let local_files = state.db.list_local_files().unwrap();
        assert_eq!(local_files.len(), 2, "both rows are tracked");

        let remote_by_path: HashMap<String, RemoteFileMeta> = HashMap::new();
        let pending_targets: HashSet<String> = HashSet::new();

        let (absent, delete_candidates) =
            remote_sweep_delete_candidates(&state, &local_files, &remote_by_path, &pending_targets)
                .expect("the batch must survive one bad row");

        assert_eq!(
            absent,
            vec!["ok.txt".to_string()],
            "the healthy neighbour is still reconciled, and only it"
        );
        assert_eq!(delete_candidates, 1);
    }

    /// The PRESENT loop has its own copy of the same `?`, and `remote_sweep_delete_candidates`
    /// is itself called with `?` from this function — so fixing only one of the two left the
    /// abort fully intact. This drives the outer function to prove both were fixed.
    #[test]
    fn the_whole_sweep_survives_an_unnormalizable_row() {
        let state = build_state();
        plant_poisoned_row(&state, "a/../escape.txt");
        state
            .db
            .upsert_remote_file("ok.txt", "H", "rev", 0, None)
            .unwrap();
        state.db.upsert_local_file("ok.txt", "H", 3, 0).unwrap();

        let local_files = state.db.list_local_files().unwrap();
        let mut remote_by_path: HashMap<String, RemoteFileMeta> = HashMap::new();
        remote_by_path.insert(
            "/ok.txt".to_string(),
            RemoteFileMeta {
                content_hash: "H".to_string(),
                rev: "rev".to_string(),
                modified_ts: 0,
                id: None,
            },
        );
        let pending_targets: HashSet<String> = HashSet::new();

        reconcile_remote_snapshot_with_breaker(
            &state,
            &local_files,
            &remote_by_path,
            &pending_targets,
        )
        .expect("one unnormalizable row must not abort the whole sweep");
    }

    // ── DBSYNC-64: mass-deletion circuit breaker, remote→local (sweep) ─────────

    #[test]
    fn remote_sweep_delete_candidates_flags_matching_absent_files_as_mass_delete() {
        let state = build_state();
        // 30 tracked files, each with a local copy matching the last-synced remote
        // hash, and NONE present in this sweep's remote snapshot → all 30 are
        // `local_delete` candidates.
        for i in 0..30 {
            let rel = format!("f{i}.txt");
            state
                .db
                .upsert_remote_file(&rel, "H", "rev", 0, None)
                .unwrap();
            state.db.upsert_local_file(&rel, "H", 3, 0).unwrap();
        }
        let local_files = state.db.list_local_files().unwrap();
        assert_eq!(local_files.len(), 30);

        let remote_by_path: HashMap<String, RemoteFileMeta> = HashMap::new();
        let pending_targets: HashSet<String> = HashSet::new();

        let (absent, delete_candidates) =
            remote_sweep_delete_candidates(&state, &local_files, &remote_by_path, &pending_targets)
                .unwrap();

        assert_eq!(
            absent.len(),
            30,
            "every tracked file is absent from the snapshot"
        );
        assert_eq!(
            delete_candidates, 30,
            "every absent file matches its last-synced hash"
        );
        assert!(
            is_mass_deletion(delete_candidates, local_files.len()),
            "30/30 candidates must trip the breaker"
        );

        // Not overridden yet.
        assert!(!consume_mass_delete_override(&state).unwrap());

        // User confirms → the one-shot override lets the batch proceed, then is
        // consumed (a second check reads false again).
        state
            .db
            .set_app_config("mass_delete_override_once", "1")
            .unwrap();
        assert!(consume_mass_delete_override(&state).unwrap());
        assert!(!consume_mass_delete_override(&state).unwrap());
    }

    #[test]
    fn remote_sweep_delete_candidates_excludes_diverged_files_and_present_files() {
        let state = build_state();

        // Diverged local copy: absent from remote, but local hash no longer matches
        // the last-synced remote hash → NOT a delete candidate (becomes a conflict
        // via reconcile_remote_absent, never counted toward the breaker).
        state
            .db
            .upsert_remote_file("diverged.txt", "H", "rev", 0, None)
            .unwrap();
        state
            .db
            .upsert_local_file("diverged.txt", "DIFFERENT", 3, 0)
            .unwrap();

        // Never indexed remotely: absent from the snapshot, but there's no prior
        // remote row, so it can't be a remote-wins delete either.
        state
            .db
            .upsert_local_file("never_indexed.txt", "H", 3, 0)
            .unwrap();

        // Present in this sweep's snapshot: excluded from `absent` entirely.
        state
            .db
            .upsert_remote_file("present.txt", "H", "rev", 0, None)
            .unwrap();
        state
            .db
            .upsert_local_file("present.txt", "H", 3, 0)
            .unwrap();

        // Pending job: skipped like `.cloudsc` files, even though it would
        // otherwise be a clean delete candidate.
        state
            .db
            .upsert_remote_file("pending.txt", "H", "rev", 0, None)
            .unwrap();
        state
            .db
            .upsert_local_file("pending.txt", "H", 3, 0)
            .unwrap();

        let local_files = state.db.list_local_files().unwrap();
        let mut remote_by_path: HashMap<String, RemoteFileMeta> = HashMap::new();
        remote_by_path.insert(
            "/present.txt".to_string(),
            RemoteFileMeta {
                content_hash: "H".to_string(),
                rev: "rev".to_string(),
                modified_ts: 0,
                id: None,
            },
        );
        let mut pending_targets: HashSet<String> = HashSet::new();
        pending_targets.insert("pending.txt".to_string());

        let (absent, delete_candidates) =
            remote_sweep_delete_candidates(&state, &local_files, &remote_by_path, &pending_targets)
                .unwrap();

        let mut absent_sorted = absent.clone();
        absent_sorted.sort();
        assert_eq!(
            absent_sorted,
            vec!["diverged.txt".to_string(), "never_indexed.txt".to_string()]
        );
        assert_eq!(
            delete_candidates, 0,
            "neither absent file matches the diverged/never-indexed exclusion rules"
        );
        assert!(!is_mass_deletion(delete_candidates, local_files.len()));
    }

    #[test]
    fn reconcile_remote_snapshot_with_breaker_blocks_then_proceeds_on_override() {
        // Regression coverage for the CTO fix: `seed_remote_delta_cursor` (reached
        // via the cursor-reset path with a FULL local index intact, per
        // `apply_remote_delta`'s "reset" branch) and the periodic full sweep both
        // funnel through this exact function — so this test exercises the shared
        // gate both callers now get, without needing to mock the network calls
        // inside either caller.
        let state = build_state();
        for i in 0..30 {
            let rel = format!("g{i}.txt");
            state
                .db
                .upsert_remote_file(&rel, "H", "rev", 0, None)
                .unwrap();
            state.db.upsert_local_file(&rel, "H", 3, 0).unwrap();
        }
        let local_files = state.db.list_local_files().unwrap();
        assert_eq!(local_files.len(), 30);

        let remote_by_path: HashMap<String, RemoteFileMeta> = HashMap::new();
        let pending_targets: HashSet<String> = HashSet::new();

        // First pass: 30/30 absent+matching → BLOCKED. Nothing enqueued/deleted,
        // and the REMOTE-direction pause flag (not the scan one) is set.
        let enqueued = reconcile_remote_snapshot_with_breaker(
            &state,
            &local_files,
            &remote_by_path,
            &pending_targets,
        )
        .unwrap();
        assert_eq!(enqueued, 0, "a blocked mass deletion enqueues nothing");
        assert!(
            job_targets(&state, "local_delete").is_empty(),
            "a mass deletion must be blocked — no local_delete jobs enqueued"
        );
        assert_eq!(
            state.db.list_local_files().unwrap().len(),
            30,
            "blocked deletion must NOT drop the index rows"
        );
        assert!(
            state
                .db
                .get_app_config("mass_delete_blocked_remote")
                .unwrap()
                .is_some_and(|s| !s.is_empty()),
            "a blocked remote-sweep mass deletion must persist the REMOTE durable pause flag"
        );
        assert!(
            state
                .db
                .get_app_config("mass_delete_blocked_scan")
                .unwrap()
                .unwrap_or_default()
                .is_empty(),
            "the remote sweep must never touch the local-scan pause flag"
        );

        // User confirms → the override lets this batch of 30 through, and clears
        // the remote pause flag.
        state
            .db
            .set_app_config("mass_delete_override_once", "1")
            .unwrap();
        let enqueued = reconcile_remote_snapshot_with_breaker(
            &state,
            &local_files,
            &remote_by_path,
            &pending_targets,
        )
        .unwrap();
        assert_eq!(
            enqueued, 30,
            "an explicit override lets the reviewed batch through"
        );
        assert_eq!(job_targets(&state, "local_delete").len(), 30);
        assert!(
            state
                .db
                .get_app_config("mass_delete_blocked_remote")
                .unwrap()
                .unwrap_or_default()
                .is_empty(),
            "overriding the batch must clear the remote pause flag"
        );
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
