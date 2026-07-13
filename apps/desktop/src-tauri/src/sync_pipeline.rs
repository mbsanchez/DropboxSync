use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::{Duration, Utc};
use walkdir::WalkDir;

use crate::cloudsc_ops::hydrate_cloudsc_placeholder_internal;
use crate::dropbox_transfer::{
    delete_local_file_internal, delete_remote_file_internal, download_remote_file_internal,
    upload_local_file_internal,
};
use crate::error::{AppError, AppResult};
use crate::models::SyncTickResult;
use crate::path_util::{
    backoff_seconds, create_conflicted_copy, hash_file, normalize_dropbox_path,
    should_ignore_local_path,
};
use crate::remote_index::refresh_remote_index_and_enqueue_downloads_internal;
use crate::overlay_state;
use crate::state::AppState;
use crate::storage::db::FileIndexRow;

/// Max jobs drained from the queue in a single `run_sync_tick_internal` call, so
/// one tick makes real progress on large backlogs instead of processing exactly
/// one job every 60s (see DBSYNC-10).
const SYNC_BATCH_CAP: usize = 50;

pub(crate) fn refresh_queue_depth_internal(state: &AppState) -> AppResult<()> {
    let queue_depth = state.db.count_active_jobs()?;

    let failed_error = state.db.latest_failed_error()?;

    let mut engine = state
        .sync_engine
        .lock()
        .map_err(|_| AppError::Sync("sync engine lock poisoned".to_string()))?;
    engine.set_queue_depth(queue_depth);
    match failed_error {
        Some(msg) => engine.set_last_error(msg),
        None => engine.clear_last_error(),
    }
    drop(engine);
    overlay_state::refresh_overlay_state_internal(state);
    Ok(())
}

/// True when a `<rel>.cloudsc` placeholder exists on disk for `rel`, i.e. the
/// path was DEHYDRATED (real file/folder replaced by its cloud placeholder)
/// rather than deleted by the user. Used to suppress spurious remote deletions.
fn placeholder_exists(tracked_root: &std::path::Path, rel: &str) -> bool {
    tracked_root.join(format!("{rel}.cloudsc")).exists()
}

/// Evaluate a single local file against the index and enqueue an upload if it is
/// new or changed (routing a change that races a still-pending upload to a
/// conflicted copy). Returns the number of jobs enqueued (0 or 1). Shared by the
/// full walk (`scan_local_changes_internal`) and the targeted watcher path
/// (`process_changed_paths`) so both behave identically (DBSYNC-29).
fn process_local_file_change(
    state: &AppState,
    tracked_root: &Path,
    relative: &str,
    absolute: &Path,
    known: Option<&FileIndexRow>,
    pending_targets: &mut HashSet<String>,
) -> AppResult<usize> {
    let (hash, size_bytes, modified_ts) = hash_file(absolute)?;

    match known {
        None => {
            state.db.enqueue_job("upload", Some(relative), Some(relative))?;
            state
                .db
                .upsert_local_file(relative, &hash, size_bytes, modified_ts)?;
            pending_targets.insert(relative.to_string());
            Ok(1)
        }
        Some(prev) if prev.hash != hash => {
            if pending_targets.contains(relative) {
                let conflicted_path = create_conflicted_copy(absolute)?;
                let conflicted_rel = conflicted_path
                    .strip_prefix(tracked_root)
                    .map_err(|e| AppError::Io(e.to_string()))?
                    .to_string_lossy()
                    .replace('\\', "/");
                state.db.add_conflict(
                    relative,
                    relative,
                    "concurrent local update while job pending",
                )?;
                state
                    .db
                    .enqueue_job("upload", Some(&conflicted_rel), Some(&conflicted_rel))?;
                state
                    .db
                    .upsert_local_file(relative, &hash, size_bytes, modified_ts)?;
                if let Ok(mut engine) = state.sync_engine.lock() {
                    engine.record_conflict();
                }
                Ok(1)
            } else {
                state.db.enqueue_job("upload", Some(relative), Some(relative))?;
                state
                    .db
                    .upsert_local_file(relative, &hash, size_bytes, modified_ts)?;
                pending_targets.insert(relative.to_string());
                Ok(1)
            }
        }
        _ => Ok(0),
    }
}

/// Propagate a single local FILE deletion to the remote, honoring the DBSYNC-45
/// dehydration guard (a `<rel>.cloudsc`-backed path was dehydrated, not deleted,
/// so it is only untracked, never remote-deleted). Returns jobs enqueued (0/1).
fn process_local_file_deletion(
    state: &AppState,
    tracked_root: &Path,
    prev_rel: &str,
) -> AppResult<usize> {
    if placeholder_exists(tracked_root, prev_rel) {
        state.db.remove_local_file(prev_rel)?;
        return Ok(0);
    }
    state.db.enqueue_job("delete", Some(prev_rel), Some(prev_rel))?;
    state.db.remove_local_file(prev_rel)?;
    Ok(1)
}

/// Propagate a single known-FOLDER deletion (recursive remote `delete_v2`),
/// honoring the DBSYNC-45 dehydration guard. Returns jobs enqueued (0/1).
fn process_known_folder_deletion(
    state: &AppState,
    tracked_root: &Path,
    rel: &str,
) -> AppResult<usize> {
    if placeholder_exists(tracked_root, rel) {
        state.db.remove_known_folder(rel)?;
        return Ok(0);
    }
    state.db.enqueue_job("delete", Some(rel), Some(rel))?;
    state.db.remove_known_folder(rel)?;
    Ok(1)
}

/// How a watched path currently resolves on disk. Distinguishes a genuine
/// absence (`NotFound`) from a transient stat failure (permission, Windows
/// sharing-violation, AV/network lock) — the latter must NEVER be treated as a
/// deletion, or a racy stat on a just-written file would enqueue a spurious
/// remote delete (DBSYNC-29 review; the full scan guards this via `walk_had_error`).
enum PathKind {
    File,
    Dir,
    Absent,
    Other,
}

/// Classify a path. `Err` means a non-`NotFound` IO error — the caller must skip
/// it (not delete). Uses `symlink_metadata` so a symlink is not followed.
fn classify_path(path: &Path) -> std::io::Result<PathKind> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_file() => Ok(PathKind::File),
        Ok(m) if m.is_dir() => Ok(PathKind::Dir),
        Ok(_) => Ok(PathKind::Other),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(PathKind::Absent),
        Err(e) => Err(e),
    }
}

/// Grace period before a targeted deletion is committed: re-stat once so a
/// delete-then-recreate save (some editors briefly unlink the target) is seen as
/// a modification, not a momentary remote delete + re-add (DBSYNC-29 review).
const DELETE_CONFIRM_MS: u64 = 150;

/// Targeted, network-free change detection for a specific set of `/`-relative
/// paths (from the filesystem watcher, DBSYNC-29). Re-evaluates each path against
/// the index and enqueues the same jobs the full scan would — without walking the
/// whole tree. The caller drains the queue. Deletions are only inferred from a
/// path being explicitly absent on disk (never from an incomplete walk), and the
/// whole-root-missing case is still gated by `is_dir()`, so this can't mass-delete.
pub(crate) fn process_changed_paths(state: &AppState, paths: &[String]) -> AppResult<usize> {
    let folder = state
        .db
        .get_sync_folder()?
        .ok_or_else(|| AppError::Sync("sync folder not configured".to_string()))?;
    let tracked_root = PathBuf::from(&folder);
    // Same catastrophic-mass-deletion guard as the full scan: if the whole root
    // is missing/unmounted, treat nothing as deleted.
    if !tracked_root.is_dir() {
        return Ok(0);
    }

    let known = state.db.list_local_files()?;
    let known_map: HashMap<String, FileIndexRow> = known
        .iter()
        .map(|f| (f.relative_path.clone(), f.clone()))
        .collect();
    let existing_jobs = state.db.list_recent_jobs(200)?;
    let mut pending_targets: HashSet<String> = existing_jobs
        .iter()
        .filter(|j| j.status == "queued" || j.status == "retry_wait" || j.status == "running")
        .filter_map(|j| j.target_path.clone())
        .collect();

    // Normalize + dedupe the incoming paths, dropping placeholders/ignored ones.
    let mut rels: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for p in paths {
        let rel = p.replace('\\', "/");
        let rel = rel.trim_start_matches('/').to_string();
        if rel.is_empty() || rel.ends_with(".cloudsc") || should_ignore_local_path(&rel) {
            continue;
        }
        if seen.insert(rel.clone()) {
            rels.push(rel);
        }
    }

    let mut enqueued = 0usize;
    for rel in rels {
        let absolute = tracked_root.join(&rel);
        let kind = match classify_path(&absolute) {
            Ok(k) => k,
            Err(e) => {
                // Transient stat failure — NOT a deletion. Skip; the next event or
                // the 5-min fallback scan reconciles it (DBSYNC-29 review blocker).
                tracing::warn!(path = %rel, error = %e, "stat failed; not treating as a deletion");
                continue;
            }
        };

        match kind {
            PathKind::File => {
                enqueued += process_local_file_change(
                    state,
                    &tracked_root,
                    &rel,
                    &absolute,
                    known_map.get(&rel),
                    &mut pending_targets,
                )?;
            }
            PathKind::Dir => {
                // A moved-in / newly-created directory: record it and enqueue any
                // pre-existing children (a bounded walk of just this subtree, not
                // the whole root). Recursive watching also emits child events.
                state.db.upsert_known_folder(&rel)?;
                for entry in WalkDir::new(&absolute).into_iter().flatten() {
                    if !entry.file_type().is_file() {
                        continue;
                    }
                    let child_rel = match entry.path().strip_prefix(&tracked_root) {
                        Ok(r) => r.to_string_lossy().replace('\\', "/"),
                        Err(_) => continue,
                    };
                    if child_rel.ends_with(".cloudsc") || should_ignore_local_path(&child_rel) {
                        continue;
                    }
                    enqueued += process_local_file_change(
                        state,
                        &tracked_root,
                        &child_rel,
                        entry.path(),
                        known_map.get(&child_rel),
                        &mut pending_targets,
                    )?;
                }
            }
            PathKind::Other => {
                // Symlink or other special file — not something we sync; skip.
                continue;
            }
            PathKind::Absent => {
                // Confirm the absence is stable before propagating any remote
                // delete, so a delete-then-recreate atomic save is treated as a
                // modification (its recreate event/re-stat wins) rather than a
                // momentary remote delete + re-add.
                std::thread::sleep(std::time::Duration::from_millis(DELETE_CONFIRM_MS));
                if !matches!(classify_path(&absolute), Ok(PathKind::Absent)) {
                    continue;
                }
                enqueued += enqueue_targeted_deletions(state, &tracked_root, &rel, &known)?;
            }
        }
    }

    Ok(enqueued)
}

/// Enqueue remote deletions for an explicitly-absent path: the file itself (if
/// tracked), every tracked descendant under a removed directory prefix, and any
/// matching `known_folders` rows. Every delete goes through the DBSYNC-45
/// dehydration guard in the per-item helpers.
fn enqueue_targeted_deletions(
    state: &AppState,
    tracked_root: &Path,
    rel: &str,
    known: &[FileIndexRow],
) -> AppResult<usize> {
    let mut enqueued = 0usize;

    // The path itself, if it was a tracked file.
    if known.iter().any(|k| k.relative_path == rel) {
        enqueued += process_local_file_deletion(state, tracked_root, rel)?;
    }

    // Tracked descendants of a removed directory prefix.
    let prefix = format!("{rel}/");
    for prev in known {
        if prev.relative_path == rel {
            continue; // already handled above
        }
        if prev.relative_path.starts_with(&prefix)
            && !prev.relative_path.ends_with(".cloudsc")
            && !should_ignore_local_path(&prev.relative_path)
        {
            enqueued += process_local_file_deletion(state, tracked_root, &prev.relative_path)?;
        }
    }

    // Matching known-folder rows (the removed dir itself + any sub-folders).
    for folder_rel in state.db.list_known_folders()? {
        if folder_rel == rel || folder_rel.starts_with(&prefix) {
            enqueued += process_known_folder_deletion(state, tracked_root, &folder_rel)?;
        }
    }

    Ok(enqueued)
}

pub(crate) fn scan_local_changes_internal(state: &AppState) -> AppResult<usize> {
    let folder = state
        .db
        .get_sync_folder()?
        .ok_or_else(|| AppError::Sync("sync folder not configured".to_string()))?;
    let known = state.db.list_local_files()?;
    let existing_jobs = state.db.list_recent_jobs(200)?;
    let pending_targets: HashSet<String> = existing_jobs
        .iter()
        .filter(|j| j.status == "queued" || j.status == "retry_wait" || j.status == "running")
        .filter_map(|j| j.target_path.clone())
        .collect();

    let tracked_root = PathBuf::from(&folder);

    // Safety guard against catastrophic mass-deletion: if the sync folder is
    // missing or inaccessible (unmounted drive, transient FS error, wrong path),
    // WalkDir yields nothing and every known file/folder would look "deleted",
    // enqueuing recursive remote deletes. Bail out instead of propagating that.
    if !tracked_root.is_dir() {
        return Ok(0);
    }

    let known_map: HashMap<String, FileIndexRow> = known
        .iter()
        .map(|f| (f.relative_path.clone(), f.clone()))
        .collect();

    let mut pending_targets = pending_targets;
    let mut seen_paths: HashSet<String> = HashSet::new();
    let mut enqueued_jobs = 0usize;
    // If any directory can't be read mid-walk (permission denied, AV/network
    // hiccup, root momentarily unlistable), its entries never enter
    // `seen_paths`/`seen_dirs` and would be mistaken for deletions — triggering
    // recursive remote `delete_v2`. Track that and skip deletion detection when
    // the walk was incomplete; uploads/downloads still proceed safely.
    let mut walk_had_error = false;

    for entry in WalkDir::new(&tracked_root).into_iter() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => {
                walk_had_error = true;
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let absolute = entry.path().to_path_buf();
        let relative = absolute
            .strip_prefix(&tracked_root)
            .map_err(|e| AppError::Io(e.to_string()))?
            .to_string_lossy()
            // Canonicalize to '/' so the in-memory key matches the '/'-normalized
            // index (DBSYNC-45); otherwise a hydrated file's '\'-key misses the
            // '/'-stored row and the scan re-uploads it every tick.
            .replace('\\', "/");

        if relative.ends_with(".cloudsc") {
            continue;
        }
        if should_ignore_local_path(&relative) {
            continue;
        }

        seen_paths.insert(relative.clone());

        enqueued_jobs += process_local_file_change(
            state,
            &tracked_root,
            &relative,
            &absolute,
            known_map.get(&relative),
            &mut pending_targets,
        )?;
    }

    // Only propagate FILE deletions when we trust the walk was complete.
    if !walk_had_error {
        for prev in known {
            if prev.relative_path.ends_with(".cloudsc") {
                continue;
            }
            if should_ignore_local_path(&prev.relative_path) {
                continue;
            }
            if !seen_paths.contains(&prev.relative_path) {
                enqueued_jobs +=
                    process_local_file_deletion(state, &tracked_root, &prev.relative_path)?;
            }
        }
    }

    // Track real (materialized) directories so a folder deletion — which has no
    // file content to diff — can still be detected: any previously-known folder
    // that is no longer present on disk must have been deleted locally, so its
    // remote counterpart needs to be deleted too (delete_v2 is recursive).
    let mut seen_dirs: HashSet<String> = HashSet::new();
    for entry in WalkDir::new(&tracked_root).into_iter() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => {
                walk_had_error = true;
                continue;
            }
        };
        if !entry.file_type().is_dir() {
            continue;
        }
        let absolute = entry.path().to_path_buf();
        let relative = absolute
            .strip_prefix(&tracked_root)
            .map_err(|e| AppError::Io(e.to_string()))?
            .to_string_lossy()
            // Canonicalize to '/' so the in-memory key matches the '/'-normalized
            // index (DBSYNC-45); otherwise a hydrated file's '\'-key misses the
            // '/'-stored row and the scan re-uploads it every tick.
            .replace('\\', "/");

        if relative.is_empty() {
            continue; // skip the sync root itself
        }
        if should_ignore_local_path(&relative) {
            continue;
        }

        seen_dirs.insert(relative.clone());
        state.db.upsert_known_folder(&relative)?;
    }

    // Only propagate FOLDER deletions (recursive remote delete) when the dir
    // walk was fully readable — a partial walk must never be treated as a batch
    // of deletions.
    if !walk_had_error {
        for rel in state.db.list_known_folders()? {
            if !seen_dirs.contains(&rel) {
                enqueued_jobs += process_known_folder_deletion(state, &tracked_root, &rel)?;
            }
        }
    }

    let remote_enqueued = refresh_remote_index_and_enqueue_downloads_internal(state)?;

    {
        let mut engine = state
            .sync_engine
            .lock()
            .map_err(|_| AppError::Sync("sync engine lock poisoned".to_string()))?;
        engine.set_last_scan_at(Utc::now().to_rfc3339());
    }

    refresh_queue_depth_internal(state)?;
    Ok(enqueued_jobs + remote_enqueued)
}

/// Drains the sync queue until it is empty (or an infra error stops it), in
/// batches. Shared by the `.cloudsc`-open drain and the filesystem watcher
/// (DBSYNC-29). The `sync_running` single-flight gate is the caller's
/// responsibility. The 1000-iteration cap is a runaway guard.
pub(crate) fn drain_sync_queue(state: &AppState) {
    let mut safety = 0usize;
    while safety < 1000 {
        safety += 1;
        match process_sync_queue_internal(state) {
            Ok(true) => continue,
            Ok(false) => break,
            Err(e) => {
                tracing::error!(error = %e, "process_sync_queue failed (drain)");
                break;
            }
        }
    }
}

pub(crate) fn process_sync_queue_internal(state: &AppState) -> AppResult<bool> {
    let next = state.db.pick_next_due_job()?;
    let Some(job) = next else {
        refresh_queue_depth_internal(state)?;
        return Ok(false);
    };

    let max_attempts = 5;
    let attempt = job.attempt_count + 1;

    // Relative path this job acts on (never a secret); used for INFO logging so a
    // user watching the log sees real sync activity (DBSYNC-47).
    let job_path = job
        .source_path
        .as_deref()
        .or(job.target_path.as_deref())
        .unwrap_or("")
        .to_string();
    tracing::info!(
        job_id = job.id,
        job_type = %job.job_type,
        path = %job_path,
        attempt,
        "sync job started"
    );

    let op_result: AppResult<()> = match job.job_type.as_str() {
        "upload" => job
            .source_path
            .as_deref()
            .ok_or_else(|| AppError::Sync("upload job missing source_path".to_string()))
            .and_then(|rel| upload_local_file_internal(state, rel, job.id)),
        "delete" => job
            .target_path
            .as_deref()
            .or(job.source_path.as_deref())
            .ok_or_else(|| AppError::Sync("delete job missing target_path/source_path".to_string()))
            .and_then(|rel| delete_remote_file_internal(state, rel)),
        "local_delete" => job
            .target_path
            .as_deref()
            .or(job.source_path.as_deref())
            .ok_or_else(|| {
                AppError::Sync("local_delete job missing target_path/source_path".to_string())
            })
            .and_then(|rel| delete_local_file_internal(state, rel)),
        "download" => job
            .target_path
            .as_deref()
            .or(job.source_path.as_deref())
            .ok_or_else(|| AppError::Sync("download job missing target_path/source_path".to_string()))
            .and_then(|rel| {
                download_remote_file_internal(state, &normalize_dropbox_path(rel)?)
            }),
        "hydrate_cloudsc" => job
            .source_path
            .as_deref()
            .ok_or_else(|| AppError::Sync("hydrate_cloudsc job missing source_path".to_string()))
            .and_then(|rel| hydrate_cloudsc_placeholder_internal(state, rel).map(|_| ())),
        other => Err(AppError::Sync(format!("unknown job_type: {other}"))),
    };

    match op_result {
        Ok(()) => {
            state.db.mark_job_completed(job.id)?;
            if let Ok(mut engine) = state.sync_engine.lock() {
                engine.record_job_processed();
            }
            tracing::info!(
                job_id = job.id,
                job_type = %job.job_type,
                path = %job_path,
                "sync job completed"
            );
        }
        Err(err) => {
            if attempt >= max_attempts {
                let msg = format!("job {} failed: {err}", job.id);
                tracing::error!(
                    job_id = job.id,
                    job_type = %job.job_type,
                    path = %job_path,
                    attempt,
                    error = %err,
                    "sync job failed (max attempts reached)"
                );
                state.db.mark_job_failed(job.id, attempt, Some(&msg))?;
            } else {
                let wait_secs = backoff_seconds(attempt);
                let retry_at = (Utc::now() + Duration::seconds(wait_secs)).to_rfc3339();
                let msg = format!(
                    "job {} retry scheduled in {}s (attempt {}): {err}",
                    job.id, wait_secs, attempt
                );
                tracing::warn!(
                    job_id = job.id,
                    job_type = %job.job_type,
                    path = %job_path,
                    attempt,
                    wait_secs,
                    error = %err,
                    "sync job failed; retry scheduled"
                );
                state
                    .db
                    .mark_job_retry_wait(job.id, attempt, &retry_at, Some(&msg))?;
            }
        }
    }

    // `refresh_queue_depth_internal` reconciles the engine's global error/health
    // from the DB, so per-job success no longer masks still-failed jobs.
    refresh_queue_depth_internal(state)?;
    Ok(true)
}

pub(crate) fn run_sync_tick_internal(state: &AppState) -> AppResult<SyncTickResult> {
    let enqueued_jobs = scan_local_changes_internal(state)?;
    if enqueued_jobs > 0 {
        tracing::info!(count = enqueued_jobs, "enqueued local changes for sync");
    }

    // Drain up to `SYNC_BATCH_CAP` due jobs in this tick instead of exactly one,
    // so large backlogs make real progress every 60s. Mirrors the drain loop in
    // `open_handlers::spawn_drain_sync_queue_if_idle`: `Ok(true)` keeps draining,
    // `Ok(false)` means the queue is empty (or nothing is due yet). Per-job
    // failures are handled inside `process_sync_queue_internal` (marked
    // retry_wait/failed, still `Ok(true)`); an `Err(_)` here is an infra/DB
    // error, so we stop this tick's drain and let the next tick retry, without
    // aborting the tick itself.
    let mut processed_job = false;
    for _ in 0..SYNC_BATCH_CAP {
        match process_sync_queue_internal(state) {
            Ok(true) => processed_job = true,
            Ok(false) => break,
            Err(e) => {
                tracing::error!(error = %e, "process_sync_queue failed (sync tick)");
                break;
            }
        }
    }

    let scanned_files = state.db.list_local_files()?.len();
    // Only summarise a tick that actually did something, so the idle 60s poll
    // doesn't spam the log (DBSYNC-47).
    if enqueued_jobs > 0 || processed_job {
        tracing::info!(
            scanned_files,
            enqueued_jobs,
            processed_job,
            "sync tick complete"
        );
    }
    Ok(SyncTickResult {
        scanned_files,
        enqueued_jobs,
        processed_job,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    use chrono::{Duration, Utc};
    use tempfile::tempdir;

    use super::{process_changed_paths, run_sync_tick_internal, SYNC_BATCH_CAP};
    use crate::state::AppState;
    use crate::storage::db::Db;
    use crate::storage::secure_store::SecureStore;
    use crate::sync::engine::SyncEngine;

    /// Builds an `AppState` backed by an isolated temp DB, with `sync_folder`
    /// pointed at a separate, empty temp directory (kept apart from the DB file
    /// itself, so `scan_local_changes_internal`'s directory walk never picks up
    /// the SQLite file as a "local change"). Using only "local_delete" jobs
    /// targeting non-existent relative paths keeps these tests free of any
    /// Dropbox network I/O: `delete_local_file_internal` no-ops when the local
    /// file is absent, and `refresh_remote_index_and_enqueue_downloads_internal`
    /// short-circuits (no network call) whenever `local_file_index` is empty,
    /// which it stays here since we enqueue jobs directly instead of going
    /// through the local file scan.
    fn build_state(root: &std::path::Path) -> AppState {
        let sync_folder = root.join("synced");
        std::fs::create_dir_all(&sync_folder).expect("create sync folder");
        let db_path = root.join("db").join("app.db");
        std::fs::create_dir_all(db_path.parent().unwrap()).expect("create db dir");
        let db = Db::new_at(&db_path).expect("db init");
        db.set_sync_folder(&sync_folder.to_string_lossy())
            .expect("set sync folder");
        AppState {
            secure_store: SecureStore::new(),
            db: Arc::new(db),
            sync_engine: Arc::new(Mutex::new(SyncEngine::new())),
            token_cache: Arc::new(Mutex::new(None)),
            scheduler_started: Arc::new(Mutex::new(false)),
            oauth_listener: Arc::new(Mutex::new(None)),
            sync_running: Arc::new(AtomicBool::new(false)),
            token_refresh_lock: Arc::new(Mutex::new(())),
            http_client: crate::state::build_http_client(),
        }
    }

    #[test]
    fn drains_multiple_jobs_up_to_batch_cap_in_one_tick() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());

        let total_jobs = SYNC_BATCH_CAP + 15;
        for i in 0..total_jobs {
            state
                .db
                .enqueue_job(
                    "local_delete",
                    Some(&format!("job-{i}.txt")),
                    Some(&format!("job-{i}.txt")),
                )
                .expect("enqueue");
        }

        let result = run_sync_tick_internal(&state).expect("tick");

        assert!(
            result.processed_job,
            "expected at least one job processed this tick"
        );

        let remaining = state.db.count_active_jobs().expect("count active");
        assert_eq!(
            remaining,
            total_jobs - SYNC_BATCH_CAP,
            "tick should drain exactly SYNC_BATCH_CAP jobs, leaving the rest queued"
        );

        let done_jobs = state
            .db
            .list_recent_jobs((total_jobs + 1) as i64)
            .expect("list jobs")
            .into_iter()
            .filter(|j| j.status == "done")
            .count();
        assert_eq!(
            done_jobs, SYNC_BATCH_CAP,
            "batch cap must not be exceeded within a single tick"
        );
    }

    #[test]
    fn empty_queue_tick_processes_nothing() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());

        let result = run_sync_tick_internal(&state).expect("tick");

        assert!(!result.processed_job);
        assert_eq!(result.enqueued_jobs, 0);
        assert_eq!(state.db.count_active_jobs().expect("count active"), 0);
    }

    #[test]
    fn retry_wait_job_with_future_retry_time_is_not_processed_this_tick() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());

        state
            .db
            .enqueue_job("local_delete", Some("future.txt"), Some("future.txt"))
            .expect("enqueue");

        // Move the job into `retry_wait` with a `next_retry_at` far in the future,
        // simulating a job that failed once and is backing off.
        let job = state
            .db
            .pick_next_due_job()
            .expect("pick job")
            .expect("job present");
        let future_retry_at = (Utc::now() + Duration::seconds(3600)).to_rfc3339();
        state
            .db
            .mark_job_retry_wait(job.id, 1, &future_retry_at, Some("simulated failure"))
            .expect("mark retry_wait");

        let result = run_sync_tick_internal(&state).expect("tick");

        assert!(
            !result.processed_job,
            "a retry_wait job whose next_retry_at is in the future must not run this tick"
        );
        assert_eq!(
            state.db.count_active_jobs().expect("count active"),
            1,
            "the future retry_wait job should remain queued/pending, untouched"
        );
    }

    // ---------------------------------------------------------------------------
    // Targeted per-path processing (DBSYNC-29)
    // ---------------------------------------------------------------------------

    fn sync_root(state: &AppState) -> std::path::PathBuf {
        std::path::PathBuf::from(state.db.get_sync_folder().unwrap().unwrap())
    }

    fn job_targets(state: &AppState, job_type: &str) -> Vec<String> {
        state
            .db
            .list_recent_jobs(500)
            .expect("jobs")
            .into_iter()
            .filter(|j| j.job_type == job_type)
            .filter_map(|j| j.target_path)
            .collect()
    }

    #[test]
    fn targeted_created_file_enqueues_upload_and_indexes() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        std::fs::write(sync_root(&state).join("a.txt"), b"hello").unwrap();

        let n = process_changed_paths(&state, &["a.txt".to_string()]).expect("process");
        assert_eq!(n, 1);
        assert_eq!(job_targets(&state, "upload"), vec!["a.txt".to_string()]);
        assert!(state.db.get_local_file("a.txt").unwrap().is_some());
    }

    #[test]
    fn targeted_modified_file_enqueues_upload() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        std::fs::write(sync_root(&state).join("m.txt"), b"new-content").unwrap();
        // Index an out-of-date hash so the on-disk content is a "modification".
        state.db.upsert_local_file("m.txt", "stale-hash", 3, 0).unwrap();

        let n = process_changed_paths(&state, &["m.txt".to_string()]).expect("process");
        assert_eq!(n, 1);
        assert_eq!(job_targets(&state, "upload"), vec!["m.txt".to_string()]);
    }

    #[test]
    fn targeted_removed_file_enqueues_delete() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        // Indexed but absent on disk → deletion.
        state.db.upsert_local_file("gone.txt", "h", 3, 0).unwrap();

        let n = process_changed_paths(&state, &["gone.txt".to_string()]).expect("process");
        assert_eq!(n, 1);
        assert_eq!(job_targets(&state, "delete"), vec!["gone.txt".to_string()]);
        assert!(state.db.get_local_file("gone.txt").unwrap().is_none());
    }

    #[test]
    fn targeted_dehydrated_file_is_not_remote_deleted() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        state.db.upsert_local_file("d.txt", "h", 3, 0).unwrap();
        // A `.cloudsc` placeholder means it was DEHYDRATED, not deleted (DBSYNC-45).
        std::fs::write(sync_root(&state).join("d.txt.cloudsc"), b"{}").unwrap();

        let n = process_changed_paths(&state, &["d.txt".to_string()]).expect("process");
        assert_eq!(n, 0);
        assert!(job_targets(&state, "delete").is_empty());
        assert!(state.db.get_local_file("d.txt").unwrap().is_none()); // untracked, not deleted
    }

    #[test]
    fn targeted_ignored_and_cloudsc_paths_are_skipped() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());

        let n = process_changed_paths(
            &state,
            &[
                ".DS_Store".to_string(),
                "x.cloudsc".to_string(),
                "._resource".to_string(),
            ],
        )
        .expect("process");
        assert_eq!(n, 0);
        assert!(state.db.list_recent_jobs(50).unwrap().is_empty());
    }

    #[test]
    fn targeted_removed_directory_deletes_children_and_folder() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        state.db.upsert_local_file("dir/a.txt", "h", 1, 0).unwrap();
        state.db.upsert_local_file("dir/b.txt", "h", 1, 0).unwrap();
        state.db.upsert_known_folder("dir").unwrap();
        // Nothing on disk under `dir` → the whole folder was removed.

        let n = process_changed_paths(&state, &["dir".to_string()]).expect("process");
        assert_eq!(n, 3);
        let deletes = job_targets(&state, "delete");
        assert!(deletes.contains(&"dir/a.txt".to_string()));
        assert!(deletes.contains(&"dir/b.txt".to_string()));
        assert!(deletes.contains(&"dir".to_string()));
        assert!(state.db.get_local_file("dir/a.txt").unwrap().is_none());
        assert!(state.db.list_known_folders().unwrap().is_empty());
    }

    #[test]
    fn targeted_missing_sync_root_bails_out() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        state
            .db
            .set_sync_folder(&tmp.path().join("does-not-exist").to_string_lossy())
            .unwrap();
        state.db.upsert_local_file("a.txt", "h", 1, 0).unwrap();

        let n = process_changed_paths(&state, &["a.txt".to_string()]).expect("process");
        assert_eq!(n, 0, "a missing root must never be read as a mass deletion");
        assert!(state.db.list_recent_jobs(50).unwrap().is_empty());
    }
}
