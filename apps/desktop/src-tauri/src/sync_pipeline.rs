use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

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
                        .map_err(|e| AppError::Io(e.to_string()))?
                        .to_string_lossy()
                        .replace('\\', "/");
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
            _ => {}
        }
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
                // DATA-LOSS GUARD (DBSYNC-45): a real file replaced by a
                // `<name>.cloudsc` placeholder was DEHYDRATED, not deleted by the
                // user. Propagating a remote delete here would destroy the user's
                // remote copy. Untrack it locally, but never delete remotely.
                if placeholder_exists(&tracked_root, &prev.relative_path) {
                    state.db.remove_local_file(&prev.relative_path)?;
                    continue;
                }
                state.db.enqueue_job(
                    "delete",
                    Some(&prev.relative_path),
                    Some(&prev.relative_path),
                )?;
                state.db.remove_local_file(&prev.relative_path)?;
                enqueued_jobs += 1;
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
                // DATA-LOSS GUARD (DBSYNC-45): a hydrated folder replaced by a
                // `<name>.cloudsc` placeholder was DEHYDRATED, not deleted. A
                // recursive remote `delete_v2` here would wipe the user's remote
                // folder. Untrack it locally, but never delete remotely.
                if placeholder_exists(&tracked_root, &rel) {
                    state.db.remove_known_folder(&rel)?;
                    continue;
                }
                state.db.enqueue_job("delete", Some(&rel), Some(&rel))?;
                state.db.remove_known_folder(&rel)?;
                enqueued_jobs += 1;
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

    use super::{run_sync_tick_internal, SYNC_BATCH_CAP};
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
}
