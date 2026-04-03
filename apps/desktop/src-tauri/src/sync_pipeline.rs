use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use chrono::{Duration, Utc};
use walkdir::WalkDir;

use crate::cloudsc_ops::hydrate_cloudsc_placeholder_internal;
use crate::dropbox_transfer::{
    delete_remote_file_internal, download_remote_file_internal, upload_local_file_internal,
};
use crate::models::SyncTickResult;
use crate::path_util::{
    backoff_seconds, create_conflicted_copy, hash_file, normalize_dropbox_path,
    should_ignore_local_path,
};
use crate::remote_index::refresh_remote_index_and_enqueue_downloads_internal;
use crate::state::AppState;
use crate::storage::db::FileIndexRow;

pub(crate) fn refresh_queue_depth_internal(state: &AppState) -> Result<(), String> {
    let queue_depth = state.db.count_active_jobs()?;

    let mut engine = state
        .sync_engine
        .lock()
        .map_err(|_| "sync engine lock poisoned".to_string())?;
    engine.set_queue_depth(queue_depth);
    Ok(())
}

pub(crate) fn scan_local_changes_internal(state: &AppState) -> Result<usize, String> {
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
            _ => {}
        }
    }

    for prev in known {
        if prev.relative_path.ends_with(".cloudsc") {
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

pub(crate) fn process_sync_queue_internal(state: &AppState) -> Result<bool, String> {
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
        "hydrate_cloudsc" => job
            .source_path
            .as_deref()
            .ok_or_else(|| "hydrate_cloudsc job missing source_path".to_string())
            .and_then(|rel| hydrate_cloudsc_placeholder_internal(state, rel).map(|_| ())),
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

pub(crate) fn run_sync_tick_internal(state: &AppState) -> Result<SyncTickResult, String> {
    let enqueued_jobs = scan_local_changes_internal(state)?;
    let processed_job = process_sync_queue_internal(state)?;
    let scanned_files = state.db.list_local_files()?.len();
    Ok(SyncTickResult {
        scanned_files,
        enqueued_jobs,
        processed_job,
    })
}
