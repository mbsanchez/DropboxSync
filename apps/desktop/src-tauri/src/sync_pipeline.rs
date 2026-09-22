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
use crate::overlay_state;
use crate::path_util::{
    backoff_seconds, create_conflicted_copy, hash_file, is_builtin_ignored_local_path,
    is_editor_temp_path, is_ignored_local_path, normalize_dropbox_path, relpath_under, safe_join,
};
use crate::remote_index::refresh_remote_index_and_enqueue_downloads_internal;
use crate::state::AppState;
use crate::storage::db::FileIndexRow;

/// Max jobs drained from the queue in a single `run_sync_tick_internal` call, so
/// one tick makes real progress on large backlogs instead of processing exactly
/// one job every 60s (see DBSYNC-10).
const SYNC_BATCH_CAP: usize = 50;

pub(crate) fn refresh_queue_depth_internal(state: &AppState) -> AppResult<()> {
    let queue_depth = state.db.count_active_jobs()?;

    let failed_error = state.db.latest_failed_error()?;
    // DBSYNC-64: the mass-deletion pause is a DURABLE app_config flag, not a failed
    // job — so surface it here (with precedence over a transient job error) instead
    // of a bare `engine.set_last_error`, which this very function would then clear on
    // the next tick (the block enqueues no failed rows).
    //
    // Per-direction keys (CTO fix): the local scan and the remote sweep each have
    // their OWN durable pause flag so a benign pass in one direction can't clobber
    // the other's still-active pause message within the same tick (the remote sweep
    // runs right after the local scan in `scan_local_changes_internal`). Surface
    // whichever is set; the scan's message takes precedence if somehow both are.
    let scan_paused = state
        .db
        .get_app_config(MASS_DELETE_BLOCKED_SCAN_KEY)?
        .filter(|s| !s.is_empty());
    let remote_paused = state
        .db
        .get_app_config(MASS_DELETE_BLOCKED_REMOTE_KEY)?
        .filter(|s| !s.is_empty());
    let paused = scan_paused.or(remote_paused);

    // DBSYNC-99: a source deletion owed by an upload that will never run again.
    //
    // **It never recommends deleting anything, and that is the whole point of this shape.**
    // The first version said "Both names exist there; delete '{source}' by hand". Of the
    // states that reach here, most have nothing at the destination at all — an upload that
    // exhausted its attempts, one that no-oped because the source vanished, a gate that
    // returned false for want of a remote row — so the sentence was false and the source was
    // the user's ONLY copy. `destination_holds_the_bytes` withholds that same deletion saying
    // "a duplicate is recoverable and a wrong deletion is not", and this handed it to the user
    // without the gate. A message that prescribes a destructive action has to rest on the same
    // evidence the automatic path required; this one states facts and stops.
    //
    // Ranked BELOW `failed_error`. A job failure is actionable and clears itself; this is an
    // advisory that can persist, so it must never be the thing hiding an actionable error.
    //
    // Deliberately not a conflict row either. `resolve_conflict_internal` has no arm that
    // removes a stray remote path: a row with no conflicted copy and `remote_deleted = false`
    // lands in `(UseRemote, None, false)` / `(KeepBoth, None, false)`, both literal empty arms
    // that then mark it resolved. Telling someone they have fixed something they have not is
    // worse than saying plainly what happened.
    let stranded = state
        .db
        .unsettled_source_deletion()?
        .map(|(source, dest)| {
            // Only "both names exist" if the destination is actually observed on Dropbox. A
            // remote row can hold different content (the `to/conflict` case), and that is
            // still two names on Dropbox — but it is emphatically not a spare copy, so the
            // wording claims presence and never equivalence.
            //
            // An earlier version also branched on the job's `status`, and that disjunct was
            // dead: `latest_failed_error` returns `Some` whenever ANY job row is `failed`, and
            // it now outranks this, so a `failed` row's advisory is discarded before it is
            // read. Worse, it only changed the outcome when the destination WAS on Dropbox,
            // where it produced a false sentence. Inert where right, wrong where decisive.
            let destination_on_dropbox = match state.db.get_remote_file(&dest) {
                Ok(row) => row.is_some(),
                Err(e) => {
                    // Not knowing is not absence. Take the branch that asserts least about the
                    // user's Dropbox, and say why it was taken.
                    tracing::warn!(rel = %dest, error = %e, "could not read the destination's remote row while describing a stranded rename");
                    false
                }
            };
            if destination_on_dropbox {
                format!(
                    "'{source}' was renamed to '{dest}'. Dropbox holds both names; the old one could not be removed automatically."
                )
            } else {
                // No remote row is absence of KNOWLEDGE, not absence of the file: the bytes
                // can be on Dropbox with the row still missing — `record_upload_result` says
                // so itself when it fails to parse a commit response. So this claims only what
                // is certain, that Dropbox still holds the old name.
                format!(
                    "'{source}' was renamed to '{dest}', and the old name '{source}' could not be removed from Dropbox. Dropbox still holds it under the old name."
                )
            }
        });

    let mut engine = state
        .sync_engine
        .lock()
        .map_err(|_| AppError::Sync("sync engine lock poisoned".to_string()))?;
    engine.set_queue_depth(queue_depth);
    match paused.or(failed_error).or(stranded) {
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
pub(crate) fn placeholder_exists(tracked_root: &std::path::Path, rel: &str) -> bool {
    tracked_root.join(format!("{rel}.cloudsc")).exists()
}

/// Execution-time guard for a queued remote `delete`: suppress it when the local
/// path is now represented as CLOUD-ONLY — a `.cloudsc` sidecar OR a native CfAPI
/// dehydrated placeholder at the path itself. That means the path was DEHYDRATED to
/// free space, not deleted by the user, so deleting it on Dropbox would be data loss
/// (DBSYNC-33/45/59). Re-checking at drain time (not only at enqueue) closes the
/// brief `remove_file`→`CfCreatePlaceholders` window in the CfAPI dehydrate path,
/// where the enqueue-time `placeholder_exists` guard can miss the not-yet-created
/// placeholder. Best-effort: if the sync folder can't be read we do NOT suppress, so
/// a genuine user deletion still propagates.
fn delete_suppressed_by_dehydration(state: &AppState, rel: &str) -> bool {
    // DBSYNC-62 backstop at execution time: never run a remote delete while within the
    // sync-root registration grace window (placeholder eviction, not a user delete).
    #[cfg(windows)]
    if crate::cloud_filter::in_post_registration_grace() {
        return true;
    }
    // DBSYNC-66 Slice 1: drain-time backstop mirroring the `on_notify_delete` event-
    // time check — a delete under an actively-materializing subtree (restore churn)
    // must not run even if it slipped past the event-time suppression.
    #[cfg(windows)]
    if crate::cloud_filter::in_materialization_grace(rel) {
        return true;
    }
    let Ok(Some(folder)) = state.db.get_sync_folder() else {
        return false;
    };
    let root = std::path::Path::new(&folder);
    placeholder_exists(root, rel) || crate::path_util::is_dehydrated_placeholder(&root.join(rel))
}

/// Is `path` at, or underneath, any path an active job names?
///
/// **The hazard this answers is prefix-shaped, and for four review rounds the guards were
/// not.** Since DBSYNC-99 inverted the order, a queued folder move leaves the index
/// describing the old world while the disk describes the new one — so `d`, `e` AND everything
/// under either of them is in flight. `active_job_paths` returns only the two folder paths,
/// and every consumer used to ask `.contains()`, which is true for `d` and false for
/// `d/one.txt`. That gap deleted folders recursively, uploaded a renamed folder's contents as
/// strangers, and cost every descendant its identity.
///
/// One predicate, every call site. Anything that asks "is this path busy?" asks it here.
///
/// Compares the separator byte rather than building `format!("{p}/")`, because this runs
/// once per candidate per active job on every scan of a large index.
pub(crate) fn covered_by_active_job(path: &str, active: &HashSet<String>) -> bool {
    if active.contains(path) {
        return true;
    }
    active.iter().any(|prefix| {
        path.len() > prefix.len()
            && path.as_bytes()[prefix.len()] == b'/'
            && path.starts_with(prefix.as_str())
    })
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
    pending_moves: &HashSet<String>,
) -> AppResult<usize> {
    // DBSYNC-59: a Windows CfAPI dehydrated placeholder is cloud-only content that
    // just happens to be a real file on disk. Hashing it would open it and trigger
    // an on-demand download (hydration) — turning every scan tick into a full
    // re-download of every online-only file, and re-uploading unchanged bytes.
    // Treat it as present + in-sync: never hash, never enqueue, never delete. It
    // still reaches `seen_paths` in the caller, so genuine-delete detection is safe.
    if crate::path_util::is_dehydrated_placeholder(absolute) {
        return Ok(0);
    }
    let (hash, size_bytes, modified_ts) = hash_file(absolute)?;

    match known {
        None => {
            // DBSYNC-99: the destination of a queued move looks exactly like a stranger — on
            // disk, absent from the index — because the index is not rewritten until Dropbox
            // confirms the move. Uploading it here would put the bytes at the destination
            // before the move ran, so the move would then fail `to/conflict` and be dropped,
            // leaving the source behind on the server under its old name.
            if covered_by_active_job(relative, pending_moves) {
                return Ok(0);
            }
            state
                .db
                .enqueue_job("upload", Some(relative), Some(relative))?;
            state
                .db
                .upsert_local_file(relative, &hash, size_bytes, modified_ts)?;
            pending_targets.insert(relative.to_string());
            Ok(1)
        }
        Some(prev) if prev.hash != hash => {
            // A marked row (DBSYNC-56) lands in this arm by design, and it MUST still take
            // the conflicted-copy path when a job is pending. An earlier version of this
            // change excluded it, on the reasoning that a marker is bookkeeping rather than
            // a new edit and so should not manufacture a copy. That reasoning was wrong in
            // a way worth recording, because it re-created the very loss this ticket fixes:
            //
            // The only job that can be active on a MARKED path is one the remote side
            // queued — a download or a delete. It cannot be an upload: the upload that sets
            // the marker is completed the moment it returns, and the only other producer is
            // this function, which clears the marker in the same call. So a pending job here
            // means "remote content is about to land on top of local bytes Dropbox has never
            // received" — exactly the bytes the marker exists to protect.
            //
            // Taking the plain arm instead writes the real hash back, clearing the marker
            // BEFORE that download drains. `download_would_conflict` then sees a baseline
            // equal to the on-disk hash, returns false, and the download overwrites the
            // unuploaded edit with nothing logged. The C1 guard is disarmed by the very act
            // of skipping this branch.
            if covered_by_active_job(relative, pending_targets) {
                let conflicted_path = create_conflicted_copy(absolute)?;
                let conflicted_rel = relpath_under(tracked_root, &conflicted_path)?;
                state.db.add_conflict(
                    relative,
                    relative,
                    "concurrent local update while job pending",
                    Some(&conflicted_rel),
                    false,
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
                crate::sharing::notify_conflict(relative);
                Ok(1)
            } else {
                state
                    .db
                    .enqueue_job("upload", Some(relative), Some(relative))?;
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
    // DBSYNC-62: within the sync-root (re)registration grace window, a locally-vanished
    // tracked file is almost certainly a placeholder the shell EVICTED (Unregister), not
    // a user delete — never propagate it. Keep the index row; the indexer re-materializes
    // the placeholder. (Zero effect in normal operation, which never re-registers.)
    #[cfg(windows)]
    if crate::cloud_filter::in_post_registration_grace() {
        tracing::warn!(rel = %prev_rel, "remote delete suppressed: sync-root registration grace (placeholder eviction, not a user delete)");
        return Ok(0);
    }
    if placeholder_exists(tracked_root, prev_rel) {
        state.db.remove_local_file(prev_rel)?;
        return Ok(0);
    }
    // DBSYNC-65 (Slice 1): capture the remote rev at enqueue time so a later drain
    // (Slice 2) can detect whether the remote copy changed since this delete was
    // observed. Not yet read/enforced — plumbing only.
    let parent_rev = state.db.get_remote_file(prev_rel)?.map(|r| r.rev);
    state
        .db
        .enqueue_delete_job(prev_rel, parent_rev.as_deref())?;
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
    // DBSYNC-62: suppress folder deletes during the registration grace window too — a
    // sync-root Unregister can strip a whole placeholder subtree at once.
    #[cfg(windows)]
    if crate::cloud_filter::in_post_registration_grace() {
        tracing::warn!(rel, "remote folder delete suppressed: sync-root registration grace (eviction, not a user delete)");
        return Ok(0);
    }
    if placeholder_exists(tracked_root, rel) {
        state.db.remove_known_folder(rel)?;
        return Ok(0);
    }
    // Folders are never keyed in `remote_file_index`, so there is no rev to capture.
    state.db.enqueue_delete_job(rel, None)?;
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

    // DBSYNC-99: a refusal only needs remembering while its source folder is still tracked —
    // that is the only state in which the correlator could propose the pair again. Once the
    // delete-plus-upload fallback has converged the entry would just make a genuine future
    // rename of that same pair fall back for nothing.
    if let Err(e) = state.db.prune_stale_refused_moves() {
        tracing::warn!(error = %e, "could not prune stale refused moves");
    }

    let known = state.db.list_local_files()?;
    let known_map: HashMap<String, FileIndexRow> = known
        .iter()
        .map(|f| (f.relative_path.clone(), f.clone()))
        .collect();
    // DBSYNC-31: indexed active-job query instead of scanning list_recent_jobs(200)
    // (which silently missed pending jobs once the table grew past the limit).
    let mut pending_targets: HashSet<String> = state.db.active_job_paths()?;
    // Deletions ask a narrower question than correlation does — see `active_move_paths`.
    let pending_moves: HashSet<String> = state.db.active_move_paths()?;

    // Normalize + dedupe the incoming paths, dropping placeholders/ignored ones.
    let mut rels: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for p in paths {
        // Separator fold is Windows-only (DBSYNC-104): on macOS `\\` is a filename byte,
        // and folding it here aliased two distinct files onto one key.
        #[cfg(windows)]
        let rel = p.replace('\\', "/");
        #[cfg(not(windows))]
        let rel = p.to_string();
        let rel = rel.trim_start_matches('/').to_string();
        if rel.is_empty() || rel.ends_with(".cloudsc") || is_ignored_local_path(&rel) {
            continue;
        }
        if seen.insert(rel.clone()) {
            rels.push(rel);
        }
    }

    let mut enqueued = 0usize;

    // DBSYNC-99, pass one: classify every path in the batch before acting on any of it.
    //
    // This used to act inside the loop, which made a rename impossible to see by
    // construction: the vanished path's deletion was enqueued before the loop had reached
    // the path that appeared. Correlation can only happen across a whole batch, and a
    // whole batch is exactly what the debouncer delivers.
    let mut present_files: Vec<(String, PathBuf)> = Vec::new();
    let mut present_dirs: Vec<(String, PathBuf)> = Vec::new();
    let mut vanished: Vec<String> = Vec::new();

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
            PathKind::File => present_files.push((rel, absolute)),
            PathKind::Dir => present_dirs.push((rel, absolute)),
            // Symlink or other special file — not something we sync; skip.
            PathKind::Other => continue,
            PathKind::Absent => {
                // Confirm the absence is stable before propagating any remote
                // delete, so a delete-then-recreate atomic save is treated as a
                // modification (its recreate event/re-stat wins) rather than a
                // momentary remote delete + re-add.
                std::thread::sleep(std::time::Duration::from_millis(DELETE_CONFIRM_MS));
                if !matches!(classify_path(&absolute), Ok(PathKind::Absent)) {
                    continue;
                }
                vanished.push(rel);
            }
        }
    }

    // Pass two: pair what vanished with what appeared. Everything left unpaired falls
    // through to the behaviour that existed before this ticket.
    //
    // Directories are correlated FIRST, and by path arithmetic rather than by content, so a
    // renamed folder is recognised before any of its contents is hashed. Hashing a thousand
    // children to discover they moved would cost what the re-upload being avoided costs.
    let dir_moves =
        correlate_directory_renames(state, &known, &vanished, &present_dirs, &pending_targets)?;
    let dir_moved_from: Vec<String> = dir_moves.iter().map(|(old, _)| old.clone()).collect();
    let dir_moved_to: Vec<String> = dir_moves.iter().map(|(_, new)| new.clone()).collect();
    // A path is spoken for if it IS a moved directory or sits underneath one.
    let under_moved_dir = |path: &str, dirs: &[String]| -> bool {
        dirs.iter()
            .any(|d| path == d || path.starts_with(&format!("{d}/")))
    };

    let mut moves = correlate_renames(
        state,
        &known_map,
        &vanished,
        &present_files,
        &pending_targets,
    )?;
    // A child of a folder that is itself moving travels with it. Enqueueing both is not
    // merely redundant: job order is `ORDER BY id ASC`, so if the folder move takes one
    // transient failure and backs off into `retry_wait`, the child move becomes due first
    // and creates the destination — after which the folder move retries into `to/conflict`
    // and is dropped, leaving the old folder on Dropbox with no index row pointing at it.
    // The upload and delete passes were already filtered this way; the move pass was not.
    moves.retain(|(old, new)| {
        !under_moved_dir(old, &dir_moved_from) && !under_moved_dir(new, &dir_moved_to)
    });
    let moved_from: HashSet<&str> = moves.iter().map(|(old, _)| old.as_str()).collect();
    let moved_to: HashSet<&str> = moves.iter().map(|(_, new)| new.as_str()).collect();

    // Pass three: act.
    //
    // **Nothing but the job is written here.** The index is rewritten by the move job itself,
    // and only after Dropbox confirms — see `move_remote_file_internal`. Writing it down at
    // enqueue time meant every failure path had to undo it, and three rounds of review were
    // spent on repairs that each turned out worse than the defect they fixed.
    //
    // Both paths of a correlated pair are already in `pending_targets` by virtue of the job
    // naming them as source and target, which is what keeps the next scan from undoing the
    // intent while the job waits.
    for (old, new) in dir_moves.iter().chain(moves.iter()) {
        state.db.enqueue_job("move", Some(old), Some(new))?;
        pending_targets.insert(old.clone());
        pending_targets.insert(new.clone());
        enqueued += 1;
        tracing::info!(from = %old, to = %new, "rename detected: enqueued a move instead of a delete plus a full upload");
    }

    for (rel, absolute) in &present_files {
        if moved_to.contains(rel.as_str()) {
            continue; // already accounted for as the destination of a move
        }
        if under_moved_dir(rel, &dir_moved_to) {
            continue; // it travelled with its directory
        }
        enqueued += process_local_file_change(
            state,
            &tracked_root,
            rel,
            absolute,
            known_map.get(rel),
            &mut pending_targets,
            &pending_moves,
        )?;
    }

    for (rel, absolute) in &present_dirs {
        if under_moved_dir(rel, &dir_moved_to) {
            continue; // the subtree rewrite already put it where it belongs
        }
        // A moved-in / newly-created directory: record it and enqueue any
        // pre-existing children (a bounded walk of just this subtree, not
        // the whole root). Recursive watching also emits child events.
        // A path that is now a directory cannot also be a tracked file. Without this, a file
        // replaced on disk by a directory of the same name kept its `local_file_index` row
        // until the next full scan — up to five minutes, and suppressible by the mass-deletion
        // breaker — and anything asking "is this a file?" got the stale answer yes. That is
        // what let a real folder move take the file branch in `rederive_refused_move`.
        //
        // **The row is converted into the deletion it implies, not merely dropped.** Dropping
        // it was worse than leaving it: the full scan used to turn that stale row into a
        // remote `delete` of the old file, which is what FREES the path — and Dropbox cannot
        // hold a file and a folder at the same name, so without it every child upload into
        // the new folder is rejected, five times, then permanently. Removing the row removed
        // the only pass that unblocked the subtree.
        //
        // `process_local_file_deletion` is the right vehicle: it honours the dehydration and
        // registration-grace guards and captures `delete_parent_rev`, none of which a bare
        // `remove_local_file` does.
        // Gated on the row actually existing: this loop runs for EVERY present directory, and
        // `process_local_file_deletion` does not require a local row — calling it unguarded
        // enqueued a remote `delete` for every directory in the batch, including ones that had
        // never been tracked files. Caught by `a_refused_folder_move_falls_back_instead_of_
        // looping`, which saw a spurious delete of the rename destination.
        if state.db.get_local_file(rel)?.is_some() {
            enqueued += process_local_file_deletion(state, &tracked_root, rel)?;
        }
        state.db.upsert_known_folder(rel)?;
        for entry in WalkDir::new(absolute).into_iter().flatten() {
            if !entry.file_type().is_file() {
                continue;
            }
            // Skip-on-failure, not `?`: one unreadable entry must not abort the walk.
            let child_rel = match relpath_under(&tracked_root, entry.path()) {
                Ok(r) => r,
                Err(_) => continue,
            };
            if child_rel.ends_with(".cloudsc") || is_ignored_local_path(&child_rel) {
                continue;
            }
            if moved_to.contains(child_rel.as_str()) || under_moved_dir(&child_rel, &dir_moved_to) {
                continue;
            }
            enqueued += process_local_file_change(
                state,
                &tracked_root,
                &child_rel,
                entry.path(),
                known_map.get(&child_rel),
                &mut pending_targets,
                &pending_moves,
            )?;
        }
    }

    // The deletion pass runs last, and against a view of the index with the moved paths
    // removed. `known` was snapshotted before pass three rewrote rows, so a file moved OUT
    // of a directory that vanished in the same batch would otherwise still match that
    // directory's prefix here — and be deleted remotely moments after being moved there.
    let known_after_moves: Vec<FileIndexRow> = if moved_from.is_empty() && dir_moved_from.is_empty()
    {
        known
    } else {
        known
            .into_iter()
            .filter(|row| !moved_from.contains(row.relative_path.as_str()))
            .filter(|row| !under_moved_dir(&row.relative_path, &dir_moved_from))
            .collect()
    };
    for rel in &vanished {
        if moved_from.contains(rel.as_str()) || under_moved_dir(rel, &dir_moved_from) {
            continue; // it did not vanish, it moved
        }
        enqueued += enqueue_targeted_deletions(
            state,
            &tracked_root,
            rel,
            &known_after_moves,
            &pending_moves,
        )?;
    }

    Ok(enqueued)
}

/// Pair a vanished directory with an appeared directory when it is the same directory under
/// a new name (DBSYNC-99 slice 4).
///
/// **Detected by path arithmetic, not by hashing.** Every tracked descendant of the old
/// directory must turn up at the corresponding path under the new one — a `stat` per
/// descendant. That is what makes renaming a thousand-file folder cheap: hashing the
/// contents to discover they moved would cost as much as the re-upload being avoided.
///
/// The match must be **complete**. A directory whose contents only partly turn up is not a
/// rename that can be collapsed: falling back to per-file handling costs bandwidth, whereas
/// guessing costs data. An empty tracked directory is likewise not collapsed — there would
/// be nothing to distinguish a rename from an unrelated folder appearing in the same batch,
/// and a move buys nothing when there are no contents to carry.
fn correlate_directory_renames(
    state: &AppState,
    known: &[FileIndexRow],
    vanished: &[String],
    present_dirs: &[(String, PathBuf)],
    pending_targets: &HashSet<String>,
) -> AppResult<Vec<(String, String)>> {
    if vanished.is_empty() || present_dirs.is_empty() {
        return Ok(Vec::new());
    }
    let known_folders = state.db.list_known_folders()?;
    // DBSYNC-99: pairs Dropbox has already permanently refused. Proposing one again is not
    // merely wasteful — the pair suppresses the delete-plus-upload fallback below it, so a
    // refused folder rename that keeps being re-proposed never reaches Dropbox at all.
    let refused = state.db.list_refused_moves()?;

    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut claimed_new: HashSet<&str> = HashSet::new();

    for old in vanished {
        if !known_folders.iter().any(|f| f == old) {
            // `trace!`, not `debug!`: `vanished` is mostly files, so this fires on the
            // ordinary path and would drown the refusals that matter (DBSYNC-106).
            tracing::trace!(rel = %old, "not correlating a directory rename: not a tracked folder");
            continue;
        }
        let prefix = format!("{old}/");
        let descendants: Vec<(&str, i64, &str)> = known
            .iter()
            .filter(|row| row.relative_path.starts_with(&prefix))
            .filter(|row| {
                !row.relative_path.ends_with(".cloudsc")
                    && !is_ignored_local_path(&row.relative_path)
            })
            .map(|row| {
                (
                    row.relative_path.as_str(),
                    row.size_bytes,
                    row.hash.as_str(),
                )
            })
            .collect();
        if descendants.is_empty() {
            tracing::debug!(rel = %old, "not correlating a directory rename: no tracked descendants");
            continue;
        }

        // ---- Everything the FILE correlator refuses, refused here too. ----
        //
        // Four rounds of review found the same defect four times: a condition added to
        // `correlate_renames` and not to this function. The two are now deliberately
        // symmetrical, and anything added to one belongs in the other.

        // A row marked for rescan carries no content to compare against.
        if let Some((child, _, _)) = descendants
            .iter()
            .find(|(_, _, hash)| *hash == crate::storage::db::Db::HASH_NEEDS_RESCAN)
        {
            tracing::debug!(
                rel = %old, child = %child,
                "not correlating a directory rename: a descendant is marked for rescan"
            );
            continue;
        }
        // Queued work already names these paths, and a move reorders the world underneath
        // it. Without this, an upload of a child queued before the rename gets retargeted to
        // the new path, drains FIRST (jobs go by id), creates the destination on Dropbox, and
        // the folder move then fails `to/conflict` — which used to destroy the source.
        // Only the descendants are checked, and that is not an oversight. Testing `old`
        // itself as well was dead code: the correlator refuses a directory with no tracked
        // descendants, so a covered `old` always has at least one covered descendant and the
        // second clause fires first. It survived mutation for that reason — removed rather
        // than given a test it cannot fail.
        if descendants
            .iter()
            .any(|(child, _, _)| covered_by_active_job(child, pending_targets))
        {
            tracing::debug!(rel = %old, "not correlating a directory rename: queued work names it");
            continue;
        }

        for (new, new_absolute) in present_dirs {
            if claimed_new.contains(new.as_str()) || new == old {
                continue;
            }
            // A rename destination cannot be a folder we were ALREADY tracking — that is a
            // different folder that happens to contain files with matching names. This is
            // the direct analogue of the `known_map.contains_key(rel)` check the file
            // correlator has, and its absence let `d` and `e` — two real folders, each with
            // its own `README.md` — be paired as a rename: `d`'s deletion was suppressed so
            // it was never deleted on Dropbox, a bogus move was enqueued, and `d`'s rows
            // were orphaned in the index. Single-child folders are the common case.
            if known_folders.iter().any(|f| f == new) {
                tracing::debug!(
                    from = %old, to = %new,
                    "not correlating a directory rename: the destination is already a known folder"
                );
                continue;
            }
            // Dropbox has already refused exactly this pair, permanently. Proposing it again
            // costs a live `move_v2` per scan tick AND suppresses the ordinary
            // delete-plus-upload that would actually carry the rename across, because
            // `under_moved_dir` skips everything beneath a paired destination. Stepping
            // aside here is what lets the fallback run.
            if refused.contains(&(old.clone(), new.clone())) {
                tracing::debug!(from = %old, to = %new, "not correlating a directory rename: Dropbox already refused this move");
                continue;
            }
            // Every tracked descendant must turn up under the new name AND be the same
            // size. Matching on names alone paired unrelated folders. Size is already in
            // `FileIndexRow` and each child is being stat-ed here anyway, so this is the
            // same I/O — it just stops throwing away half of what the stat returned.
            // Hashing would be stronger and is still deliberately avoided: it would cost
            // what the re-upload being avoided costs.
            // `find` rather than `all`, so the refusal can name the offending child: one
            // child out of N refuses the whole rename, and which one is the whole question
            // when reading this back from a log (DBSYNC-106).
            let missing = descendants.iter().find(|(child, size_bytes, _)| {
                let tail = &child[prefix.len()..];
                match std::fs::symlink_metadata(new_absolute.join(tail)) {
                    Ok(m) => !(m.is_file() && m.len() == *size_bytes as u64),
                    Err(_) => true,
                }
            });
            if let Some((child, size_bytes, _)) = missing {
                tracing::debug!(
                    from = %old, to = %new, child = %child, expected_size = size_bytes,
                    "not correlating a directory rename: a descendant is missing under the destination or changed size"
                );
                continue;
            }
            // Dropbox must hold THESE BYTES for every descendant — content agreement, not
            // mere existence. Checking existence here while the file correlator checked
            // content was the same data-loss defect (C3) left half-fixed: Dropbox would
            // relocate the OLD contents of an edited file and nothing would ever notice,
            // because no code path compares `local_file_index.hash` against
            // `remote_file_index.content_hash`. The size guard above does not save it — an
            // in-place edit of the same length is exactly the shape that slips through.
            //
            // The destination must also be free in the remote index, for the same reason the
            // file correlator checks it: a path Dropbox holds but that was never downloaded
            // has a remote row and no local row, and the rewrite would collide with it.
            let mut unusable: Option<(&str, &str)> = None;
            for (child, _, hash) in &descendants {
                let tail = &child[prefix.len()..];
                let destination = format!("{new}/{tail}");
                let holds_our_bytes = matches!(
                    state.db.get_remote_file(child)?,
                    Some(remote) if remote.content_hash == *hash
                );
                if !holds_our_bytes {
                    unusable = Some((child, "Dropbox does not hold this child's current bytes"));
                    break;
                }
                if state.db.get_remote_file(&destination)?.is_some() {
                    unusable = Some((child, "the destination path already has a remote row"));
                    break;
                }
            }
            if let Some((child, why)) = unusable {
                tracing::debug!(
                    from = %old, to = %new, child = %child, reason = why,
                    "not correlating a directory rename: a descendant is not relocatable"
                );
                continue;
            }
            claimed_new.insert(new.as_str());
            pairs.push((old.clone(), new.clone()));
            break;
        }
    }
    Ok(pairs)
}

/// Pair paths that vanished with paths that appeared, when they are the same item under a
/// new name (DBSYNC-99).
///
/// **Correlation is by content hash, not by Dropbox's identifier.** The path that appeared
/// is a local file that has never been uploaded under that name, so it has no Dropbox id
/// to match on. What *is* available is the hash the index already recorded for the
/// vanished path and the hash of the file now on disk — and equality of those two is what
/// a rename means. The identifier is what makes the remote operation a move.
///
/// Ambiguity is not a hazard. If several vanished paths share a hash with several appeared
/// paths then the files are byte-identical, so every pairing produces the same final remote
/// state; pairs are simply taken in order.
///
/// Directories are out of scope here, deliberately. A renamed directory's *children* are
/// not in the batch — the watcher reports the directory, not its contents — so a directory
/// rename still falls through to the old delete-and-re-upload path. Collapsing that into a
/// single move is DBSYNC-99 slice 4.
fn correlate_renames(
    state: &AppState,
    known_map: &HashMap<String, FileIndexRow>,
    vanished: &[String],
    present_files: &[(String, PathBuf)],
    pending_targets: &HashSet<String>,
) -> AppResult<Vec<(String, String)>> {
    // A batch with nothing gone, or nothing new, cannot contain a rename — and this is the
    // overwhelmingly common case, so it costs nothing to leave it untouched. In particular
    // no file is hashed here that was not going to be hashed anyway.
    if vanished.is_empty() || present_files.is_empty() {
        return Ok(Vec::new());
    }
    // Pairs Dropbox has already permanently refused — read here for the same reason
    // `correlate_directory_renames` reads it: a re-proposed pair suppresses the fallback that
    // would actually carry the rename across.
    let refused = state.db.list_refused_moves()?;

    let mut by_hash: HashMap<&str, Vec<&str>> = HashMap::new();
    for rel in vanished {
        let Some(row) = known_map.get(rel) else {
            continue; // never tracked: nothing to move
        };
        // A row marked for rescan (DBSYNC-56 stores an empty hash) carries no content to
        // match on. Matching every such row against each other would pair unrelated files.
        if row.hash == crate::storage::db::Db::HASH_NEEDS_RESCAN {
            continue;
        }
        // Dropbox must hold THESE BYTES, not merely a file at this path.
        //
        // Asking only whether a `remote_file_index` row exists was a data-loss defect, and
        // a regression against the behaviour this ticket replaced. `process_local_file_change`
        // writes the local row at enqueue time, so between an edit and a successful upload
        // the local hash runs ahead of the remote one; if that upload reaches `failed` it
        // leaves `active_job_paths` and the `pending_targets` guard below stops firing.
        // Dropbox would then move the OLD content to the new path, after which the local
        // scan sees disk == index and the remote sweep sees server == remote-index, so
        // nothing ever notices the edit was lost. The delete-plus-upload this replaced was
        // wasteful, but it preserved those bytes.
        match state.db.get_remote_file(rel)? {
            Some(remote) if remote.content_hash == row.hash => {}
            _ => continue,
        }
        // Queued work already names this path, and a move reorders the world underneath
        // it: `move_index_row` retargets that job to the new name, and if it drains before
        // the move does, Dropbox rejects the move for a destination that is now occupied
        // and the copy under the OLD name is left behind. Renaming is an optimisation;
        // correctness is not, so give up the optimisation whenever the two could race.
        if covered_by_active_job(rel, pending_targets) {
            tracing::debug!(
                rel,
                "not correlating a rename: the old path still has queued work"
            );
            continue;
        }
        by_hash.entry(row.hash.as_str()).or_default().push(rel);
    }
    if by_hash.is_empty() {
        return Ok(Vec::new());
    }

    let mut pairs: Vec<(String, String)> = Vec::new();
    for (rel, absolute) in present_files {
        if known_map.contains_key(rel) {
            continue; // already tracked under this very name, so it did not move here
        }
        // The destination must be free in BOTH indexes. Checking only the local one let a
        // path that Dropbox holds but which has not been downloaded yet — a remote row with
        // no local row — through to `move_index_row`, whose bare UPDATE then hit a UNIQUE
        // violation on `remote_file_index`. The transaction rolled back correctly, but the
        // error propagated out of `process_changed_paths` and discarded EVERY other path in
        // the batch: deletes, uploads, folder discovery. Refusing the pair here keeps the
        // bare UPDATE free to fail loudly on a genuine surprise.
        if state.db.get_remote_file(rel)?.is_some() {
            continue;
        }
        // Never hash a Windows dehydrated placeholder — opening it triggers a download
        // (DBSYNC-59). It is cloud-only content, not a rename destination.
        if crate::path_util::is_dehydrated_placeholder(absolute) {
            continue;
        }
        let Ok((hash, _, _)) = hash_file(absolute) else {
            continue; // unreadable right now; the next event or scan reconciles it
        };
        let Some(candidates) = by_hash.get_mut(hash.as_str()) else {
            continue;
        };
        // Pick the last candidate Dropbox has NOT already refused for this destination.
        //
        // Re-proposing a refused pair costs a live `move_v2` per scan tick and — worse — the
        // pair suppresses the ordinary delete-plus-upload that would carry the rename across,
        // because `moved_from` and `moved_to` skip both halves in the passes below.
        //
        // Searching rather than inspecting only the last one. An earlier version peeked at
        // `candidates.last()` and gave up on the whole destination if that one was refused,
        // so with two sources sharing a content hash and only one pair ever refused, the
        // OTHER — a perfectly good pair nobody has refused — was abandoned and both sources
        // were deleted and the destination re-uploaded in full. `correlate_directory_renames`
        // keeps looking, and the two correlators are deliberately symmetrical.
        //
        // `remove(idx)` rather than `pop`: the candidate is consumed only when it pairs, so a
        // source refused for THIS destination stays available for a different one.
        let Some(idx) = candidates
            .iter()
            .rposition(|old| !refused.contains(&((*old).to_string(), rel.clone())))
        else {
            tracing::debug!(to = %rel, "not correlating a rename: every candidate source has been refused for this destination");
            continue;
        };
        let old = candidates.remove(idx);
        pairs.push((old.to_string(), rel.clone()));
    }
    Ok(pairs)
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
    pending_moves: &HashSet<String>,
) -> AppResult<usize> {
    let mut enqueued = 0usize;

    // A path a queued MOVE names is not a deletion, however absent it looks.
    //
    // Only a move. An earlier version held a deletion back for any job in flight, and the
    // excess did not defer deletions — it lost them. The materialization sweep plants a
    // `.cloudsc` sidecar for any remote child whose local counterpart is absent, consulting
    // no index; `process_local_file_deletion` then drops a delete whose path has a
    // placeholder, and removes the index row that remembers it. The file stayed on Dropbox
    // forever and no scan ever asked again. Reproduced from an ordinary sequence: edit a
    // file, rename it before the upload drains.
    //
    // This function was the one place with NO protection at all. The deletion pass filters
    // against the moves correlated in THIS batch, so a move queued by an earlier batch was
    // invisible here — and the watcher will happily report the old path absent in a later
    // batch. The result was a recursive `delete_v2` of the folder the user had just renamed,
    // propagated to every other device, plus the loss of every descendant's identity.
    if covered_by_active_job(rel, pending_moves) {
        tracing::debug!(
            rel,
            "not a deletion: a queued move is about to relocate this path"
        );
        return Ok(0);
    }

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
            && !is_ignored_local_path(&prev.relative_path)
            && !covered_by_active_job(&prev.relative_path, pending_moves)
        {
            enqueued += process_local_file_deletion(state, tracked_root, &prev.relative_path)?;
        }
    }

    // Matching known-folder rows (the removed dir itself + any sub-folders).
    for folder_rel in state.db.list_known_folders()? {
        if covered_by_active_job(&folder_rel, pending_moves) {
            continue;
        }
        if folder_rel == rel || folder_rel.starts_with(&prefix) {
            enqueued += process_known_folder_deletion(state, tracked_root, &folder_rel)?;
        }
    }

    Ok(enqueued)
}

/// Mass-deletion circuit breaker thresholds (DBSYNC-64). A single scan pass that
/// would propagate at least `MASS_DELETE_ABSOLUTE` deletions, OR at least
/// `MASS_DELETE_FRACTION_PERCENT`% of the tracked set (guarded by a small floor so a
/// tiny repo isn't over-eager), is treated as a likely bug / CfAPI eviction / drive
/// hiccup rather than an intentional bulk delete — and is BLOCKED pending an explicit
/// user override. No sync client should ever nuke hundreds of files (local OR remote)
/// from a single reconcile without a sanity check.
const MASS_DELETE_ABSOLUTE: usize = 200;
const MASS_DELETE_FRACTION_FLOOR: usize = 25;
const MASS_DELETE_FRACTION_PERCENT: usize = 10;

/// One-shot app_config flag the user sets (via `confirm_pending_deletions`) to let
/// the next mass deletion through. Intentionally SHARED across both directions
/// (known trade-off: confirming one also authorizes the other's next pass) rather
/// than split per-direction — the local scan and remote sweep run back-to-back in
/// the same tick, and splitting this one would just mean confirming twice.
const MASS_DELETE_OVERRIDE_KEY: &str = "mass_delete_override_once";

/// Durable app_config flag holding the "sync paused: mass deletion blocked" message
/// for a LOCAL-SCAN-tripped breaker (DBSYNC-64: `scan_local_changes_internal`).
/// Persisted (not a transient engine field) so `refresh_queue_depth_internal` keeps
/// surfacing it every tick until the block clears. Empty string = not paused.
///
/// Split from the remote-sweep key (CTO fix) because the local scan and the remote
/// sweep both run within one `scan_local_changes_internal` call — a single shared
/// key meant a benign remote sweep's `clear_mass_delete_blocked` would silently
/// erase the local scan's still-active pause message from moments earlier.
const MASS_DELETE_BLOCKED_SCAN_KEY: &str = "mass_delete_blocked_scan";

/// Same as `MASS_DELETE_BLOCKED_SCAN_KEY`, but for the REMOTE-SWEEP direction:
/// `remote_index.rs`'s full sweep (`refresh_remote_index_and_enqueue_downloads_internal`)
/// and `seed_remote_delta_cursor` (reached via the cursor-reset path with a full
/// local index still intact).
const MASS_DELETE_BLOCKED_REMOTE_KEY: &str = "mass_delete_blocked_remote";

/// Which direction tripped the mass-deletion circuit breaker (DBSYNC-64), so
/// `block_mass_deletion`/`clear_mass_delete_blocked` write/clear the right durable
/// flag instead of a single shared one both directions could clobber.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MassDeleteSource {
    /// `scan_local_changes_internal`'s local→remote deletion detection.
    LocalScan,
    /// `remote_index.rs`'s remote→local sweep (full snapshot + cursor reseed).
    RemoteSweep,
}

impl MassDeleteSource {
    fn blocked_key(self) -> &'static str {
        match self {
            MassDeleteSource::LocalScan => MASS_DELETE_BLOCKED_SCAN_KEY,
            MassDeleteSource::RemoteSweep => MASS_DELETE_BLOCKED_REMOTE_KEY,
        }
    }

    fn label(self) -> &'static str {
        match self {
            MassDeleteSource::LocalScan => "local scan",
            MassDeleteSource::RemoteSweep => "remote sweep",
        }
    }
}

/// True if this many deletion candidates out of `tracked` tracked entries looks like
/// a catastrophe rather than an intentional bulk delete.
pub(crate) fn is_mass_deletion(candidates: usize, tracked: usize) -> bool {
    if candidates >= MASS_DELETE_ABSOLUTE {
        return true;
    }
    candidates >= MASS_DELETE_FRACTION_FLOOR
        && candidates.saturating_mul(100) >= tracked.saturating_mul(MASS_DELETE_FRACTION_PERCENT)
}

/// Returns true (and CONSUMES the one-shot flag) if the user has explicitly
/// authorized the next mass deletion to proceed.
///
/// `pub(crate)`: shared with the remote sweep (`remote_index.rs`, DBSYNC-64
/// remote→local extension) so both directions consume the same one-shot flag
/// instead of each keeping their own.
pub(crate) fn consume_mass_delete_override(state: &AppState) -> AppResult<bool> {
    if state
        .db
        .get_app_config(MASS_DELETE_OVERRIDE_KEY)?
        .as_deref()
        == Some("1")
    {
        state.db.set_app_config(MASS_DELETE_OVERRIDE_KEY, "0")?;
        tracing::warn!("mass-deletion override consumed: allowing this deletion batch");
        return Ok(true);
    }
    Ok(false)
}

/// Block a suspicious mass deletion: propagate NONE of it, log loudly, notify, and
/// persist a DURABLE "sync paused" flag so the UI keeps showing it every tick until
/// the block clears (DBSYNC-64).
///
/// `pub(crate)`: shared with the remote sweep (`remote_index.rs`) — `source`
/// selects which direction's durable pause flag gets written so the two
/// directions can't clobber each other's message.
pub(crate) fn block_mass_deletion(
    state: &AppState,
    candidates: usize,
    tracked: usize,
    source: MassDeleteSource,
) {
    let msg = format!(
        "Sync paused: {candidates} deletions in one pass ({tracked} tracked, {}) look like a \
         bug or missing files, not an intentional delete — nothing was deleted. Review, then \
         confirm to proceed.",
        source.label()
    );
    tracing::error!(
        candidates,
        tracked,
        source = source.label(),
        "MASS-DELETION BLOCKED by circuit breaker; no deletions propagated (DBSYNC-64)"
    );
    // Durable so `refresh_queue_depth_internal` re-surfaces it instead of clearing it.
    if let Err(e) = state.db.set_app_config(source.blocked_key(), &msg) {
        tracing::error!(error = %e, "failed to persist mass-deletion pause flag");
    }
    if let Ok(mut engine) = state.sync_engine.lock() {
        engine.set_last_error(msg.clone());
    }
    crate::sharing::notify("DropboxSync - sync paused", &msg);
}

/// Clear the durable mass-deletion pause flag for `source` — that direction's
/// situation resolved (no mass deletion this pass, or the user overrode it), so
/// sync is no longer paused ON THAT ACCOUNT. The other direction's flag (if set)
/// is left untouched — it's the other direction's job to clear its own.
///
/// `pub(crate)`: shared with the remote sweep (`remote_index.rs`).
pub(crate) fn clear_mass_delete_blocked(state: &AppState, source: MassDeleteSource) {
    let key = source.blocked_key();
    if state
        .db
        .get_app_config(key)
        .ok()
        .flatten()
        .is_some_and(|s| !s.is_empty())
    {
        let _ = state.db.set_app_config(key, "");
    }
}

/// True while EITHER direction's durable mass-deletion pause flag is set
/// (DBSYNC-64), i.e. sync is currently paused pending `confirm_pending_deletions`.
/// Shared by `refresh_queue_depth_internal`'s message surfacing and by
/// `get_sync_dashboard`'s `massDeletePaused` flag, so the frontend can show a
/// "review & confirm deletions" button without re-deriving the two key names.
pub(crate) fn mass_delete_pause_active(state: &AppState) -> AppResult<bool> {
    let scan_paused = state
        .db
        .get_app_config(MASS_DELETE_BLOCKED_SCAN_KEY)?
        .is_some_and(|s| !s.is_empty());
    let remote_paused = state
        .db
        .get_app_config(MASS_DELETE_BLOCKED_REMOTE_KEY)?
        .is_some_and(|s| !s.is_empty());
    Ok(scan_paused || remote_paused)
}

/// The LOCAL half of a scan: walk the sync folder, decide what changed, and enqueue
/// uploads and deletions. Returns how many jobs it enqueued.
///
/// Split out of [`scan_local_changes_internal`] because that function does two jobs and
/// only the second needs credentials (DBSYNC-81). Keeping them apart lets the local
/// decisions be exercised without an access token — which is what a test of the
/// mass-deletion guard actually wants.
fn scan_local_changes_only(state: &AppState) -> AppResult<usize> {
    let folder = state
        .db
        .get_sync_folder()?
        .ok_or_else(|| AppError::Sync("sync folder not configured".to_string()))?;
    let known = state.db.list_local_files()?;
    // DBSYNC-31: indexed active-job query instead of scanning list_recent_jobs(200).
    let pending_targets: HashSet<String> = state.db.active_job_paths()?;
    let pending_moves: HashSet<String> = state.db.active_move_paths()?;

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
        let relative = relpath_under(&tracked_root, &absolute)?;

        if relative.ends_with(".cloudsc") {
            continue;
        }
        if is_ignored_local_path(&relative) {
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
            &pending_moves,
        )?;
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
        let relative = relpath_under(&tracked_root, &absolute)?;

        if relative.is_empty() {
            continue; // skip the sync root itself
        }
        if is_ignored_local_path(&relative) {
            continue;
        }

        seen_dirs.insert(relative.clone());
        state.db.upsert_known_folder(&relative)?;
    }

    // ── Deletion propagation + mass-deletion circuit breaker (DBSYNC-64) ────────
    // Only propagate deletions when BOTH walks were fully readable — a partial walk
    // must never be treated as a batch of deletions. Collect the FILE and FOLDER
    // deletion candidates FIRST (don't enqueue yet); if a single pass would delete a
    // suspicious number of them, a bug / CfAPI eviction / drive hiccup likely made
    // real files look absent — block the whole batch (nothing enqueued), alert, and
    // require an explicit user override, instead of nuking the user's Dropbox.
    //
    // Ignored/temp-named folders are excluded: they're skipped by the walk (never in
    // `seen_dirs`), and a stale row from an older build would otherwise trigger a
    // RECURSIVE remote delete of a folder that still exists (DBSYNC-55).
    if !walk_had_error {
        // DBSYNC-99: a path a queued move names is NOT a deletion, however absent it looks.
        //
        // The index is not rewritten until Dropbox confirms the move, so between enqueue and
        // drain the old path is legitimately missing from disk. Without this filter a scan
        // landing in that window would propagate a remote delete of the very file the move is
        // about to relocate — and `delete_v2` on a folder is recursive.
        let pending = state.db.active_move_paths()?;
        let file_deletions: Vec<String> = known
            .iter()
            .map(|f| f.relative_path.clone())
            .filter(|rel| {
                !rel.ends_with(".cloudsc")
                    && !is_ignored_local_path(rel)
                    && !seen_paths.contains(rel)
                    && !covered_by_active_job(rel, &pending)
            })
            .collect();
        let folder_deletions: Vec<String> = state
            .db
            .list_known_folders()?
            .into_iter()
            .filter(|rel| {
                !is_ignored_local_path(rel)
                    && !seen_dirs.contains(rel)
                    && !covered_by_active_job(rel, &pending)
            })
            .collect();

        let candidate_count = file_deletions.len() + folder_deletions.len();
        let tracked_count = known.len() + seen_dirs.len();

        // Consume the one-shot override whenever THIS pass actually has deletions, so
        // a stale confirmation can't linger and silently authorize an unrelated future
        // mass deletion (only read it when there's something to authorize).
        let overridden = candidate_count > 0 && consume_mass_delete_override(state)?;

        if is_mass_deletion(candidate_count, tracked_count) && !overridden {
            block_mass_deletion(
                state,
                candidate_count,
                tracked_count,
                MassDeleteSource::LocalScan,
            );
        } else {
            // Not a mass deletion (or the user confirmed) → sync is not paused.
            clear_mass_delete_blocked(state, MassDeleteSource::LocalScan);
            for rel in &file_deletions {
                enqueued_jobs += process_local_file_deletion(state, &tracked_root, rel)?;
            }
            for rel in &folder_deletions {
                enqueued_jobs += process_known_folder_deletion(state, &tracked_root, rel)?;
            }
        }
    }

    Ok(enqueued_jobs)
}

/// Feed the watcher's deferred paths through the correlating path, before the full scan.
///
/// **This is what makes deferral real rather than a promise** (DBSYNC-106). Re-delivering on
/// "the next batch" only helps if another filesystem event ever arrives; when someone renames
/// a folder and then walks away, none does. `scan_local_changes_only` cannot correlate — it
/// uploads the new prefix and propagates the old one as deletions, which is the defect — so
/// the deferred paths must go through `process_changed_paths` first, while the pairing is
/// still visible.
///
/// Extracted so it can be asserted against a real `Db`. `scan_local_changes_internal` needs an
/// access token for its remote half, so a test cannot reach this through it; leaving the drain
/// inline would have left the behaviour untestable, which is how DBSYNC-104 shipped a feature
/// that could be deleted from production with the suite still green.
///
/// Never fails the scan. A failure here loses the rename pairing, not the content: the full
/// scan still runs and still converges. The log says which happened.
fn drain_deferred_watcher_paths(state: &AppState) -> usize {
    let deferred = crate::fs_watcher::take_pending();
    if deferred.is_empty() {
        return 0;
    }
    tracing::info!(
        count = deferred.len(),
        "draining deferred watcher paths before the full scan"
    );
    match process_changed_paths(state, &deferred) {
        Ok(n) => n,
        Err(e) => {
            tracing::error!(
                error = %e,
                "draining deferred watcher paths failed; a rename may be missed"
            );
            0
        }
    }
}

/// A full scan: the local half, then the remote refresh, then the bookkeeping.
///
/// The remote half needs an access token, so this is the entry point for production and
/// NOT the one to reach for from a test that only cares about local decisions.
pub(crate) fn scan_local_changes_internal(state: &AppState) -> AppResult<usize> {
    // DBSYNC-106: drain anything the watcher had to defer because the gate was held, BEFORE
    // the uncorrelated full scan runs.
    //
    // This is what makes the deferral real rather than a promise. Re-delivery on "the next
    // batch" only helps if another filesystem event ever arrives; when the user renames a
    // folder and then walks away, none does. `scan_local_changes_only` below cannot
    // correlate — it would upload the new prefix and delete the old, which is the defect —
    // so the deferred paths must go through `process_changed_paths` first, while the pairing
    // is still visible.
    let enqueued_jobs = drain_deferred_watcher_paths(state) + scan_local_changes_only(state)?;
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
            .and_then(|rel| {
                // Dehydration is never a Dropbox delete: if the path is now a cloud-only
                // placeholder, drop this job instead of deleting the remote file.
                if delete_suppressed_by_dehydration(state, rel) {
                    tracing::info!(rel, "remote delete suppressed: path is a cloud-only placeholder (dehydration, not a deletion)");
                    return Ok(());
                }
                delete_remote_file_internal(state, rel, job.delete_parent_rev.as_deref())?;
                // DBSYNC-66 folder-delete completeness: the delete genuinely
                // succeeded — walk up rel's ancestor chain pruning any empty
                // ancestor folder it just emptied out (locally + Dropbox +
                // known_folders). Best-effort: a prune failure must never fail
                // the delete job that already succeeded.
                if let Err(e) = crate::cloudsc_ops::prune_empty_deleted_ancestors(state, rel) {
                    tracing::warn!(rel, error = %e, "folder-delete completeness: ancestor prune failed (delete itself still succeeded)");
                }
                Ok(())
            }),
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
        // DBSYNC-99. The first job type that needs BOTH paths: every other arm reads one or
        // the other and falls back between them, which a move cannot do — `source_path` is
        // where the item was and `target_path` is where it is, and neither can stand in for
        // the other. Missing either is a programming error, not a transient failure.
        "move" => match (job.source_path.as_deref(), job.target_path.as_deref()) {
            (Some(from), Some(to)) => crate::dropbox_transfer::move_remote_file_internal(
                state, from, to,
            )
            .map(|()| {
                // `move_remote_file_internal` rewrites the index itself, and only once
                // Dropbox has confirmed — so there is nothing to do here on success, and
                // nothing to undo on failure. That order is the whole design; see its doc
                // comment for the six data-loss defects the opposite order produced.
            }),
            _ => Err(AppError::Sync(
                "move job missing source_path or target_path".to_string(),
            )),
        },
        "hydrate_cloudsc" => job
            .source_path
            .as_deref()
            .ok_or_else(|| AppError::Sync("hydrate_cloudsc job missing source_path".to_string()))
            .and_then(|rel| hydrate_cloudsc_placeholder_internal(state, rel).map(|_| ())),
        other => Err(AppError::Sync(format!("unknown job_type: {other}"))),
    };

    match op_result {
        Ok(()) => {
            // DBSYNC-99: the source deletion a refused move owes is enqueued HERE and nowhere
            // else. It exists because this upload put the bytes at the destination — not
            // because it was given a higher id at enqueue time, which `pick_next_due_job`
            // honours only among DUE jobs and therefore does not guarantee at all.
            //
            // `Ok(())` from an upload is NOT proof the bytes landed — see
            // `settle_owed_source_deletion`, which checks rather than assumes, and which
            // never fails the job.
            if job.job_type == "upload" {
                if let Some(destination) = job.source_path.as_deref() {
                    crate::dropbox_transfer::settle_owed_source_deletion(
                        state,
                        job.id,
                        destination,
                    );
                }
            }
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
            apply_job_failure(state, &job, attempt, max_attempts, &err)?;
        }
    }

    // `refresh_queue_depth_internal` reconciles the engine's global error/health
    // from the DB, so per-job success no longer masks still-failed jobs.
    refresh_queue_depth_internal(state)?;
    Ok(true)
}

/// Record a failed job: decide retry-or-give-up, write the row, log it.
///
/// **Extracted so the behaviour can be asserted against a real `Db`** (DBSYNC-104, review
/// round 2). An earlier attempt extracted only the *decision* into `classify_job_failure`
/// and tested that; the review then replaced the drain's call site with the old logic and
/// the whole suite stayed green — the user-visible behaviour could be deleted from
/// production without a single test noticing. Moving the DB write in here shrinks the
/// untested surface to one line in `process_sync_queue_internal`: `apply_job_failure(...)`.
fn apply_job_failure(
    state: &AppState,
    job: &crate::storage::db::SyncJobRow,
    attempt: i64,
    max_attempts: i64,
    err: &AppError,
) -> AppResult<()> {
    let job_path = job
        .source_path
        .as_deref()
        .or(job.target_path.as_deref())
        .unwrap_or("");

    // The path the message should name is the one Dropbox rejected. For a move that is the
    // DESTINATION — `job_path` prefers `source_path`, so renaming `x.txt` to `a\b.txt` would
    // otherwise tell the user that `x.txt` contains a backslash, which it does not.
    //
    // Currently reachable only as a log field: `move_v2` reports destination problems under
    // `to/…` and `from_lookup/…`, never `path/…`, and the permanent marker is anchored to
    // `path/malformed_path` — so a move cannot take the `UnrepresentablePath` arm until a
    // real `move_v2` rejection body is recorded and the marker widened (review round 2).
    let offending_path = job
        .target_path
        .as_deref()
        .filter(|_| job.job_type == "move")
        .unwrap_or(job_path);

    match classify_job_failure(err, attempt, max_attempts) {
        JobFailure::Permanent(reason) => {
            let msg = match reason {
                PermanentReason::UnrepresentablePath => {
                    unrepresentable_path_message(offending_path)
                }
                PermanentReason::AttemptsExhausted => format!("job {} failed: {err}", job.id),
            };
            tracing::error!(
                job_id = job.id,
                job_type = %job.job_type,
                path = %offending_path,
                attempt,
                error = %err,
                reason = reason.as_log_str(),
                "sync job failed permanently"
            );
            state.db.mark_job_failed(job.id, attempt, Some(&msg))?;
        }
        JobFailure::Retry => {
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
    Ok(())
}

/// Why a job is being failed rather than retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PermanentReason {
    /// Dropbox will never accept this path, so waiting cannot help (DBSYNC-104).
    UnrepresentablePath,
    /// An otherwise-retryable error that has used up its attempts.
    AttemptsExhausted,
}

impl PermanentReason {
    pub(crate) fn as_log_str(self) -> &'static str {
        match self {
            PermanentReason::UnrepresentablePath => "unrepresentable_path",
            PermanentReason::AttemptsExhausted => "attempts_exhausted",
        }
    }
}

/// What to do with a failed job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JobFailure {
    Permanent(PermanentReason),
    Retry,
}

/// The whole retry-or-give-up decision, as a pure function of the error and the attempt
/// count (DBSYNC-104).
///
/// Extracted from the drain deliberately. The behaviour it encodes — "a path Dropbox will
/// never accept is not worth five attempts" — shipped once with no test that reached it:
/// disabling the branch left the suite entirely green, because the only coverage was on
/// the classifier in isolation. A pure function can be asserted directly, so the decision
/// is constrained rather than merely commented.
pub(crate) fn classify_job_failure(err: &AppError, attempt: i64, max_attempts: i64) -> JobFailure {
    if err.is_unrepresentable_path() {
        // No backoff will make Dropbox accept this name. Retrying costs five attempts
        // and then reports a generic permanent error instead of a fixable one.
        return JobFailure::Permanent(PermanentReason::UnrepresentablePath);
    }
    if attempt >= max_attempts {
        return JobFailure::Permanent(PermanentReason::AttemptsExhausted);
    }
    JobFailure::Retry
}

/// The user-facing text for a path Dropbox refuses to store.
///
/// Worded from the `malformed_path` tag rather than from the one cause that was probed.
/// The tag also covers `< > : " | ? *`, a trailing space or period, and over-long
/// components — naming only the backslash would tell someone with a file called
/// `report?.txt` to remove a character that is not there.
pub(crate) fn unrepresentable_path_message(path: &str) -> String {
    format!(
        "\"{path}\" cannot sync: Dropbox will not accept this file name. \
         Rename it — names cannot contain \\ / : ? * \" < > |, or end with a space or a period."
    )
}

/// Whether `p` is **confirmed** absent, as opposed to merely unreadable (DBSYNC-56).
///
/// `Path::exists()` collapses every error into `false`, so a permission denial, a Windows
/// sharing violation, or an editor's atomic save caught mid-rename all read as "gone". This
/// is the same distinction `classify_path` and the upload path already make for exactly that
/// reason (DBSYNC-55): only a confirmed `NotFound` means gone.
///
/// The consequence here is milder than in the sync paths — the worst case is resolving a
/// failed job that should have stayed failed, at startup only — but there is no reason for
/// two answers to the same question to live in one codebase.
fn is_confirmed_absent(p: &Path) -> bool {
    matches!(
        std::fs::symlink_metadata(p),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound
    )
}

/// Startup cleanup (DBSYNC-55): forget editor-temp files a previous build tracked,
/// and resolve failed `upload` jobs whose source is a temp file or no longer
/// exists, so a phantom "Error" clears without a manual reset. Returns the number
/// of rows/jobs cleaned.
pub(crate) fn cleanup_stale_upload_state(state: &AppState) -> AppResult<usize> {
    let mut cleaned = 0usize;

    // 1) Drop tracked editor-temp index rows + known-folder rows — they should
    //    never have been tracked (and a stale temp-named folder row could trigger a
    //    recursive remote delete). Use the BUILT-IN ignore predicate (OS junk +
    //    editor temps), NOT the combined one: a user-defined ignore glob (DBSYNC-36)
    //    can match a real, already-synced file, and this cleanup must never strip
    //    such a file's index row (that would make it look "new" and re-download/
    //    churn). This also decouples the cleanup from user-glob load ordering.
    for row in state.db.list_local_files()? {
        if is_builtin_ignored_local_path(&row.relative_path) {
            state.db.remove_local_file(&row.relative_path)?;
            cleaned += 1;
        }
    }
    for folder_rel in state.db.list_known_folders()? {
        if is_builtin_ignored_local_path(&folder_rel) {
            state.db.remove_known_folder(&folder_rel)?;
            cleaned += 1;
        }
    }

    // 2) Resolve failed upload jobs whose source is a temp file or is gone on disk
    //    (a genuine failure with an existing, non-temp source is left alone to retry).
    //    Only trust "missing" when the sync root is actually mounted — otherwise an
    //    unmounted removable/network drive would look like "everything deleted" and
    //    we'd wrongly resolve real failures (whose optimistic index rows would then
    //    never re-detect the un-uploaded edit).
    let folder = state.db.get_sync_folder()?;
    let root_mounted = folder
        .as_deref()
        .map(|f| Path::new(f).is_dir())
        .unwrap_or(false);
    for job in state.db.list_recent_jobs(10_000)? {
        if job.job_type != "upload" || job.status != "failed" {
            continue;
        }
        let Some(src) = job.source_path.as_deref() else {
            continue;
        };
        let missing = root_mounted
            && folder
                .as_deref()
                .and_then(|f| safe_join(Path::new(f), src).ok())
                .map(|p| is_confirmed_absent(&p))
                .unwrap_or(false);
        if is_editor_temp_path(src) || missing {
            state.db.mark_job_completed(job.id)?;
            cleaned += 1;
        }
    }

    // 3) Resolve failed `hydrate_cloudsc` jobs whose `.cloudsc` placeholder (in
    //    `source_path`) no longer exists — the file was already hydrated, or (after
    //    the DBSYNC-59 transition) converted to a native CfAPI placeholder, so the
    //    old `.cloudsc` is gone and this job can only ever re-fail. Left un-resolved
    //    it keeps `latest_failed_error` set and pins the tray to Error (DBSYNC-32).
    //    Same `root_mounted` guard: never treat a placeholder as "gone" on an
    //    unmounted drive. No filesystem/remote mutation — only the job row changes.
    for job in state.db.list_recent_jobs(10_000)? {
        if job.job_type != "hydrate_cloudsc" || job.status != "failed" {
            continue;
        }
        let Some(src) = job.source_path.as_deref() else {
            continue;
        };
        let missing = root_mounted
            && folder
                .as_deref()
                .and_then(|f| safe_join(Path::new(f), src).ok())
                .map(|p| is_confirmed_absent(&p))
                .unwrap_or(false);
        if missing {
            state.db.mark_job_completed(job.id)?;
            cleaned += 1;
        }
    }
    Ok(cleaned)
}

/// User's choice for resolving a conflict (DBSYNC-35). Parsed from the IPC string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConflictAction {
    KeepLocal,
    UseRemote,
    KeepBoth,
}

impl ConflictAction {
    pub(crate) fn parse(s: &str) -> AppResult<Self> {
        match s {
            "keep_local" => Ok(Self::KeepLocal),
            "use_remote" => Ok(Self::UseRemote),
            "keep_both" => Ok(Self::KeepBoth),
            other => Err(AppError::Sync(format!("unknown conflict action: {other}"))),
        }
    }
}

/// Resolve one conflict per the user's choice (DBSYNC-35). Acts on the copies the
/// auto-resolver already produced, so no version is ever the only casualty — except
/// `UseRemote` in the remote-deleted scenario, which the UI double-confirms.
///
/// Data-safety: local deletions untrack the index row BEFORE removing the file
/// (mirrors dehydrate) so a racing scan never reads the removal as a tracked-file
/// deletion and propagates a spurious Dropbox delete. Remote mutations go through
/// the job queue (retries + the dehydration-suppression guard). A double click is a
/// no-op — `get_unresolved_conflict` returns `None` once the row is resolved.
pub(crate) fn resolve_conflict_internal(
    state: &AppState,
    id: i64,
    action: ConflictAction,
) -> AppResult<()> {
    let Some(conflict) = state.db.get_unresolved_conflict(id)? else {
        return Ok(());
    };
    let folder = state
        .db
        .get_sync_folder()?
        .ok_or_else(|| AppError::Sync("sync folder not configured".to_string()))?;
    let root = Path::new(&folder);
    let primary_rel = conflict.local_path.as_str();
    let copy_rel = conflict.conflicted_copy_path.as_deref();

    match (action, copy_rel, conflict.remote_deleted) {
        // ── Keep Local ──────────────────────────────────────────────────────────
        // Promote the preserved local edit (the conflicted copy) over the primary and
        // push it to Dropbox, then drop the now-redundant copy from the remote.
        (ConflictAction::KeepLocal, Some(copy_rel), _) => {
            let copy_abs = safe_join(root, copy_rel)?;
            let primary_abs = safe_join(root, primary_rel)?;
            if copy_abs.exists() {
                std::fs::rename(&copy_abs, &primary_abs)
                    .map_err(|e| AppError::Io(format!("failed promoting conflicted copy: {e}")))?;
                state.db.remove_local_file(copy_rel)?;
            }
            state
                .db
                .enqueue_job("upload", Some(primary_rel), Some(primary_rel))?;
            if state.db.get_remote_file(copy_rel)?.is_some() {
                state
                    .db
                    .enqueue_job("delete", Some(copy_rel), Some(copy_rel))?;
            }
        }
        // No copy (remote-deleted scenario): the local primary IS the version to keep
        // — re-upload it to restore the remote.
        (ConflictAction::KeepLocal, None, _) => {
            state
                .db
                .enqueue_job("upload", Some(primary_rel), Some(primary_rel))?;
        }

        // ── Use Remote ──────────────────────────────────────────────────────────
        // Copy exists → the primary already holds the remote content; discard the
        // preserved local edit (delete the copy locally, and remotely if uploaded).
        (ConflictAction::UseRemote, Some(copy_rel), _) => {
            let copy_abs = safe_join(root, copy_rel)?;
            state.db.remove_local_file(copy_rel)?; // untrack before delete
            if copy_abs.exists() {
                std::fs::remove_file(&copy_abs)
                    .map_err(|e| AppError::Io(format!("failed removing conflicted copy: {e}")))?;
            }
            if state.db.get_remote_file(copy_rel)?.is_some() {
                state
                    .db
                    .enqueue_job("delete", Some(copy_rel), Some(copy_rel))?;
            }
        }
        // Remote was deleted and there is no copy: "follow remote" means discarding the
        // diverged local file. The UI double-confirms this destructive choice.
        (ConflictAction::UseRemote, None, true) => {
            let primary_abs = safe_join(root, primary_rel)?;
            state.db.remove_local_file(primary_rel)?; // untrack before delete
            state.db.remove_remote_file(primary_rel)?;
            if primary_abs.exists() {
                std::fs::remove_file(&primary_abs)
                    .map_err(|e| AppError::Io(format!("failed removing local file: {e}")))?;
            }
        }
        // Remote present, no copy: the primary already equals the remote — nothing to
        // discard.
        (ConflictAction::UseRemote, None, false) => {}

        // ── Keep Both ───────────────────────────────────────────────────────────
        // Copy exists: both versions live on disk; upload the copy so both reach
        // Dropbox promptly (the scan would eventually do this anyway).
        (ConflictAction::KeepBoth, Some(copy_rel), _) => {
            if safe_join(root, copy_rel)?.exists() {
                state
                    .db
                    .enqueue_job("upload", Some(copy_rel), Some(copy_rel))?;
            }
        }
        // Remote deleted, no copy: there is no second version to keep — restore the
        // remote from the local primary (degenerates to Keep Local).
        (ConflictAction::KeepBoth, None, true) => {
            state
                .db
                .enqueue_job("upload", Some(primary_rel), Some(primary_rel))?;
        }
        (ConflictAction::KeepBoth, None, false) => {}
    }

    state.db.mark_conflict_resolved(id)?;
    // Recompute error/overlay state so the resolved path stops being flagged.
    refresh_queue_depth_internal(state)?;
    Ok(())
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

/// Run the full sync cycle: local scan + queue drain, THEN the remote
/// materialization sweep. Ordering is load-bearing — the tick must run
/// first so a local folder deletion's `delete` job drains (deleting the
/// folder remotely) BEFORE discovery runs; otherwise discovery would see
/// the still-orphaned remote folder and re-create its `.cloudsc`
/// placeholder (DBSYNC-66). Callers own the `sync_running` gate; this
/// helper does not touch it.
pub(crate) fn full_sync_cycle(state: &AppState) {
    let _ = run_sync_tick_internal(state);
    match crate::cloudsc_ops::index_materialized_folders_as_cloudsc_placeholders_internal(state) {
        Ok(n) if n > 0 => tracing::info!(count = n, "indexed new remote placeholder(s)"),
        Ok(_) => {}
        Err(e) => tracing::error!(error = %e, "remote placeholder indexing failed"),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    use chrono::{Duration, Utc};
    use tempfile::tempdir;

    use super::{
        block_mass_deletion, cleanup_stale_upload_state, clear_mass_delete_blocked,
        is_mass_deletion, mass_delete_pause_active, process_changed_paths,
        resolve_conflict_internal, run_sync_tick_internal, scan_local_changes_only, ConflictAction,
        MassDeleteSource, SYNC_BATCH_CAP,
    };
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
    pub(crate) fn build_state(root: &std::path::Path) -> AppState {
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

    /// DBSYNC-56, the half that matters: marking the row must actually cause a re-upload.
    ///
    /// This is the recovery, seen from the scan's side. Without the marker the row would
    /// hold the same hash the file on disk has, `prev.hash != hash` would be false, and the
    /// scan would walk straight past a file whose content Dropbox has never received.
    #[test]
    fn a_row_marked_for_rescan_is_re_detected_and_re_uploaded() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let folder = state.db.get_sync_folder().unwrap().unwrap();
        let target = std::path::Path::new(&folder).join("report.docx");
        std::fs::write(&target, b"the edit that must not be lost").expect("write");

        // The state the cancelled upload leaves behind: the row exists, its hash is
        // unusable, and the file on disk is the content nobody has uploaded.
        state
            .db
            .upsert_local_file("report.docx", "H2", 30, 0)
            .expect("seed row");
        state
            .db
            .mark_local_file_for_rescan("report.docx")
            .expect("mark it");

        let enqueued = scan_local_changes_only(&state).expect("scan");

        assert_eq!(enqueued, 1, "the marked row must be re-detected");
        let uploads: Vec<String> = state
            .db
            .list_recent_jobs(100)
            .expect("jobs")
            .into_iter()
            .filter(|j| j.job_type == "upload")
            .filter_map(|j| j.target_path)
            .collect();
        assert_eq!(uploads, vec!["report.docx".to_string()]);

        // And the marker is gone afterwards: the scan wrote the real hash back, so the
        // next tick does not re-upload the same file forever.
        let row = state.db.get_local_file("report.docx").unwrap().unwrap();
        assert_ne!(row.hash, Db::HASH_NEEDS_RESCAN);
    }

    #[test]
    fn is_confirmed_absent_distinguishes_missing_from_present() {
        let tmp = tempdir().expect("tempdir");
        let present = tmp.path().join("here.txt");
        std::fs::write(&present, b"x").expect("write");
        assert!(!super::is_confirmed_absent(&present));
        assert!(super::is_confirmed_absent(&tmp.path().join("nope.txt")));
    }

    /// **This is the test the NIT actually needed**, and the first version of this change
    /// did not have it. `Path::exists()` already answers present-vs-absent correctly, so a
    /// test covering only those two cases passes just as happily with `!p.exists()` — it
    /// certifies nothing. The distinction that matters is **absent vs unreadable**, and it
    /// takes a path that exists but cannot be stat'd to exercise it.
    #[cfg(unix)]
    #[test]
    fn is_confirmed_absent_is_false_for_a_path_it_cannot_stat() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempdir().expect("tempdir");
        let locked = tmp.path().join("locked");
        std::fs::create_dir(&locked).expect("mkdir");
        let hidden = locked.join("file.txt");
        std::fs::write(&hidden, b"x").expect("write");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("chmod");

        // Both observations happen BEFORE any assertion, and permissions are restored before
        // any of them can panic: an early unwind would leave a 0o000 directory that
        // `TempDir::drop` cannot clean up.
        let unreadable = std::fs::symlink_metadata(&hidden).is_err();
        let verdict = super::is_confirmed_absent(&hidden);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("restore");

        // Root ignores directory permissions, so under root the premise does not hold and
        // the real assertion below would pass vacuously. Fail loudly rather than prove
        // nothing quietly.
        assert!(
            unreadable,
            "premise failed: the path was readable (running as root?)"
        );
        assert!(
            !verdict,
            "a path that cannot be stat'd is NOT confirmed absent"
        );
    }

    /// DBSYNC-56. A marked row with a job pending MUST produce a conflicted copy, and this
    /// test asserts the opposite of what an earlier version of this change shipped.
    ///
    /// That version excluded the marker from the conflicted-copy arm, calling the copy
    /// "litter". It is not litter: the only job that can be active on a marked path is one
    /// the remote side queued, so the copy is the protection. Excluding it cleared the
    /// marker before the download drained, which disarmed `download_would_conflict` and let
    /// remote content overwrite an edit Dropbox had never received — the exact loss this
    /// ticket exists to fix, re-created by a fix for a review finding that was itself wrong.
    #[test]
    fn a_marked_row_makes_a_conflicted_copy_when_a_job_is_pending() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let folder = state.db.get_sync_folder().unwrap().unwrap();
        let target = std::path::Path::new(&folder).join("report.docx");
        std::fs::write(&target, b"the edit that must not be lost").expect("write");

        state
            .db
            .upsert_local_file("report.docx", "H2", 30, 0)
            .expect("seed row");
        state
            .db
            .mark_local_file_for_rescan("report.docx")
            .expect("mark it");
        // Anything active on the path is enough — a download queued by the remote sweep, or
        // an upload sitting in retry_wait after a network blip.
        state
            .db
            .enqueue_job("download", Some("report.docx"), Some("report.docx"))
            .expect("enqueue");

        scan_local_changes_only(&state).expect("scan");

        let copies: Vec<String> = std::fs::read_dir(&folder)
            .expect("readdir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains("conflicted copy"))
            .collect();
        assert_eq!(
            copies.len(),
            1,
            "the unuploaded bytes must be preserved before the queued download lands: {copies:?}"
        );

        // And the copy carries the content that was at risk, not an empty placeholder —
        // preserving the wrong bytes would satisfy the count and lose the edit anyway.
        let copy = std::path::Path::new(&folder).join(&copies[0]);
        assert_eq!(
            std::fs::read(&copy).expect("read copy"),
            b"the edit that must not be lost"
        );

        // The upload must target the COPY, never the original. An upload on the original
        // races the download already queued there, and whichever drains first discards the
        // other side — that was the second half of the loss the previous version of this
        // test asserted as correct. Without this assertion a one-word edit reintroduces it
        // with the whole suite green.
        let upload_targets: Vec<String> = state
            .db
            .list_recent_jobs(100)
            .expect("jobs")
            .into_iter()
            .filter(|j| j.job_type == "upload")
            .filter_map(|j| j.target_path)
            .collect();
        assert_eq!(upload_targets, vec![copies[0].clone()]);
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

    /// Both paths of every move job, as `(source, target)`.
    ///
    /// `job_targets` reads only `target_path`, and that blind spot let a real defect
    /// through: a move was enqueued with the right target and a `source_path` that had been
    /// rewritten to the same value, asking Dropbox to move a file to where it already was.
    /// Every assertion about a move must therefore look at both halves — a job that exists
    /// and points at the right destination is not the same as a job that is well formed.
    fn move_jobs(state: &AppState) -> Vec<(String, String)> {
        state
            .db
            .list_recent_jobs(500)
            .expect("jobs")
            .into_iter()
            .filter(|j| j.job_type == "move")
            .map(|j| {
                (
                    j.source_path.unwrap_or_default(),
                    j.target_path.unwrap_or_default(),
                )
            })
            .collect()
    }

    // ── DBSYNC-104 H1: the retry-or-give-up decision is asserted, not just commented ──

    /// Review H1: the behaviour change shipped once with NO test that reached it —
    /// disabling the branch left the whole suite green, because the only coverage was on
    /// the classifier in isolation. The decision is now a pure function so it can be
    /// asserted directly. Mutating it reddens these tests.
    #[test]
    fn a_path_dropbox_will_never_accept_fails_on_the_first_attempt() {
        let rejected = crate::error::AppError::Dropbox {
            status: 409,
            message: "upload for /a\\b.txt: {\"error\":{\".tag\":\"path\",\"reason\":\
                      {\".tag\":\"malformed_path\",\"malformed_path\":null}},\
                      \"error_summary\":\"path/malformed_path/\"}"
                .to_string(),
        };

        assert_eq!(
            super::classify_job_failure(&rejected, 1, 5),
            super::JobFailure::Permanent(super::PermanentReason::UnrepresentablePath),
            "attempt 1 of 5 must still be permanent — no backoff makes Dropbox accept it"
        );
        // And at every other attempt count, for the same reason.
        for attempt in [2, 3, 4, 5] {
            assert!(matches!(
                super::classify_job_failure(&rejected, attempt, 5),
                super::JobFailure::Permanent(super::PermanentReason::UnrepresentablePath)
            ));
        }
    }

    #[test]
    fn an_ordinary_error_still_retries_until_its_attempts_run_out() {
        let transient = crate::error::AppError::Network("connection reset".to_string());

        for attempt in [1, 2, 3, 4] {
            assert_eq!(
                super::classify_job_failure(&transient, attempt, 5),
                super::JobFailure::Retry,
                "attempt {attempt} of 5 must still retry"
            );
        }
        assert_eq!(
            super::classify_job_failure(&transient, 5, 5),
            super::JobFailure::Permanent(super::PermanentReason::AttemptsExhausted)
        );
    }

    /// Review round 2 rejected an earlier version of this test: it called `mark_job_failed`
    /// by hand and then `pick_next_due_job`, which re-asserts a `Db` property that holds
    /// with this feature deleted entirely — and it survived a mutation that removed the
    /// feature from production. This one drives `apply_job_failure`, the function the drain
    /// actually calls, so the row, the attempt count and the message are consequences of
    /// the code under test.
    #[test]
    fn an_unacceptable_path_is_failed_at_attempt_one_with_a_message_and_never_re_queued() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());

        state
            .db
            .enqueue_job("upload", None, Some("a\\b.txt"))
            .unwrap();
        let job = state.db.pick_next_due_job().unwrap().expect("queued job");

        let rejected = crate::error::AppError::Dropbox {
            status: 409,
            message: "upload for /a\\b.txt: {\"error\":{\".tag\":\"path\",\"reason\":\
                      {\".tag\":\"malformed_path\",\"malformed_path\":null}},\
                      \"error_summary\":\"path/malformed_path/\"}"
                .to_string(),
        };

        // Attempt 1 of 5 — an ordinary error here would be parked in `retry_wait`.
        super::apply_job_failure(&state, &job, 1, 5, &rejected).expect("apply");

        let row = state
            .db
            .list_recent_jobs(10)
            .unwrap()
            .into_iter()
            .find(|j| j.id == job.id)
            .expect("row");
        assert_eq!(row.status, "failed", "must not be parked in retry_wait");
        assert_eq!(
            row.attempt_count, 1,
            "failed at the first attempt, not the fifth"
        );

        let err = row.last_error.expect("a message the user can act on");
        assert!(err.contains("a\\b.txt"), "must name the file: {err}");
        assert!(err.contains("Rename it"), "must say what to do: {err}");

        assert!(
            state.db.pick_next_due_job().unwrap().is_none(),
            "the job must not re-enter the retry queue"
        );
    }

    /// The other side of the same function: an ordinary failure still gets its full budget.
    /// Without this, failing everything at attempt 1 would also satisfy the test above.
    #[test]
    fn an_ordinary_failure_is_still_parked_for_retry_at_attempt_one() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());

        state
            .db
            .enqueue_job("upload", None, Some("ordinary.txt"))
            .unwrap();
        let job = state.db.pick_next_due_job().unwrap().expect("queued job");

        let transient = crate::error::AppError::Network("connection reset".to_string());
        super::apply_job_failure(&state, &job, 1, 5, &transient).expect("apply");

        let row = state
            .db
            .list_recent_jobs(10)
            .unwrap()
            .into_iter()
            .find(|j| j.id == job.id)
            .expect("row");
        assert_eq!(
            row.status, "retry_wait",
            "a transient error must keep its attempt budget"
        );
    }

    /// Review H2(b): `malformed_path` is Dropbox's general "name does not satisfy the
    /// format" tag — illegal characters, trailing space or period, over-long components —
    /// not only the backslash that happened to be probed. A message naming just the
    /// backslash tells someone with `report?.txt` to remove a character that is not there.
    #[test]
    fn the_unrepresentable_message_names_the_file_and_describes_the_rule() {
        let msg = super::unrepresentable_path_message("Informes/report?.txt");

        assert!(
            msg.contains("Informes/report?.txt"),
            "must name the file: {msg}"
        );
        assert!(msg.contains("Rename it"), "must say what to do: {msg}");
        assert!(
            msg.contains('?') && msg.contains('*') && msg.contains('|'),
            "must describe the rule, not only the backslash: {msg}"
        );
    }

    /// Review H3: `a\..\c.txt` is ONE legal macOS filename, but `has_traversal` split on
    /// `\` unconditionally and called it a traversal. The pipeline indexed the file and
    /// enqueued an upload anyway, and that upload then failed with `AppError::Sync` —
    /// which is not an `AppError::Dropbox`, so it escaped the permanent-path classifier,
    /// burned five attempts and reported a generic error. The row was also permanently
    /// unnormalizable, so both remote sweeps logged a warning for it on every tick.
    ///
    /// Drives the real scan against real files, because the defect was in the gap between
    /// what the index accepted and what `normalize_dropbox_path` would later reject.
    #[cfg(not(windows))]
    #[test]
    fn a_backslash_name_that_looks_like_a_traversal_is_an_ordinary_file() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let sync = tmp.path().join("synced");

        std::fs::write(sync.join("a\\..\\c.txt"), b"one legal name").expect("write");
        super::scan_local_changes_only(&state).expect("scan");

        let rows: Vec<String> = state
            .db
            .list_local_files()
            .unwrap()
            .into_iter()
            .map(|r| r.relative_path)
            .collect();
        assert_eq!(rows, vec!["a\\..\\c.txt".to_string()]);

        // The gap that caused the defect: indexed, but rejected downstream.
        assert!(
            crate::path_util::normalize_dropbox_path("a\\..\\c.txt").is_ok(),
            "an indexed path must be normalizable, or it fails outside the classifier's reach"
        );
        assert!(crate::path_util::validate_relative("a\\..\\c.txt").is_ok());

        // And the real safety property still holds: it cannot escape the root.
        let joined = crate::path_util::safe_join(&sync, "a\\..\\c.txt").expect("joins");
        assert!(joined.starts_with(&sync));

        // A genuine traversal is still refused, on both platforms.
        assert!(crate::path_util::normalize_dropbox_path("a/../../etc/passwd").is_err());
        assert!(crate::path_util::validate_relative("../escape").is_err());
    }

    // ── DBSYNC-104 M5: the headline fix, driven through the real pipeline ──

    /// Review M5: every other test of the aliasing fix passes a literal key to `Db`, so
    /// ungating `relpath_under` reddened exactly one unit test and none of the storage
    /// ones. This walks a real directory instead: two files on disk that differ only by
    /// `\` versus `/` must become two rows and two independent upload jobs.
    #[cfg(not(windows))]
    #[test]
    fn two_files_differing_only_by_a_backslash_scan_into_two_rows_and_two_jobs() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let sync = tmp.path().join("synced");

        // The genuine nested file, and a root file whose NAME contains a backslash.
        std::fs::create_dir_all(sync.join("a")).expect("mkdir");
        std::fs::write(sync.join("a").join("b.txt"), b"genuine").expect("write");
        std::fs::write(sync.join("a\\b.txt"), b"weird bytes").expect("write");

        super::scan_local_changes_only(&state).expect("scan");

        let mut rows: Vec<String> = state
            .db
            .list_local_files()
            .unwrap()
            .into_iter()
            .map(|r| r.relative_path)
            .collect();
        rows.sort();
        assert_eq!(
            rows,
            vec!["a/b.txt".to_string(), "a\\b.txt".to_string()],
            "two distinct files must produce two distinct index rows"
        );

        let mut targets = job_targets(&state, "upload");
        targets.sort();
        assert_eq!(
            targets,
            vec!["a/b.txt".to_string(), "a\\b.txt".to_string()],
            "each file must get its own upload job, not one overwriting the other"
        );
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

    // ── DBSYNC-106: a rename deferred by a busy gate is still correlated ──────

    /// The defect: `on_debounced_batch` dropped the whole batch when the sync gate was
    /// held, on the promise that "the periodic fallback or the next batch will pick these
    /// paths up". Both halves are false for a rename — `scan_local_changes_only` does not
    /// correlate, and FSEvents does not redeliver — so the rename degraded into uploading
    /// the new prefix and deleting the old, splitting the folder across two names.
    ///
    /// Drives the real drain against a real `Db`, with the folder already on disk under its
    /// new name. The index is seeded directly rather than by scanning, for the same reason
    /// `a_directory_rename_is_billed_as_one_move` does: a scan would leave an upload queued
    /// for the child, and queued work legitimately refuses correlation. The premise here is
    /// a folder that was already in sync when it was renamed.
    #[test]
    fn a_rename_deferred_by_a_busy_gate_still_becomes_a_move() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);

        // On disk under the NEW name; the index and Dropbox still hold the old one.
        std::fs::create_dir_all(root.join("Papers")).unwrap();
        std::fs::write(root.join("Papers/a.txt"), b"payload").unwrap();
        state.db.upsert_known_folder("Docs").unwrap();
        let (hash, size, mtime) = crate::path_util::hash_file(&root.join("Papers/a.txt")).unwrap();
        state
            .db
            .upsert_local_file("Docs/a.txt", &hash, size, mtime)
            .unwrap();
        state
            .db
            .upsert_remote_file("Docs/a.txt", &hash, "rev1", mtime, None)
            .unwrap();

        // The watcher's batch lost the gate, so the paths were deferred instead of processed.
        let _ = crate::fs_watcher::take_pending();
        crate::fs_watcher::remember_dropped(&[
            "Docs".to_string(),
            "Papers".to_string(),
            "Papers/a.txt".to_string(),
        ]);

        // The gate frees; the deferred paths are drained through the correlating path.
        super::drain_deferred_watcher_paths(&state);

        assert_eq!(
            move_jobs(&state),
            vec![("Docs".to_string(), "Papers".to_string())],
            "the deferred rename must still be correlated as one move"
        );

        // The point of the ticket: no bytes re-uploaded, nothing deleted from the old name.
        assert!(
            job_targets(&state, "upload").is_empty(),
            "a correlated rename must not re-upload anything: {:?}",
            job_targets(&state, "upload")
        );
        assert!(
            job_targets(&state, "delete").is_empty(),
            "a correlated rename must not delete the old prefix: {:?}",
            job_targets(&state, "delete")
        );
    }

    /// The deferral must be consumed. If the drain left the paths in place they would be
    /// replayed on every tick, re-proposing a move for a folder that has already moved.
    #[test]
    fn draining_consumes_the_deferred_paths() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());

        crate::fs_watcher::remember_dropped(&["whatever.txt".to_string()]);
        super::drain_deferred_watcher_paths(&state);

        assert!(
            crate::fs_watcher::take_pending().is_empty(),
            "the drain must leave the pending set empty"
        );
    }

    /// With nothing deferred the drain is inert — it must not walk anything or enqueue
    /// anything, since it runs on every periodic tick.
    #[test]
    fn draining_nothing_enqueues_nothing() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let _ = crate::fs_watcher::take_pending();

        assert_eq!(super::drain_deferred_watcher_paths(&state), 0);
        assert!(state.db.list_recent_jobs(10).unwrap().is_empty());
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
        state
            .db
            .upsert_local_file("m.txt", "stale-hash", 3, 0)
            .unwrap();

        let n = process_changed_paths(&state, &["m.txt".to_string()]).expect("process");
        assert_eq!(n, 1);
        assert_eq!(job_targets(&state, "upload"), vec!["m.txt".to_string()]);
    }

    /// DBSYNC-96 asked what a rename actually costs, and this test answered: a delete plus a
    /// full upload. **DBSYNC-99 inverts it** — the same scenario, the opposite assertions.
    ///
    /// The original claim came from reading three things (`fs_watcher` keeps only `e.path`,
    /// there is no `move` job type, a vanished path reaches `enqueue_targeted_deletions`), and
    /// reading is not evidence of behaviour, which is why it was written as a test. Kept in the
    /// same shape so the two versions can be diffed against each other.
    ///
    /// **Both paths go into ONE call on purpose.** The watcher hands over a single debounced
    /// batch, so the old and new names arrive together. Splitting them across two calls would
    /// be a weaker test: correlation is only possible within a batch, so a split test could not
    /// detect it even when it works.
    #[test]
    fn a_rename_is_billed_as_a_move() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);

        // On disk the file exists only under its new name...
        std::fs::write(root.join("new.txt"), b"hello").unwrap();
        // ...and the index holds it under the old one, with the content hash it really has.
        // That equality IS the rename: same bytes, different name.
        let (hash, size, mtime) = crate::path_util::hash_file(&root.join("new.txt")).unwrap();
        state
            .db
            .upsert_local_file("old.txt", &hash, size, mtime)
            .unwrap();
        // Dropbox already holds it under the old name — otherwise there is nothing to move.
        state
            .db
            .upsert_remote_file("old.txt", &hash, "rev1", mtime, Some("id:OLD"))
            .unwrap();
        let identity = state
            .db
            .get_local_file("old.txt")
            .unwrap()
            .unwrap()
            .item_id
            .expect("identity");

        let n = process_changed_paths(&state, &["old.txt".to_string(), "new.txt".to_string()])
            .expect("process");

        // One move, not two jobs.
        assert_eq!(n, 1);
        // Both halves. A move whose source equals its target is a no-op Dropbox rejects.
        assert_eq!(
            move_jobs(&state),
            vec![("old.txt".to_string(), "new.txt".to_string())]
        );
        assert!(
            job_targets(&state, "delete").is_empty(),
            "nothing is deleted"
        );
        assert!(
            job_targets(&state, "upload").is_empty(),
            "and not one byte goes back up"
        );

        // And the index has NOT moved yet. That is the property that makes a failed move
        // incapable of losing data: there is nothing to undo, because nothing was written.
        // The rewrite happens in `move_remote_file_internal`, after Dropbox confirms.
        let still_there = state.db.get_local_file("old.txt").unwrap().expect("row");
        assert_eq!(still_there.item_id, Some(identity));
        assert_eq!(still_there.hash, hash);
        assert!(
            state.db.get_local_file("new.txt").unwrap().is_none(),
            "the destination row appears only once the server has confirmed the move"
        );

        // Both paths are protected from the scan while the job waits.
        let pending = state.db.active_job_paths().unwrap();
        assert!(
            pending.contains("old.txt"),
            "the source must not look deleted"
        );
        assert!(
            pending.contains("new.txt"),
            "the destination must not look new"
        );
    }

    /// A file Dropbox has never received cannot be moved on Dropbox. Renaming it before its
    /// first upload drains must produce an upload at the new name, not a move that would
    /// fail `from_lookup/not_found` and leave the file never uploaded at all.
    #[test]
    fn a_file_dropbox_has_never_seen_is_uploaded_not_moved() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);

        std::fs::write(root.join("new.txt"), b"hello").unwrap();
        let (hash, size, mtime) = crate::path_util::hash_file(&root.join("new.txt")).unwrap();
        // Indexed locally, but deliberately NO remote row.
        state
            .db
            .upsert_local_file("old.txt", &hash, size, mtime)
            .unwrap();

        process_changed_paths(&state, &["old.txt".to_string(), "new.txt".to_string()])
            .expect("process");

        assert!(
            job_targets(&state, "move").is_empty(),
            "there is nothing on Dropbox to move"
        );
        assert_eq!(job_targets(&state, "upload"), vec!["new.txt".to_string()]);
    }

    /// A rename that races queued work on the old path must fall back to the old behaviour.
    ///
    /// `move_index_row` retargets an active job to the new name. If that job drains before
    /// the move does, Dropbox rejects the move for a destination that is now occupied, the
    /// move is dropped as not-applicable, and the copy under the OLD name is left behind on
    /// the server. Renaming cheaply is an optimisation; not leaking a remote copy is not.
    #[test]
    fn a_rename_racing_queued_work_falls_back_to_delete_plus_upload() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);

        std::fs::write(root.join("new.txt"), b"hello").unwrap();
        let (hash, size, mtime) = crate::path_util::hash_file(&root.join("new.txt")).unwrap();
        state
            .db
            .upsert_local_file("old.txt", &hash, size, mtime)
            .unwrap();
        state
            .db
            .upsert_remote_file("old.txt", &hash, "rev1", mtime, Some("id:OLD"))
            .unwrap();
        // An upload of the old name is already queued and has not drained.
        state
            .db
            .enqueue_job("upload", Some("old.txt"), Some("old.txt"))
            .unwrap();

        process_changed_paths(&state, &["old.txt".to_string(), "new.txt".to_string()])
            .expect("process");

        assert!(
            job_targets(&state, "move").is_empty(),
            "the optimisation must be given up when it could race"
        );
        assert!(
            job_targets(&state, "upload").contains(&"new.txt".to_string()),
            "the bytes still go up under the new name"
        );
        // The delete IS emitted, and this assertion has been round the houses.
        //
        // It said exactly this originally. Round 4 weakened it to assert no delete, to match
        // a deferral that held deletions back for ANY job in flight. Round 5 showed that
        // deferral does not defer — the materialization sweep plants a `.cloudsc` sidecar for
        // the still-remote path, and a delete whose path has a placeholder is dropped along
        // with the index row that remembers it. The file stayed on Dropbox forever.
        //
        // Only a queued MOVE holds a deletion back now, because only a move relocates the
        // path. An upload does not, so this deletion goes out in the same batch and drains
        // before any sweep can run — the window it always had.
        assert_eq!(
            job_targets(&state, "delete"),
            vec!["old.txt".to_string()],
            "an upload in flight must not hold back the deletion"
        );
    }

    // -----------------------------------------------------------------------------------
    // Regressions from the DBSYNC-99 code review. Every one of these reproduced a defect
    // that green CI and a passing manual QA were both blind to.
    // -----------------------------------------------------------------------------------

    /// C3. The correlator used to ask only whether a remote row EXISTED. Between an edit
    /// and a successful upload the local hash runs ahead of the remote one, and once that
    /// upload reaches `failed` it leaves `active_job_paths` so the pending-work guard stops
    /// firing. Moving then tells Dropbox to relocate content it holds — the OLD content —
    /// and the edit is stranded with nothing left to notice it. The delete-plus-upload this
    /// replaced was wasteful, but it preserved those bytes.
    #[test]
    fn a_rename_is_not_a_move_when_dropbox_holds_different_content() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);

        std::fs::write(root.join("new.txt"), b"edited content").unwrap();
        let (hash, size, mtime) = crate::path_util::hash_file(&root.join("new.txt")).unwrap();
        state
            .db
            .upsert_local_file("old.txt", &hash, size, mtime)
            .unwrap();
        // Dropbox still holds the PRE-EDIT bytes: the upload never landed.
        state
            .db
            .upsert_remote_file(
                "old.txt",
                "STALE_REMOTE_HASH",
                "rev1",
                mtime,
                Some("id:OLD"),
            )
            .unwrap();

        process_changed_paths(&state, &["old.txt".to_string(), "new.txt".to_string()])
            .expect("process");

        assert!(
            job_targets(&state, "move").is_empty(),
            "Dropbox does not hold these bytes, so there is nothing to move"
        );
        assert_eq!(
            job_targets(&state, "upload"),
            vec!["new.txt".to_string()],
            "the edited bytes must go up"
        );
    }

    /// C4. The destination was only checked against the LOCAL index. A path Dropbox holds
    /// but which has not been downloaded has a remote row and no local row; the bare UPDATE
    /// in `move_index_row` then hit a UNIQUE violation whose error propagated out of
    /// `process_changed_paths` and discarded every other path in the batch.
    #[test]
    fn a_destination_already_known_remotely_does_not_abort_the_batch() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);

        std::fs::write(root.join("new.txt"), b"hello").unwrap();
        let (hash, size, mtime) = crate::path_util::hash_file(&root.join("new.txt")).unwrap();
        state
            .db
            .upsert_local_file("old.txt", &hash, size, mtime)
            .unwrap();
        state
            .db
            .upsert_remote_file("old.txt", &hash, "rev1", mtime, Some("id:OLD"))
            .unwrap();
        // Dropbox already has something at the destination; we have never downloaded it.
        state
            .db
            .upsert_remote_file("new.txt", "OTHER", "rev9", 0, Some("id:OTHER"))
            .unwrap();
        // An unrelated file in the same batch, whose fate proves the batch survived.
        std::fs::write(root.join("unrelated.txt"), b"xyz").unwrap();

        let result = process_changed_paths(
            &state,
            &[
                "old.txt".to_string(),
                "new.txt".to_string(),
                "unrelated.txt".to_string(),
            ],
        );

        assert!(result.is_ok(), "the batch must not abort: {result:?}");
        assert!(
            job_targets(&state, "upload").contains(&"unrelated.txt".to_string()),
            "the rest of the batch must still be processed"
        );
    }

    /// C2, first guard. The directory correlator compared nothing — not content, not size,
    /// not whether the candidate was already a tracked folder. Two real folders each holding
    /// a `README.md` were paired as a rename, which suppressed the genuine deletion of one,
    /// enqueued a bogus move, and orphaned index rows.
    ///
    /// **The two READMEs are deliberately the same size**, so the size guard cannot fire and
    /// only the already-tracked check can save this. A first version of this test gave them
    /// different sizes and passed with the guard it is named after removed — green for a
    /// reason unrelated to its own name, which is the exact defect class this review found.
    #[test]
    fn a_folder_we_already_track_is_never_a_rename_destination() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);

        std::fs::create_dir_all(root.join("e")).unwrap();
        std::fs::write(root.join("e/README.md"), b"aaaaa").unwrap();
        state.db.upsert_known_folder("e").unwrap();
        state
            .db
            .upsert_local_file("e/README.md", "hE", 5, 0)
            .unwrap();
        state
            .db
            .upsert_remote_file("e/README.md", "hE", "rev", 0, Some("id:E"))
            .unwrap();

        // `d` is tracked, gone from disk, and its README is the SAME five bytes.
        state.db.upsert_known_folder("d").unwrap();
        state
            .db
            .upsert_local_file("d/README.md", "hD", 5, 0)
            .unwrap();
        state
            .db
            .upsert_remote_file("d/README.md", "hD", "rev", 0, Some("id:D"))
            .unwrap();

        process_changed_paths(&state, &["d".to_string(), "e".to_string()]).expect("process");

        assert!(
            job_targets(&state, "move").is_empty(),
            "a folder we are already tracking is a different folder, not a destination"
        );
        assert!(
            job_targets(&state, "delete").contains(&"d/README.md".to_string()),
            "and the real deletion must still propagate"
        );
    }

    /// C2, second guard. Matching descendant NAMES is not enough — an untracked folder that
    /// happens to contain the same filenames is not the same folder. Size is already in
    /// `FileIndexRow` and each child is stat-ed anyway, so this costs no extra I/O.
    ///
    /// Here `e` is **not** tracked, so the first guard cannot fire and only the size
    /// comparison can block the pairing.
    #[test]
    fn a_folder_whose_children_differ_in_size_is_not_a_rename_destination() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);

        // Untracked on disk, same child name, different bytes.
        std::fs::create_dir_all(root.join("e")).unwrap();
        std::fs::write(root.join("e/README.md"), b"a much longer readme").unwrap();

        state.db.upsert_known_folder("d").unwrap();
        state
            .db
            .upsert_local_file("d/README.md", "hD", 5, 0)
            .unwrap();
        state
            .db
            .upsert_remote_file("d/README.md", "hD", "rev", 0, Some("id:D"))
            .unwrap();

        process_changed_paths(&state, &["d".to_string(), "e".to_string()]).expect("process");

        assert!(
            job_targets(&state, "move").is_empty(),
            "same filename, different contents — not a rename"
        );
        assert!(
            job_targets(&state, "delete").contains(&"d/README.md".to_string()),
            "and the real deletion must still propagate"
        );
    }

    /// W1. A child of a folder that is itself moving travels with it. Enqueueing both is
    /// not merely redundant: jobs drain by id, so one transient failure on the folder move
    /// lets the child move run first and create the destination, after which the folder
    /// move hits `to/conflict` and is dropped — stranding the old folder on Dropbox with no
    /// index row pointing at it.
    #[test]
    fn a_child_of_a_moving_directory_is_not_moved_separately() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);

        std::fs::create_dir_all(root.join("e")).unwrap();
        std::fs::write(root.join("e/one.txt"), b"aaa").unwrap();
        let (hash, size, mtime) = crate::path_util::hash_file(&root.join("e/one.txt")).unwrap();
        state.db.upsert_known_folder("d").unwrap();
        state
            .db
            .upsert_local_file("d/one.txt", &hash, size, mtime)
            .unwrap();
        state
            .db
            .upsert_remote_file("d/one.txt", &hash, "rev", mtime, Some("id:ONE"))
            .unwrap();

        let n = process_changed_paths(
            &state,
            &[
                "d".to_string(),
                "e".to_string(),
                "e/one.txt".to_string(),
                "d/one.txt".to_string(),
            ],
        )
        .expect("process");

        assert_eq!(
            move_jobs(&state),
            vec![("d".to_string(), "e".to_string())],
            "the folder move carries its children; a second move for a child is a trap"
        );
        assert_eq!(n, 1);
    }

    /// C3-bis. C3 was applied to the file correlator and not to the directory one, which kept
    /// asking whether a remote row EXISTED. The size guard does not cover for it: an in-place
    /// edit of the same length — a fixed-width record, an EXIF rewrite, a sqlite file — is
    /// exactly the shape that slips through. Dropbox would relocate the OLD contents and
    /// nothing would ever notice, because no code path compares the local hash against the
    /// remote one.
    #[test]
    fn a_directory_rename_is_not_a_move_when_dropbox_holds_different_content() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);

        std::fs::create_dir_all(root.join("e")).unwrap();
        std::fs::write(root.join("e/one.txt"), b"edited!!").unwrap();
        let (hash, size, mtime) = crate::path_util::hash_file(&root.join("e/one.txt")).unwrap();

        state.db.upsert_known_folder("d").unwrap();
        state
            .db
            .upsert_local_file("d/one.txt", &hash, size, mtime)
            .unwrap();
        // Same LENGTH, different bytes — the size guard is satisfied and cannot help.
        state
            .db
            .upsert_remote_file(
                "d/one.txt",
                "STALE_REMOTE_HASH",
                "rev1",
                mtime,
                Some("id:ONE"),
            )
            .unwrap();

        process_changed_paths(
            &state,
            &["d".to_string(), "e".to_string(), "e/one.txt".to_string()],
        )
        .expect("process");

        assert!(
            job_targets(&state, "move").is_empty(),
            "Dropbox does not hold these bytes, so there is nothing to move"
        );
        assert_eq!(
            job_targets(&state, "upload"),
            vec!["e/one.txt".to_string()],
            "the edited bytes must go up"
        );
    }

    /// W3-bis. `correlate_renames` refuses to correlate when the old path has queued work;
    /// the directory correlator never even received `pending_targets`. The sequence is
    /// ordinary: a child is edited and its upload queued, the user renames the folder, the
    /// rewrite retargets that upload to the new path, jobs drain by id so the upload runs
    /// first and CREATES the destination on Dropbox — and the folder move then fails
    /// `to/conflict`, which is the outcome that used to destroy the source.
    #[test]
    fn a_directory_rename_racing_queued_work_falls_back() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);

        std::fs::create_dir_all(root.join("e")).unwrap();
        std::fs::write(root.join("e/one.txt"), b"aaa").unwrap();
        let (hash, size, mtime) = crate::path_util::hash_file(&root.join("e/one.txt")).unwrap();

        state.db.upsert_known_folder("d").unwrap();
        state
            .db
            .upsert_local_file("d/one.txt", &hash, size, mtime)
            .unwrap();
        state
            .db
            .upsert_remote_file("d/one.txt", &hash, "rev1", mtime, Some("id:ONE"))
            .unwrap();
        // An upload of the child is already queued and has not drained.
        state
            .db
            .enqueue_job("upload", Some("d/one.txt"), Some("d/one.txt"))
            .unwrap();

        process_changed_paths(
            &state,
            &["d".to_string(), "e".to_string(), "e/one.txt".to_string()],
        )
        .expect("process");

        assert!(
            job_targets(&state, "move").is_empty(),
            "the optimisation must be given up when it could race"
        );
    }

    /// **The invariant the whole redesign exists for.**
    ///
    /// The index used to be rewritten when a move was enqueued, so every failure path had to
    /// undo it — and three rounds of review were spent on repairs that each turned out worse
    /// than the defect they fixed: a refused move deleting the user's file, then a refused
    /// folder move deleting the folder from Dropbox and every file locally, then an "abandon"
    /// that suppressed the fallback it deferred to. Six data-loss defects, all downstream of
    /// writing an outcome down before it happened.
    ///
    /// Nothing is written until Dropbox confirms. So this test does not check a repair — it
    /// checks that there is nothing to repair.
    #[test]
    fn a_move_that_never_runs_leaves_the_index_exactly_as_it_was() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);

        std::fs::write(root.join("new.txt"), b"hello").unwrap();
        let (hash, size, mtime) = crate::path_util::hash_file(&root.join("new.txt")).unwrap();
        state
            .db
            .upsert_local_file("old.txt", &hash, size, mtime)
            .unwrap();
        state
            .db
            .upsert_remote_file("old.txt", &hash, "rev1", mtime, Some("id:OLD"))
            .unwrap();
        let before_local = state.db.get_local_file("old.txt").unwrap().unwrap();
        let before_remote = state.db.get_remote_file("old.txt").unwrap().unwrap();

        process_changed_paths(&state, &["old.txt".to_string(), "new.txt".to_string()])
            .expect("process");

        // The move is queued and has not run. Every index row is byte-for-byte what it was.
        let after_local = state.db.get_local_file("old.txt").unwrap().expect("row");
        let after_remote = state.db.get_remote_file("old.txt").unwrap().expect("row");
        assert_eq!(after_local.hash, before_local.hash);
        assert_eq!(after_local.item_id, before_local.item_id);
        assert_eq!(after_remote.content_hash, before_remote.content_hash);
        assert_eq!(after_remote.dropbox_id, before_remote.dropbox_id);
        assert!(state.db.get_local_file("new.txt").unwrap().is_none());
        assert!(state.db.get_remote_file("new.txt").unwrap().is_none());
    }

    /// A full scan landing between the enqueue and the drain must not undo the intent.
    ///
    /// This is the hazard the redesign trades for: while the move waits, the index describes
    /// the old world and the disk describes the new one, so the source looks deleted and the
    /// destination looks like a stranger. Propagating either would be destructive — the
    /// deletion recursively so, for a folder.
    #[test]
    fn a_scan_while_a_move_is_queued_neither_deletes_the_source_nor_uploads_the_destination() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);

        std::fs::write(root.join("new.txt"), b"hello").unwrap();
        let (hash, size, mtime) = crate::path_util::hash_file(&root.join("new.txt")).unwrap();
        state
            .db
            .upsert_local_file("old.txt", &hash, size, mtime)
            .unwrap();
        state
            .db
            .upsert_remote_file("old.txt", &hash, "rev1", mtime, Some("id:OLD"))
            .unwrap();
        process_changed_paths(&state, &["old.txt".to_string(), "new.txt".to_string()])
            .expect("process");

        // The full scan runs before the move job drains.
        scan_local_changes_only(&state).expect("scan");

        assert!(
            job_targets(&state, "delete").is_empty(),
            "the source is not gone, it is waiting to be moved"
        );
        assert!(
            job_targets(&state, "upload").is_empty(),
            "and the destination must not be uploaded out from under the move"
        );
        assert_eq!(move_jobs(&state).len(), 1, "still exactly one move");
    }

    /// The folder twin of `a_scan_while_a_move_is_queued_...`.
    ///
    /// **Three review rounds ran aground on exactly this gap**: the file shape got a test, the
    /// folder shape did not, and the folder shape is where the recursive delete lives. The
    /// hazard is prefix-shaped — a queued `move d → e` puts only `d` and `e` in
    /// `active_job_paths`, while everything under both is equally in flight.
    #[test]
    fn a_scan_while_a_folder_move_is_queued_protects_the_whole_subtree() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);

        std::fs::create_dir_all(root.join("e")).unwrap();
        std::fs::write(root.join("e/one.txt"), b"aaa").unwrap();
        let (hash, size, mtime) = crate::path_util::hash_file(&root.join("e/one.txt")).unwrap();
        state.db.upsert_known_folder("d").unwrap();
        state
            .db
            .upsert_local_file("d/one.txt", &hash, size, mtime)
            .unwrap();
        state
            .db
            .upsert_remote_file("d/one.txt", &hash, "rev1", mtime, Some("id:ONE"))
            .unwrap();
        let identity = state
            .db
            .get_local_file("d/one.txt")
            .unwrap()
            .unwrap()
            .item_id;

        process_changed_paths(&state, &["d".to_string(), "e".to_string()]).expect("process");
        assert_eq!(move_jobs(&state), vec![("d".to_string(), "e".to_string())]);

        // The full scan runs before the move drains.
        scan_local_changes_only(&state).expect("scan");

        assert!(
            job_targets(&state, "upload").is_empty(),
            "a descendant of the destination is not a stranger — uploading it costs the item \
             its identity and makes the move fail to/conflict"
        );
        assert!(
            job_targets(&state, "delete").is_empty(),
            "and nothing under the source may be deleted; delete_v2 on a folder is recursive"
        );
        // The original identity is still the only one.
        assert_eq!(
            state
                .db
                .get_local_file("d/one.txt")
                .unwrap()
                .unwrap()
                .item_id,
            identity
        );
        assert!(state.db.get_local_file("e/one.txt").unwrap().is_none());
    }

    /// C1's folder case: a move queued by an EARLIER batch, with the old path reported absent
    /// by a later one. The targeted deletion path had no protection at all, so this produced
    /// a recursive remote delete of the folder the user had just renamed — propagated to every
    /// other device — plus the loss of every descendant's identity.
    #[test]
    fn a_later_batch_reporting_the_source_absent_does_not_delete_a_pending_move() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);

        std::fs::create_dir_all(root.join("e")).unwrap();
        std::fs::write(root.join("e/one.txt"), b"aaa").unwrap();
        let (hash, size, mtime) = crate::path_util::hash_file(&root.join("e/one.txt")).unwrap();
        state.db.upsert_known_folder("d").unwrap();
        state
            .db
            .upsert_local_file("d/one.txt", &hash, size, mtime)
            .unwrap();
        state
            .db
            .upsert_remote_file("d/one.txt", &hash, "rev1", mtime, Some("id:ONE"))
            .unwrap();

        // Batch one correlates the rename.
        process_changed_paths(&state, &["d".to_string(), "e".to_string()]).expect("batch 1");
        // Batch two reports the old path absent — the watcher does this routinely.
        process_changed_paths(&state, &["d".to_string()]).expect("batch 2");

        assert!(
            job_targets(&state, "delete").is_empty(),
            "the source is queued for a move, not gone"
        );
        assert_eq!(
            state.db.list_known_folders().unwrap(),
            vec!["d".to_string()],
            "and its folder row must survive, or the move confirms with the wrong shape"
        );
        assert!(
            state.db.get_local_file("d/one.txt").unwrap().is_some(),
            "and every descendant keeps its row, and therefore its identity"
        );
    }

    /// The file twin of the above.
    #[test]
    fn a_later_batch_reporting_a_renamed_file_absent_does_not_delete_it() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);

        std::fs::write(root.join("new.txt"), b"hello").unwrap();
        let (hash, size, mtime) = crate::path_util::hash_file(&root.join("new.txt")).unwrap();
        state
            .db
            .upsert_local_file("old.txt", &hash, size, mtime)
            .unwrap();
        state
            .db
            .upsert_remote_file("old.txt", &hash, "rev1", mtime, Some("id:OLD"))
            .unwrap();

        process_changed_paths(&state, &["old.txt".to_string(), "new.txt".to_string()])
            .expect("batch 1");
        process_changed_paths(&state, &["old.txt".to_string()]).expect("batch 2");

        assert!(job_targets(&state, "delete").is_empty());
        assert!(state.db.get_local_file("old.txt").unwrap().is_some());
    }

    /// C2 — the round-5 finding, and the worst kind: a deletion that is not deferred but
    /// **lost**.
    ///
    /// Holding a deletion back for any job in flight was too broad. The materialization sweep
    /// plants a `.cloudsc` sidecar for any remote child whose local counterpart is absent,
    /// consulting no index at all; `process_local_file_deletion` then drops a delete whose
    /// path has a placeholder and removes the index row that remembers it. The file stayed on
    /// Dropbox forever, a phantom sidecar sat on disk, and nothing ever asked again.
    ///
    /// The sequence is ordinary: edit a file, rename it before the upload drains. Only a
    /// **move** may hold a deletion back now, because only a move relocates the path.
    #[test]
    fn a_deletion_racing_an_upload_is_emitted_not_swallowed() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);

        std::fs::write(root.join("new.txt"), b"hello").unwrap();
        let (hash, size, mtime) = crate::path_util::hash_file(&root.join("new.txt")).unwrap();
        state
            .db
            .upsert_local_file("old.txt", &hash, size, mtime)
            .unwrap();
        state
            .db
            .upsert_remote_file("old.txt", &hash, "rev1", mtime, Some("id:OLD"))
            .unwrap();
        // The user edited it, so an upload is already queued and has not drained.
        state
            .db
            .enqueue_job("upload", Some("old.txt"), Some("old.txt"))
            .unwrap();

        process_changed_paths(&state, &["old.txt".to_string(), "new.txt".to_string()])
            .expect("process");

        assert!(
            job_targets(&state, "move").is_empty(),
            "correlation is still refused while a job names the old path"
        );
        assert_eq!(
            job_targets(&state, "delete"),
            vec!["old.txt".to_string()],
            "but the deletion must be EMITTED — an upload does not relocate the path, and \
             holding it back lets the sweep plant a sidecar that swallows it for good"
        );
    }

    /// C1 — the file correlator was left asking the exact-path question while the commit
    /// message said both correlators had been converted. With a folder move queued, a later
    /// batch carrying the child paths correlated the child as its own rename; jobs drain by
    /// id, so the folder move rewrote the child's rows first and the child move then burned
    /// five attempts into `failed` — a permanent sync error on a rename that succeeded.
    #[test]
    fn a_child_of_a_queued_folder_move_is_not_correlated_by_a_later_batch() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);

        std::fs::create_dir_all(root.join("e")).unwrap();
        std::fs::write(root.join("e/one.txt"), b"aaa").unwrap();
        let (hash, size, mtime) = crate::path_util::hash_file(&root.join("e/one.txt")).unwrap();
        state.db.upsert_known_folder("d").unwrap();
        state
            .db
            .upsert_local_file("d/one.txt", &hash, size, mtime)
            .unwrap();
        state
            .db
            .upsert_remote_file("d/one.txt", &hash, "rev1", mtime, Some("id:ONE"))
            .unwrap();

        process_changed_paths(&state, &["d".to_string(), "e".to_string()]).expect("batch 1");
        // FSEvents reports the child paths separately, routinely.
        process_changed_paths(&state, &["d/one.txt".to_string(), "e/one.txt".to_string()])
            .expect("batch 2");

        assert_eq!(
            move_jobs(&state),
            vec![("d".to_string(), "e".to_string())],
            "the folder move carries its children; a second move for a child fails and stays \
             failed, which the user sees as a permanent sync error"
        );
    }

    /// The `known_folders` filter in `enqueue_targeted_deletions`, which had no test.
    ///
    /// Every other test short-circuits before reaching it: the guard at the top of the
    /// function returns early whenever the reported path is itself covered. This one only
    /// matters when the reported path is **not** covered but a sub-folder is — a parent
    /// reported absent while a move is queued on a folder inside it. It is the last thing
    /// standing between that report and a recursive remote `delete_v2` of a folder whose
    /// contents are mid-relocation.
    #[test]
    fn a_parent_reported_absent_does_not_delete_a_subfolder_with_a_queued_move() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);

        // `parent/inner` is renamed to `parent/moved`, so a move is queued naming both.
        std::fs::create_dir_all(root.join("parent/moved")).unwrap();
        std::fs::write(root.join("parent/moved/one.txt"), b"aaa").unwrap();
        let (hash, size, mtime) =
            crate::path_util::hash_file(&root.join("parent/moved/one.txt")).unwrap();
        state.db.upsert_known_folder("parent").unwrap();
        state.db.upsert_known_folder("parent/inner").unwrap();
        // A sub-folder UNDER the move's destination. It is not a member of the move set by
        // name, so only the prefix predicate protects it — an exact match lets it through to
        // a recursive remote delete.
        state.db.upsert_known_folder("parent/moved/deep").unwrap();
        state
            .db
            .upsert_local_file("parent/inner/one.txt", &hash, size, mtime)
            .unwrap();
        state
            .db
            .upsert_remote_file("parent/inner/one.txt", &hash, "rev1", mtime, Some("id:ONE"))
            .unwrap();
        process_changed_paths(
            &state,
            &["parent/inner".to_string(), "parent/moved".to_string()],
        )
        .expect("batch 1");
        assert_eq!(
            move_jobs(&state),
            vec![("parent/inner".to_string(), "parent/moved".to_string())]
        );

        // Now the watcher reports the PARENT absent. `parent` itself has no queued move, so
        // the early return does not fire and the folder loop is reached.
        std::fs::remove_dir_all(root.join("parent")).ok();
        process_changed_paths(&state, &["parent".to_string()]).expect("batch 2");

        assert!(
            !job_targets(&state, "delete").contains(&"parent/inner".to_string()),
            "a folder with a queued move must not be recursively deleted on Dropbox"
        );
        // And its FILES are guarded separately, by the descendant filter rather than the
        // folder one. The test stopped one assertion short of that branch, which is why
        // mutating it left the suite green: every other test short-circuits before reaching
        // it, because the early return fires whenever the reported path is itself covered.
        assert!(
            !job_targets(&state, "delete").contains(&"parent/inner/one.txt".to_string()),
            "nor may its contents be deleted individually"
        );
        assert!(
            !job_targets(&state, "delete").contains(&"parent/moved/deep".to_string()),
            "nor a sub-folder under the destination, which delete_v2 would remove recursively"
        );
    }

    /// The prefix behaviour of the deletion guards, pinned.
    ///
    /// Every deletion test so far reported the **folder** absent, and a folder is an exact
    /// member of `active_move_paths` — so reverting those guards to `.contains()` left the
    /// suite green. The hazard they exist for is the other shape: a queued `move d → e` names
    /// only `d` and `e`, while every descendant is equally mid-relocation, and the watcher
    /// routinely reports descendants on their own.
    ///
    /// This is the test that distinguishes the prefix predicate from the exact match. Without
    /// it, reverting any of those guards — which this ticket has done by accident five times —
    /// says nothing.
    #[test]
    fn a_descendant_reported_absent_during_a_folder_move_is_not_deleted() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);

        std::fs::create_dir_all(root.join("e")).unwrap();
        std::fs::write(root.join("e/one.txt"), b"aaa").unwrap();
        let (hash, size, mtime) = crate::path_util::hash_file(&root.join("e/one.txt")).unwrap();
        state.db.upsert_known_folder("d").unwrap();
        state
            .db
            .upsert_local_file("d/one.txt", &hash, size, mtime)
            .unwrap();
        state
            .db
            .upsert_remote_file("d/one.txt", &hash, "rev1", mtime, Some("id:ONE"))
            .unwrap();

        process_changed_paths(&state, &["d".to_string(), "e".to_string()]).expect("batch 1");
        assert_eq!(move_jobs(&state), vec![("d".to_string(), "e".to_string())]);

        // A later batch reports only the DESCENDANT absent. It is not in the move set by
        // name — only its folder is — so an exact-match guard lets it through.
        process_changed_paths(&state, &["d/one.txt".to_string()]).expect("batch 2");

        assert!(
            job_targets(&state, "delete").is_empty(),
            "a descendant of a folder being moved is not a deletion"
        );
        assert!(
            state.db.get_local_file("d/one.txt").unwrap().is_some(),
            "and it keeps its row, and therefore its identity"
        );
    }

    /// The same shape for the full scan's deletion filters, which walk the whole tree rather
    /// than a reported batch. `:1170` was pinned; `:1180`, the folder half, was not.
    #[test]
    fn a_full_scan_during_a_folder_move_deletes_nothing_under_it() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);

        std::fs::create_dir_all(root.join("e")).unwrap();
        std::fs::write(root.join("e/one.txt"), b"aaa").unwrap();
        let (hash, size, mtime) = crate::path_util::hash_file(&root.join("e/one.txt")).unwrap();
        state.db.upsert_known_folder("d").unwrap();
        state.db.upsert_known_folder("d/inner").unwrap();
        state
            .db
            .upsert_local_file("d/one.txt", &hash, size, mtime)
            .unwrap();
        state
            .db
            .upsert_remote_file("d/one.txt", &hash, "rev1", mtime, Some("id:ONE"))
            .unwrap();

        process_changed_paths(&state, &["d".to_string(), "e".to_string()]).expect("correlate");
        scan_local_changes_only(&state).expect("scan");

        assert!(
            job_targets(&state, "delete").is_empty(),
            "neither the descendant file nor the sub-folder may be deleted mid-move"
        );
    }

    /// A vanished path with no counterpart is still a delete. The correlation must not be so
    /// eager that deleting a file looks like moving it somewhere unobserved.
    #[test]
    fn a_deletion_with_no_counterpart_is_still_a_deletion() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        state.db.upsert_local_file("gone.txt", "h", 5, 0).unwrap();

        let n = process_changed_paths(&state, &["gone.txt".to_string()]).expect("process");

        assert_eq!(n, 1);
        assert_eq!(job_targets(&state, "delete"), vec!["gone.txt".to_string()]);
        assert!(job_targets(&state, "move").is_empty());
    }

    /// An appeared file whose bytes match nothing that vanished is a new file, not a move.
    #[test]
    fn an_unrelated_new_file_is_not_mistaken_for_a_move() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);
        // Something did vanish in this batch — but with different content.
        state
            .db
            .upsert_local_file("gone.txt", "DIFFERENT", 5, 0)
            .unwrap();
        std::fs::write(root.join("fresh.txt"), b"hello").unwrap();

        process_changed_paths(&state, &["gone.txt".to_string(), "fresh.txt".to_string()])
            .expect("process");

        assert_eq!(job_targets(&state, "upload"), vec!["fresh.txt".to_string()]);
        assert_eq!(job_targets(&state, "delete"), vec!["gone.txt".to_string()]);
        assert!(job_targets(&state, "move").is_empty());
    }

    /// A rename must not orphan the records that point at the old name. `add_conflict` stores
    /// three paths per row and `enqueue_job` addresses its target by path, so both follow the
    /// item rather than being left behind pointing at nothing.
    #[test]
    fn a_conflict_record_follows_its_subject_through_a_rename() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);

        std::fs::write(root.join("new.txt"), b"hello").unwrap();
        let (hash, size, mtime) = crate::path_util::hash_file(&root.join("new.txt")).unwrap();
        state
            .db
            .upsert_local_file("old.txt", &hash, size, mtime)
            .unwrap();
        state
            .db
            .upsert_remote_file("old.txt", &hash, "rev1", mtime, Some("id:OLD"))
            .unwrap();
        state
            .db
            .add_conflict("old.txt", "old.txt", "unresolved", None, false)
            .unwrap();

        process_changed_paths(&state, &["old.txt".to_string(), "new.txt".to_string()])
            .expect("process");
        // The rewrite now happens when the server confirms the move, so drive it directly —
        // the network call around it is manual-QA-only.
        state
            .db
            .move_index_row("old.txt", "new.txt")
            .expect("confirm");

        let conflicts = state.db.list_recent_conflicts(10).expect("conflicts");
        assert_eq!(conflicts.len(), 1, "the record must not be dropped");
        assert_eq!(
            conflicts[0].local_path, "new.txt",
            "nor left pointing at a path that no longer exists"
        );
    }

    /// DBSYNC-96 measured a directory rename as one delete per tracked descendant, plus the
    /// folder row, plus a full upload each — **three** deletes for a two-file folder, not
    /// two, because the folder row goes as well. **DBSYNC-99 slice 4 inverts it.**
    ///
    /// **The assertion that matters is the job count.** A directory move implemented as a
    /// per-child loop reaches the same end state and would satisfy any test that only
    /// checked where the files ended up, while delivering none of the benefit — the whole
    /// point is that renaming a folder costs the same as renaming a file.
    #[test]
    fn a_directory_rename_is_billed_as_one_move() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);

        // New directory on disk with the two files in it.
        std::fs::create_dir_all(root.join("e")).unwrap();
        std::fs::write(root.join("e/one.txt"), b"aaa").unwrap();
        std::fs::write(root.join("e/two.txt"), b"bbb").unwrap();

        // The index holds all of it under the old name, and Dropbox has it.
        state.db.upsert_known_folder("d").unwrap();
        for (child, bytes) in [("one.txt", "aaa"), ("two.txt", "bbb")] {
            let (hash, size, mtime) =
                crate::path_util::hash_file(&root.join(format!("e/{child}"))).unwrap();
            let _ = bytes;
            let old_rel = format!("d/{child}");
            state
                .db
                .upsert_local_file(&old_rel, &hash, size, mtime)
                .unwrap();
            state
                .db
                .upsert_remote_file(&old_rel, &hash, "rev1", mtime, Some("id:CHILD"))
                .unwrap();
        }
        let identity = state
            .db
            .get_local_file("d/one.txt")
            .unwrap()
            .unwrap()
            .item_id
            .expect("identity");

        let n = process_changed_paths(
            &state,
            &[
                "d".to_string(),
                "e".to_string(),
                "e/one.txt".to_string(),
                "e/two.txt".to_string(),
            ],
        )
        .expect("process");

        // ONE job. Not one per child, and not one per child plus the folder.
        assert_eq!(n, 1, "a folder rename must cost what a file rename costs");
        assert_eq!(
            move_jobs(&state),
            vec![("d".to_string(), "e".to_string())],
            "one move, and it must name where the folder came FROM as well as where it went"
        );
        assert!(job_targets(&state, "delete").is_empty());
        assert!(
            job_targets(&state, "upload").is_empty(),
            "not one byte of the contents goes back up"
        );

        // The index has NOT moved yet, and that is the point: a move that Dropbox refuses
        // has nothing to undo, because nothing was written. The subtree rewrite happens in
        // `move_remote_file_internal` once the server confirms, and
        // `move_index_subtree_rewrites_the_subtree_and_leaves_siblings_alone` covers that it
        // travels whole when it does.
        assert_eq!(
            state.db.list_known_folders().unwrap(),
            vec!["d".to_string()]
        );
        assert_eq!(
            state
                .db
                .get_local_file("d/one.txt")
                .unwrap()
                .unwrap()
                .item_id,
            Some(identity)
        );
        assert!(state.db.get_local_file("e/one.txt").unwrap().is_none());
    }

    /// A directory whose tracked contents do NOT all turn up under the new name is not a
    /// rename that can be collapsed. Falling back costs bandwidth; guessing costs data.
    ///
    /// **Every other guard is deliberately satisfied here** so that only the completeness
    /// check can block the pairing: `e` is untracked, `one.txt` is byte-for-byte the size
    /// the index records, and both descendants have remote rows. An earlier version of this
    /// test seeded no remote rows at all, so `get_remote_file` blocked the correlation and
    /// the test passed with the guard it is named after removed — green for the wrong
    /// reason, caught by the DBSYNC-99 review.
    #[test]
    fn a_partial_directory_match_is_not_collapsed_into_a_move() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);

        std::fs::create_dir_all(root.join("e")).unwrap();
        std::fs::write(root.join("e/one.txt"), b"aaa").unwrap();

        state.db.upsert_known_folder("d").unwrap();
        state.db.upsert_local_file("d/one.txt", "h1", 3, 0).unwrap();
        // Tracked under the old name, but absent under the new one. This alone must stop it.
        state.db.upsert_local_file("d/two.txt", "h2", 3, 0).unwrap();
        state
            .db
            .upsert_remote_file("d/one.txt", "h1", "rev", 0, Some("id:ONE"))
            .unwrap();
        state
            .db
            .upsert_remote_file("d/two.txt", "h2", "rev", 0, Some("id:TWO"))
            .unwrap();

        process_changed_paths(
            &state,
            &["d".to_string(), "e".to_string(), "e/one.txt".to_string()],
        )
        .expect("process");

        assert!(
            job_targets(&state, "move").is_empty(),
            "an incomplete match must not be guessed at"
        );
        assert!(job_targets(&state, "delete").contains(&"d/two.txt".to_string()));
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

    #[test]
    fn delete_is_suppressed_when_path_is_a_cloud_only_placeholder() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let sync = tmp.path().join("synced");
        // A queued remote delete whose path now has a `.cloudsc` sidecar is a
        // dehydration (free up space), not a user deletion → suppress at drain time.
        // (The native CfAPI placeholder branch needs a real sync root, so it is
        // exercised by manual/integration testing; here we cover the `.cloudsc` case
        // and the genuine-deletion case that must still propagate.)
        std::fs::write(sync.join("foo.txt.cloudsc"), b"{}").unwrap();
        assert!(super::delete_suppressed_by_dehydration(&state, "foo.txt"));
        assert!(
            !super::delete_suppressed_by_dehydration(&state, "gone.txt"),
            "a genuinely deleted file (no placeholder) must still be deleted on remote"
        );
    }

    // ---- DBSYNC-65 (Slice 1): capture delete-time parent rev (plumbing only) ----

    #[test]
    fn file_deletion_captures_remote_rev_on_enqueue() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);
        // No `.cloudsc`/native placeholder at this path, so the enqueue path in
        // `process_local_file_deletion` is actually exercised (not short-circuited).
        state
            .db
            .upsert_remote_file("foo.txt", "hash", "rev123", 0, None)
            .expect("seed remote row");

        let n = super::process_local_file_deletion(&state, &root, "foo.txt").expect("process");
        assert_eq!(n, 1);

        let job = state
            .db
            .pick_next_due_job()
            .expect("pick job")
            .expect("job present");
        assert_eq!(job.job_type, "delete");
        assert_eq!(job.delete_parent_rev, Some("rev123".to_string()));
    }

    #[test]
    fn file_deletion_with_no_remote_index_row_enqueues_none_rev() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);
        // No `remote_file_index` row seeded for this path.

        let n = super::process_local_file_deletion(&state, &root, "gone.txt").expect("process");
        assert_eq!(n, 1);

        let job = state
            .db
            .pick_next_due_job()
            .expect("pick job")
            .expect("job present");
        assert_eq!(job.delete_parent_rev, None);
    }

    #[test]
    fn folder_deletion_always_enqueues_none_rev() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);
        // Seeded defensively at the folder's path — `process_known_folder_deletion`
        // must never consult `remote_file_index` (folders aren't keyed there).
        state
            .db
            .upsert_remote_file("dir", "hash", "rev999", 0, None)
            .expect("seed remote row");

        let n = super::process_known_folder_deletion(&state, &root, "dir").expect("process");
        assert_eq!(n, 1);

        let job = state
            .db
            .pick_next_due_job()
            .expect("pick job")
            .expect("job present");
        assert_eq!(job.job_type, "delete");
        assert_eq!(job.delete_parent_rev, None);
    }

    #[test]
    fn re_enqueuing_delete_refreshes_stale_parent_rev() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());

        state
            .db
            .enqueue_delete_job("foo.txt", Some("old_rev"))
            .expect("enqueue old");
        state
            .db
            .enqueue_delete_job("foo.txt", Some("new_rev"))
            .expect("enqueue new");

        let active: Vec<_> = state
            .db
            .list_recent_jobs(50)
            .expect("list jobs")
            .into_iter()
            .filter(|j| {
                j.job_type == "delete"
                    && matches!(j.status.as_str(), "queued" | "retry_wait" | "running")
            })
            .collect();
        assert_eq!(
            active.len(),
            1,
            "re-enqueuing the same delete target must collapse into one active job, not duplicate"
        );
        assert_eq!(active[0].delete_parent_rev, Some("new_rev".to_string()));
    }

    // ---- DBSYNC-55: editor-temp / vanished-source handling ----

    fn last_error(state: &AppState) -> Option<String> {
        state
            .sync_engine
            .lock()
            .unwrap()
            .current_status(false)
            .last_error
    }

    fn delete_jobs(state: &AppState) -> Vec<String> {
        state
            .db
            .list_recent_jobs(50)
            .unwrap()
            .into_iter()
            .filter(|j| j.job_type == "delete")
            .filter_map(|j| j.target_path)
            .collect()
    }

    /// DBSYNC-99. Build a refused file move exactly as production does, then settle it.
    ///
    /// Returns `(upload_job_id, content_hash)`. Nothing here writes a
    /// `local_file_index` row for the destination, **because the pipeline does not**: the
    /// correlator pairs only a destination the local index lacks, and pass three skips
    /// `moved_to` paths. Two earlier tests manufactured that row by hand and so asserted a
    /// world that never occurs — the gate they were defending could not pass in production
    /// and the whole mechanism was inert. Setup goes through `process_changed_paths`.
    fn refused_file_move(root: &std::path::Path, state: &AppState) -> (i64, String) {
        std::fs::write(root.join("new.txt"), b"hello").unwrap();
        let (hash, size, mtime) = crate::path_util::hash_file(&root.join("new.txt")).unwrap();
        state
            .db
            .upsert_local_file("old.txt", &hash, size, mtime)
            .unwrap();
        state
            .db
            .upsert_remote_file("old.txt", &hash, "rev1", mtime, Some("id:OLD"))
            .unwrap();

        process_changed_paths(
            &state.clone(),
            &["old.txt".to_string(), "new.txt".to_string()],
        )
        .expect("process");
        assert_eq!(
            move_jobs(state),
            vec![("old.txt".to_string(), "new.txt".to_string())],
            "precondition: the scan enqueued a move"
        );
        assert!(
            state.db.get_local_file("new.txt").unwrap().is_none(),
            "precondition: the scan does NOT index the destination — this is the fact the \
             hand-written setup used to paper over"
        );

        // Dropbox refuses it permanently.
        crate::dropbox_transfer::rederive_refused_move(state, "old.txt", "new.txt", true).unwrap();
        let upload_id = state
            .db
            .list_recent_jobs(50)
            .unwrap()
            .into_iter()
            .find(|j| j.job_type == "upload")
            .expect("the destination is queued for upload")
            .id;
        (upload_id, hash)
    }

    /// The falsifier for the defect that made the deferred deletion inert: with the bytes
    /// genuinely at the destination, the source must be deleted.
    #[test]
    fn a_landed_upload_settles_the_refused_moves_source() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);
        let (upload_id, hash) = refused_file_move(&root, &state);

        // The upload lands. `record_upload_result` writes the remote row and NOTHING else —
        // no local row — so this is the whole of what success leaves behind.
        state
            .db
            .upsert_remote_file("new.txt", &hash, "rev2", 0, Some("id:OLD"))
            .unwrap();

        crate::dropbox_transfer::settle_owed_source_deletion(&state, upload_id, "new.txt");

        assert_eq!(
            delete_jobs(&state),
            vec!["old.txt".to_string()],
            "the bytes ARE at the destination on Dropbox, so the source must be deleted"
        );
        let delete = state
            .db
            .list_recent_jobs(50)
            .unwrap()
            .into_iter()
            .find(|j| j.job_type == "delete")
            .unwrap();
        assert_eq!(
            delete.delete_parent_rev.as_deref(),
            Some("rev1"),
            "carrying the source's pre-move rev, so `delete_v2` still notices a server-side \
             change in the meantime"
        );

        // Settled means the debt is DISCHARGED, not merely that a job appeared once. Without
        // this, removing the `clear` call entirely left the suite green — coverage of `clear`
        // had migrated to the db method while the mechanism went unpinned.
        assert_eq!(
            state.db.peek_deferred_source_delete(upload_id).unwrap(),
            None,
            "the debt must be cleared once the delete job exists"
        );
        state.db.mark_job_completed(delete.id).unwrap();
        crate::dropbox_transfer::settle_owed_source_deletion(&state, upload_id, "new.txt");
        assert_eq!(
            delete_jobs(&state).len(),
            1,
            "and a second settle cannot enqueue the same deletion again"
        );
    }

    /// And with nothing at the destination, the source is the only copy and must survive.
    #[test]
    fn an_upload_that_did_not_land_leaves_the_refused_moves_source_alone() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);
        let (upload_id, _) = refused_file_move(&root, &state);

        // No remote row for the destination: the upload no-oped (a vanished source, a
        // `.cloudsc` path) and put nothing on Dropbox.
        crate::dropbox_transfer::settle_owed_source_deletion(&state, upload_id, "new.txt");

        assert!(
            delete_jobs(&state).is_empty(),
            "Dropbox holds nothing at the destination — deleting the source removes the last copy"
        );
        assert_eq!(
            state
                .db
                .peek_deferred_source_delete(upload_id)
                .unwrap()
                .map(|(p, _)| p),
            Some("old.txt".to_string()),
            "and the debt survives the decline: reading it used to destroy it on first sight"
        );
        // The queue marks the job done right after settling, exactly as production does.
        state.db.mark_job_completed(upload_id).unwrap();

        // The notice is available immediately, with no hand-written row: `rederive_refused_
        // move` keeps the source's REMOTE row precisely so this precondition is real.
        //
        // The previous version of this test wrote that row itself, under a comment claiming
        // "the remote sweep re-indexes it". The sweep cannot: it is driven from the local
        // index, which no longer holds the path. So the notice could never fire in production
        // and this test passed only because it manufactured the state — the exact failure this
        // module's own helper exists to stop.
        super::refresh_queue_depth_internal(&state).expect("refresh");

        // Withholding is correct and it does not heal itself, so it cannot be a log line only.
        // It is deliberately NOT a conflict row: `resolve_conflict_internal` has no arm that
        // removes a stray remote path, so the buttons would be no-ops that mark it resolved.
        assert!(
            state
                .db
                .list_unresolved_conflict_local_paths()
                .unwrap()
                .is_empty(),
            "no conflict row: a conflict promises a resolution this shape does not have, and \
             telling the user they fixed it is worse than the log line"
        );
        let err = last_error(&state).expect("the user must be told the rename did not complete");
        assert!(
            err.contains("old.txt") && err.contains("new.txt"),
            "and told WHICH paths, or they cannot act on it: {err}"
        );
        // **The message must never recommend a deletion.** Here the destination was never put
        // on Dropbox, so `old.txt` is the user's only copy; the first version of this notice
        // said "delete 'old.txt' by hand".
        assert!(
            !err.to_lowercase().contains("delete"),
            "this must not instruct a deletion — the app itself refused to perform it: {err}"
        );
        assert!(
            err.contains("still holds it under the old name"),
            "it must say what is actually true of Dropbox right now: {err}"
        );

        // And it is durable: the next tick recomputes `last_error` from scratch and clears
        // anything transient. A bare `set_last_error` — the first thing I reached for — would
        // be gone here.
        super::refresh_queue_depth_internal(&state).expect("second refresh");
        assert!(
            last_error(&state).is_some_and(|e| e.contains("old.txt")),
            "the notice must survive a recompute, or the user sees it for one tick"
        );
    }

    /// The index fast path must compare hashes, not merely find rows.
    ///
    /// This site had a test and this commit's predecessor deleted it: the replacement
    /// deliberately has no local row and so exercises the **disk** branch instead. Two
    /// near-identical names, two different sites — "the `to/conflict` case is covered" was
    /// true of the group and false of this one, and substituting `local.hash == local.hash`
    /// left the whole suite green.
    ///
    /// The local row is production-built: once the move job is gone, an ordinary scan indexes
    /// the destination like any other new file.
    #[test]
    fn an_indexed_destination_with_a_different_remote_hash_withholds() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);
        let (upload_id, _) = refused_file_move(&root, &state);

        let move_id = state
            .db
            .list_recent_jobs(50)
            .unwrap()
            .into_iter()
            .find(|j| j.job_type == "move")
            .unwrap()
            .id;
        state.db.mark_job_completed(move_id).unwrap();
        process_changed_paths(&state, &["new.txt".to_string()]).expect("scan indexes it");
        assert!(
            state.db.get_local_file("new.txt").unwrap().is_some(),
            "precondition: the scan gave the destination a local row"
        );

        // Dropbox holds something else entirely at that name — the `to/conflict` shape.
        state
            .db
            .upsert_remote_file("new.txt", "SOMEONE-ELSES-BYTES", "revX", 0, None)
            .unwrap();

        crate::dropbox_transfer::settle_owed_source_deletion(&state, upload_id, "new.txt");

        assert!(
            delete_jobs(&state).is_empty(),
            "rows on both sides is not agreement; deleting the source here loses the user's bytes"
        );
    }

    /// A destination that cannot be read is not a destination that matches.
    ///
    /// The disk fallback's `Err` arm was unpinned: making it return `Ok(true)` left 265 tests
    /// green while an unreadable destination — vanished, locked, permission denied — licensed
    /// deleting the source. That is the data-loss shape the comment above it forbids.
    #[test]
    fn an_unreadable_destination_withholds_the_deletion() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);
        let (upload_id, hash) = refused_file_move(&root, &state);

        // Dropbox reports the bytes are there...
        state
            .db
            .upsert_remote_file("new.txt", &hash, "rev2", 0, None)
            .unwrap();
        // ...but the destination is gone from disk, so nothing can confirm it.
        std::fs::remove_file(root.join("new.txt")).unwrap();
        assert!(
            state.db.get_local_file("new.txt").unwrap().is_none(),
            "precondition: no local row, so the check must reach the disk"
        );

        crate::dropbox_transfer::settle_owed_source_deletion(&state, upload_id, "new.txt");

        assert!(
            delete_jobs(&state).is_empty(),
            "unreadable is not proof of a match — withhold, because a duplicate is recoverable \
             and a wrong deletion is not"
        );
    }

    /// An upload must never overwrite content this client has never indexed.
    ///
    /// Every upload sends `mode: overwrite`, and the identical-content guard is gated on a
    /// local index row — so a path with no row, which is precisely one we have never seen,
    /// skipped the guard entirely. Reached through the `to/conflict` recovery: a collaborator
    /// creates a file, the user renames onto that name before the delta lands, `move_v2` is
    /// refused, and the recovery uploaded over another person's file with no conflict row and
    /// no log line.
    ///
    /// The destination here is `.cloudsc`, which returns `Ok(())` at the top of
    /// `upload_local_file_internal` before any network call — so this test cannot reach the
    /// guard. It is here to state that gap explicitly rather than leave it implied: the guard
    /// itself is pinned by `an_upload_over_unknown_remote_content_is_refused` below, which
    /// calls the DB-level precondition directly.
    #[test]
    fn a_path_with_no_local_row_is_the_state_the_overwrite_guard_defends() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);
        let (_upload_id, _) = refused_file_move(&root, &state);

        // This is the precondition the guard keys on, and `refused_file_move` asserts the
        // pipeline really produces it.
        assert!(
            state.db.get_local_file("new.txt").unwrap().is_none(),
            "a refused move's destination has no local row — which is what made the \
             identical-content guard structurally skippable"
        );
    }

    /// The other message branch: Dropbox is observed to hold BOTH names.
    ///
    /// Only the "could not be removed" wording was pinned; forcing `destination_on_dropbox`
    /// false left the whole suite green, so this branch — the `to/conflict` case, which is the
    /// marker that sends most moves down this path — was never exercised.
    ///
    /// The wording claims presence and never equivalence: a remote row at the destination can
    /// hold somebody else's content, so "both names" is true while "a spare copy" is not.
    #[test]
    fn a_destination_present_on_dropbox_says_both_names_exist() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);
        let (upload_id, _) = refused_file_move(&root, &state);
        state.db.mark_job_completed(upload_id).unwrap();
        // Dropbox holds the destination too, under different content — the `to/conflict` shape.
        state
            .db
            .upsert_remote_file("new.txt", "OTHER-BYTES", "revX", 0, None)
            .unwrap();

        super::refresh_queue_depth_internal(&state).expect("refresh");

        let err = last_error(&state).expect("the user must be told");
        assert!(
            err.contains("Dropbox holds both names"),
            "the destination IS on Dropbox, so the wording must say so: {err}"
        );
        assert!(
            !err.contains("could not be removed from Dropbox. Dropbox still holds it under"),
            "and must not use the no-knowledge wording: {err}"
        );
        assert!(
            !err.to_lowercase().contains("delete"),
            "and still recommends nothing: {err}"
        );
    }

    /// A dead upload surfaces its OWN error, not the advisory — and that ordering is the fix.
    ///
    /// The stranded notice used to outrank `latest_failed_error`, and since nothing ever
    /// clears the debt on a terminal row (no caller outside settle's success arm, `sync_jobs`
    /// never pruned of `done`/`failed`, `requeue_failed_jobs` touching only `failed`), it
    /// masked every real failure after it — an expired token, a full disk — for the life of
    /// the database. A job failure is actionable and clears itself when acted on; an advisory
    /// is not, so it must never be the thing hiding one.
    #[test]
    fn a_dead_upload_surfaces_its_own_error_not_the_advisory() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());

        // An upload whose path escapes the sync root fails in `safe_join`, which
        // `upload_local_file_internal` reaches BEFORE `get_access_token` — so this drains to
        // permanent failure deterministically, with no network and no keychain.
        assert!(state
            .db
            .enqueue_upload_then_delete("../escaped.txt", "old.txt", Some("rev1"))
            .unwrap());
        let upload_id = state
            .db
            .list_recent_jobs(10)
            .unwrap()
            .into_iter()
            .find(|j| j.job_type == "upload")
            .unwrap()
            .id;
        // Last attempt, so the next drain takes the max-attempts branch.
        state
            .db
            .mark_job_retry_wait(upload_id, 4, &Utc::now().to_rfc3339(), Some("previous"))
            .unwrap();

        assert!(super::process_sync_queue_internal(&state).expect("drain"));

        assert_eq!(
            state
                .db
                .list_recent_jobs(10)
                .unwrap()
                .into_iter()
                .find(|j| j.id == upload_id)
                .unwrap()
                .status,
            "failed",
            "precondition: the upload really died rather than backing off again"
        );

        // Dropbox is observed to still hold the source, so the advisory is available...
        state
            .db
            .upsert_remote_file("old.txt", "H", "rev1", 0, Some("id:OLD"))
            .unwrap();
        assert!(
            state.db.unsettled_source_deletion().unwrap().is_some(),
            "precondition: the debt is stranded on a terminal row and would be reportable"
        );

        super::refresh_queue_depth_internal(&state).expect("refresh");
        let err = last_error(&state).expect("something must reach the user");
        // Discriminating, not co-varying: the destination in this test is `../escaped.txt`,
        // so BOTH messages contain "escaped.txt" and asserting that proves nothing. The
        // advisory's distinctive phrase is "was renamed to"; the job error's is the failure
        // reason itself.
        assert!(
            err.contains("rejected unsafe relative path"),
            "...but the actionable job error is what surfaces: {err}"
        );
        assert!(
            !err.contains("was renamed to"),
            "the advisory must NOT be what the user sees while a real failure stands: {err}"
        );
    }

    /// `to/conflict` is the marker that sends most moves here, and it means the destination is
    /// already TAKEN on Dropbox. A remote row can therefore exist without this upload having
    /// done anything, so presence is not the question — content is.
    #[test]
    fn a_destination_holding_other_content_does_not_license_the_deletion() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);
        let (upload_id, _) = refused_file_move(&root, &state);

        state
            .db
            .upsert_remote_file(
                "new.txt",
                "SOMEONE-ELSES-BYTES",
                "revX",
                0,
                Some("id:OTHER"),
            )
            .unwrap();

        crate::dropbox_transfer::settle_owed_source_deletion(&state, upload_id, "new.txt");

        assert!(
            delete_jobs(&state).is_empty(),
            "a destination that merely EXISTS on Dropbox is not the destination holding the \
             user's bytes"
        );
    }

    /// The queue's success arm must actually call the settle, with the job's own path.
    ///
    /// I claimed this wiring could not be tested without the network. It can:
    /// `upload_local_file_internal` returns `Ok(())` for a `.cloudsc` path at its very first
    /// line, before touching auth, disk or the database, so the success arm is reachable for
    /// free. Asserting the claim instead of spending ten minutes disproving it is what let the
    /// defect above ship.
    #[test]
    fn the_queues_success_arm_settles_the_debt_it_owes() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());

        let root = sync_root(&state);
        assert!(state
            .db
            .enqueue_upload_then_delete("new.txt.cloudsc", "old.txt", Some("rev1"))
            .unwrap());
        // The destination genuinely holds the bytes, established the way production does it:
        // a real file on disk plus the remote row `record_upload_result` would write. An
        // earlier version wrote a `local_file_index` row for this path instead — which the
        // scan never creates for a move destination, and doubly never for a `.cloudsc` path,
        // since `process_changed_paths` skips those outright. That is the same hand-built
        // state this module's own helper exists to stop using.
        std::fs::write(root.join("new.txt.cloudsc"), b"hello").unwrap();
        let (hash, _, _) = crate::path_util::hash_file(&root.join("new.txt.cloudsc")).unwrap();
        state
            .db
            .upsert_remote_file("new.txt.cloudsc", &hash, "rev2", 0, None)
            .unwrap();
        assert!(
            state
                .db
                .get_local_file("new.txt.cloudsc")
                .unwrap()
                .is_none(),
            "precondition: no manufactured local row — the gate must reach the disk"
        );

        assert!(super::process_sync_queue_internal(&state).expect("drain"));

        // What this pins is that the success arm calls settle at all. It does NOT pin which
        // field the destination is read from: `enqueue_upload_then_delete` inserts
        // `VALUES('upload', ?1, ?1, …)`, so `source_path` and `target_path` are equal by
        // construction for every row this mechanism creates, and an assertion here cannot
        // tell them apart. The earlier version of this test claimed it could.
        assert_eq!(
            delete_jobs(&state),
            vec!["old.txt".to_string()],
            "the upload's success arm must settle the debt it owes"
        );
    }

    /// A refused **folder** move must converge. Refusing to recover it is right; refusing
    /// silently is what left the user's data unsynced.
    ///
    /// Declining writes nothing, so the correlator's inputs were byte-for-byte identical on
    /// the next scan and it proposed the same pair again — a live `move_v2` per tick forever.
    /// And the pair is what suppresses the fallback: `under_moved_dir` skips every path below
    /// a paired destination, so the rename never reached Dropbox and **every later edit under
    /// the renamed folder stopped being uploaded**, with the UI reading `synced`. The doc
    /// comment called this "exactly what `main` does"; `main` has no directory correlation at
    /// all, so there the fallback runs and converges.
    #[test]
    fn a_refused_folder_move_falls_back_instead_of_looping() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);

        std::fs::create_dir_all(root.join("Papers")).unwrap();
        std::fs::write(root.join("Papers/one.txt"), b"hello").unwrap();
        let (hash, size, mtime) =
            crate::path_util::hash_file(&root.join("Papers/one.txt")).unwrap();
        state.db.upsert_known_folder("Docs").unwrap();
        state
            .db
            .upsert_local_file("Docs/one.txt", &hash, size, mtime)
            .unwrap();
        state
            .db
            .upsert_remote_file("Docs/one.txt", &hash, "rev1", mtime, Some("id:ONE"))
            .unwrap();

        let paths = ["Docs".to_string(), "Papers".to_string()];
        process_changed_paths(&state, &paths).expect("first pass");
        assert_eq!(
            move_jobs(&state),
            vec![("Docs".to_string(), "Papers".to_string())],
            "precondition: the first scan pairs the folder rename"
        );

        // Dropbox refuses it permanently. `cant_move_shared_folder` is the marker named here,
        // but note it has never been observed live: manual QA renamed an owned shared folder
        // and Dropbox performed the move. The refusal shape is what this test pins; which
        // marker produces it is unverified. The comment this replaced said renaming a shared
        // folder produces every time — and the job completes.
        crate::dropbox_transfer::rederive_refused_move(&state, "Docs", "Papers", true).unwrap();
        let move_id = state
            .db
            .list_recent_jobs(50)
            .unwrap()
            .into_iter()
            .find(|j| j.job_type == "move")
            .unwrap()
            .id;
        state.db.mark_job_completed(move_id).unwrap();

        process_changed_paths(&state, &paths).expect("second pass");

        assert_eq!(
            move_jobs(&state).len(),
            1,
            "the refused pair must not be proposed a second time: {:?}",
            move_jobs(&state)
        );
        let mut deletes = delete_jobs(&state);
        deletes.sort();
        assert_eq!(
            deletes,
            vec!["Docs".to_string(), "Docs/one.txt".to_string()],
            "the ordinary fallback must run — the child AND the emptied folder, which is what \
             `main` does for a removed directory and what the loop was suppressing"
        );
        assert_eq!(
            job_targets(&state, "upload"),
            vec!["Papers/one.txt".to_string()],
            "and the child must be uploaded under the new name, or the rename never reaches \
             Dropbox and later edits are never backed up"
        );
    }

    /// A second refused move onto one destination must not loop either.
    ///
    /// The early return that keeps the declined source's index rows — correct, or it becomes
    /// an orphan on Dropbox — reproduced the folder loop at file granularity: the correlator's
    /// inputs stay identical, so it re-proposed the same pair on every scan, one live
    /// `move_v2` per tick, and the pair suppressed the fallback through `moved_from`.
    /// `refused_moves` covered only directories, and `correlate_renames` did not read it.
    #[test]
    fn a_declined_second_refusal_is_not_re_proposed() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);

        std::fs::write(root.join("x.txt"), b"hello").unwrap();
        let (hash, size, mtime) = crate::path_util::hash_file(&root.join("x.txt")).unwrap();
        for src in ["a.txt", "b.txt"] {
            state.db.upsert_local_file(src, &hash, size, mtime).unwrap();
            state
                .db
                .upsert_remote_file(src, &hash, "rev1", mtime, Some("id:X"))
                .unwrap();
        }

        // Both refused onto the same destination. The second is declined, keeping its rows.
        crate::dropbox_transfer::rederive_refused_move(&state, "a.txt", "x.txt", true).unwrap();
        crate::dropbox_transfer::rederive_refused_move(&state, "b.txt", "x.txt", true).unwrap();
        assert!(
            state.db.get_local_file("b.txt").unwrap().is_some(),
            "precondition: the declined source keeps its rows"
        );

        let before = move_jobs(&state).len();
        process_changed_paths(&state, &["a.txt".into(), "b.txt".into(), "x.txt".into()])
            .expect("scan");

        assert_eq!(
            move_jobs(&state).len(),
            before,
            "the refused pair must not be proposed again: {:?}",
            move_jobs(&state)
        );
    }

    /// A source refused for ONE destination must still pair with another.
    ///
    /// The check used to inspect only `candidates.last()` and give up on the whole destination
    /// if that one was refused. With two sources sharing a content hash and only one pair ever
    /// refused, the other — a pair nobody has refused — was abandoned, and both sources were
    /// deleted remotely while the destination went back up in full: the exact re-upload this
    /// ticket exists to prevent, caused by the fix for a different defect.
    #[test]
    fn a_refusal_for_one_destination_does_not_block_another_candidate() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);

        // Two tracked sources with identical content, one present destination.
        std::fs::write(root.join("x.txt"), b"hello").unwrap();
        let (hash, size, mtime) = crate::path_util::hash_file(&root.join("x.txt")).unwrap();
        for src in ["a.txt", "b.txt"] {
            state.db.upsert_local_file(src, &hash, size, mtime).unwrap();
            state
                .db
                .upsert_remote_file(src, &hash, "rev1", mtime, Some("id:X"))
                .unwrap();
        }
        // Dropbox has refused exactly one of the two possible pairs.
        state.db.record_refused_move("b.txt", "x.txt").unwrap();

        process_changed_paths(&state, &["a.txt".into(), "b.txt".into(), "x.txt".into()])
            .expect("scan");

        assert_eq!(
            move_jobs(&state),
            vec![("a.txt".to_string(), "x.txt".to_string())],
            "the unrefused pair must still form — abandoning it costs the full re-upload"
        );
        assert!(
            job_targets(&state, "upload").is_empty(),
            "and nothing goes back up"
        );
    }

    /// The advisory must not fire while the upload is still alive and healthy.
    ///
    /// The terminal-status clause is the whole meaning of "an upload that will never run
    /// again". Without it the notice appears the instant a move is refused — while the upload
    /// is `queued` and about to succeed — telling the user their Dropbox holds two copies
    /// when it holds one.
    #[test]
    fn no_advisory_while_the_upload_is_still_queued() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);
        refused_file_move(&root, &state);
        // Dropbox is observed to still hold the source.
        state
            .db
            .upsert_remote_file("old.txt", "H", "rev1", 0, Some("id:OLD"))
            .unwrap();

        assert!(
            state.db.unsettled_source_deletion().unwrap().is_none(),
            "the upload is queued and owes a deletion it is perfectly likely to settle"
        );
        super::refresh_queue_depth_internal(&state).expect("refresh");
        assert!(last_error(&state).is_none());
    }

    /// A halted sync outranks the advisory: nothing is moving at all, which is the more
    /// urgent thing to say.
    #[test]
    fn the_mass_deletion_pause_outranks_the_advisory() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);
        let (upload_id, _) = refused_file_move(&root, &state);
        state.db.mark_job_completed(upload_id).unwrap();
        state
            .db
            .upsert_remote_file("old.txt", "H", "rev1", 0, Some("id:OLD"))
            .unwrap();
        state
            .db
            .set_app_config(
                super::MASS_DELETE_BLOCKED_SCAN_KEY,
                "sync paused: mass deletion blocked",
            )
            .unwrap();

        super::refresh_queue_depth_internal(&state).expect("refresh");

        assert!(
            last_error(&state).is_some_and(|e| e.contains("mass deletion")),
            "the pause must win: sync is stopped entirely"
        );
    }

    /// A cloud-only destination is not hashed, and therefore licenses nothing.
    ///
    /// I claimed this could not be pinned on macOS because `is_dehydrated_placeholder` is
    /// `#[cfg(windows)]`. That was wrong twice over: the guard should check the legacy
    /// `.cloudsc` sidecar too — `delete_suppressed_by_dehydration` does, and the guard's own
    /// comment cited it as precedent while being weaker than it — and the sidecar half is
    /// platform-independent, so adding it makes the guard both correct and testable here.
    ///
    /// The remote hash deliberately MATCHES, so only the guard can withhold the deletion.
    #[test]
    fn a_cloud_only_destination_withholds_the_deletion() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);
        let (upload_id, hash) = refused_file_move(&root, &state);
        state
            .db
            .upsert_remote_file("new.txt", &hash, "rev2", 0, None)
            .unwrap();
        std::fs::write(root.join("new.txt.cloudsc"), b"").unwrap();

        crate::dropbox_transfer::settle_owed_source_deletion(&state, upload_id, "new.txt");

        assert!(
            delete_jobs(&state).is_empty(),
            "the destination is cloud-only; hashing it would trigger a download and its bytes \
             are not local evidence of anything"
        );
    }

    /// A refusal is only worth remembering while its source folder is still tracked — that is
    /// the only state in which the correlator could propose the pair again. Kept forever, it
    /// would make a genuine later rename of that same pair fall back to delete-plus-upload
    /// for nothing, which is the cost this ticket exists to remove.
    #[test]
    fn a_refusal_is_forgotten_once_its_folder_is_gone() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        state.db.upsert_known_folder("Docs").unwrap();
        state.db.record_refused_move("Docs", "Papers").unwrap();

        // The fallback converged: `Docs` is no longer a folder the index tracks.
        state.db.remove_known_folder("Docs").unwrap();
        process_changed_paths(&state, &[]).expect("process");

        assert!(
            state.db.list_refused_moves().unwrap().is_empty(),
            "the refusal outlived the folder it was about"
        );
    }

    /// A refusal recorded for a directory found STRUCTURALLY must survive the prune.
    ///
    /// The detector identifies a directory by rows under `from_path + "/"`; the prune used to
    /// identify "still tracked" by exact membership. For a folder with no `known_folders` row
    /// — the case the structural detector exists for — every clause was satisfied and the
    /// entry was deleted on the very next tick. The refusal that `rederive_refused_move` calls
    /// "not bookkeeping, the whole fix" lived for one scan.
    #[test]
    fn a_structurally_detected_directorys_refusal_survives_the_prune() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        // No `known_folders` row — only children.
        state.db.upsert_local_file("Docs/a.txt", "H", 5, 0).unwrap();
        state
            .db
            .upsert_remote_file("Docs/a.txt", "H", "rev1", 0, Some("id:A"))
            .unwrap();

        crate::dropbox_transfer::rederive_refused_move(&state, "Docs", "Papers", true).unwrap();
        assert_eq!(
            state.db.list_refused_moves().unwrap().len(),
            1,
            "precondition: the structural detector took the directory branch and recorded it"
        );

        state.db.prune_stale_refused_moves().unwrap();

        assert_eq!(
            state.db.list_refused_moves().unwrap().len(),
            1,
            "the prune must use the same notion of 'tracked' the detector does, or the record \
             is gone before the next scan can read it"
        );
    }

    /// ...but not while the work it caused is still in flight.
    ///
    /// Being untracked means the fallback has **started**, not finished: at that moment its
    /// deletes and its upload are still queued and can still fail, be dropped by
    /// `delete_suppressed_by_dehydration`, or exhaust their attempts. If anything then puts
    /// the source back — the remote sweep re-seeding a folder Dropbox still holds because the
    /// delete failed — forgetting the refusal re-arms the entire cycle.
    #[test]
    fn a_refusal_outlives_the_fallback_it_caused() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        state.db.record_refused_move("Docs", "Papers").unwrap();
        // Untracked in both indexes — the old predicate would prune here...
        state.db.enqueue_delete_job("Docs", None).unwrap();

        state.db.prune_stale_refused_moves().unwrap();

        assert_eq!(
            state.db.list_refused_moves().unwrap().len(),
            1,
            "...but a queued job still names it, so the fallback has not converged yet"
        );
    }

    /// A path that becomes a directory loses its tracked-file row immediately.
    ///
    /// `upsert_known_folder` did not clear it, so a file replaced on disk by a directory of
    /// the same name kept its `local_file_index` row until the next full scan — up to five
    /// minutes, and suppressible by the mass-deletion breaker. Anything asking "is this a
    /// file?" got the stale answer yes, which is what let a real folder move take the file
    /// branch in `rederive_refused_move` and owe a recursive delete.
    #[test]
    fn a_path_that_became_a_directory_loses_its_file_row() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);
        // Tracked as a file...
        state.db.upsert_local_file("Notes", "H", 5, 0).unwrap();
        // ...and now a directory on disk under the same name.
        std::fs::create_dir_all(root.join("Notes")).unwrap();
        std::fs::write(root.join("Notes/inner.txt"), b"hi").unwrap();

        process_changed_paths(&state, &["Notes".to_string()]).expect("scan");

        assert!(
            state.db.get_local_file("Notes").unwrap().is_none(),
            "the stale file row must go the moment the path is known to be a directory"
        );
        // And it must go as the DELETION it implies, not be silently dropped. Dropbox cannot
        // hold a file and a folder at the same name, so without this the child uploads into
        // the new folder are rejected five times and then fail permanently — and dropping the
        // row removed the full-scan pass that used to free the path.
        assert_eq!(
            delete_jobs(&state),
            vec!["Notes".to_string()],
            "the old remote file must be queued for deletion, or the new subtree can never sync"
        );
        assert!(state
            .db
            .list_known_folders()
            .unwrap()
            .contains(&"Notes".to_string()));
    }

    /// The `known_folders` clause must be prefix-aware too, not only `local_file_index`'s.
    ///
    /// Both halves exist and only one was pinned: narrowing the `local_file_index` clause was
    /// killed, narrowing this one left the whole suite green. A refused parent folder whose
    /// only surviving trace is a tracked SUBfolder would have its refusal pruned, and the
    /// correlator would re-propose the pair it had just been refused.
    #[test]
    fn a_refusal_survives_on_a_tracked_subfolder_alone() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        state.db.record_refused_move("Docs", "Papers").unwrap();
        // Nothing at `Docs` itself in either index — only a folder row beneath it.
        state.db.upsert_known_folder("Docs/Sub").unwrap();

        state.db.prune_stale_refused_moves().unwrap();

        assert_eq!(
            state.db.list_refused_moves().unwrap().len(),
            1,
            "a tracked subfolder is still a trace of the refused parent"
        );
    }

    /// The `known_folders` clause's EXACT half, which is the one that fires for an ordinary
    /// refused folder move.
    ///
    /// I added `a_refusal_survives_on_a_tracked_subfolder_alone` last round with the doc
    /// "both halves exist and only one was pinned" — and the same defect was one level down
    /// inside the block it fixed: that clause is itself a two-way `||`, and only its PREFIX
    /// half was pinned. The other test seeds a folder row and then removes it, so it only
    /// exercises the negative direction.
    #[test]
    fn a_refusal_survives_on_the_folder_row_itself() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        state.db.record_refused_move("Docs", "Papers").unwrap();
        // The folder row at the exact path, and nothing beneath it in either index.
        state.db.upsert_known_folder("Docs").unwrap();

        state.db.prune_stale_refused_moves().unwrap();

        assert_eq!(
            state.db.list_refused_moves().unwrap().len(),
            1,
            "the folder is still tracked, so the correlator can still propose the pair"
        );
    }

    /// A database read error while describing a stranded rename must take the branch that
    /// asserts least — it must not tell the user Dropbox holds both names.
    #[test]
    fn an_unreadable_destination_row_does_not_assert_presence() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let root = sync_root(&state);
        let (upload_id, _) = refused_file_move(&root, &state);
        state.db.mark_job_completed(upload_id).unwrap();

        // No remote row for the destination: the same state a read error resolves to, since
        // the `Err` arm is specified to behave as "not on Dropbox".
        super::refresh_queue_depth_internal(&state).expect("refresh");

        let err = last_error(&state).expect("the user must be told");
        assert!(
            !err.contains("Dropbox holds both names"),
            "absence of knowledge is not presence: {err}"
        );
    }

    /// Prefix matching is BINARY and needs no escaping — two properties in one test, because
    /// the same choice of operator broke both.
    ///
    /// `LIKE` is ASCII case-insensitive in SQLite while `=` is BINARY, so the exact and prefix
    /// halves disagreed on case and an unrelated `docs/...` retained a refusal for `Docs`.
    /// `COLLATE BINARY` does not fix that — collations are ignored by `LIKE`, measured. And
    /// `LIKE` needed `%`/`_`/`!` escaping that nothing tested. `substr` answers both.
    #[test]
    fn prefix_matching_is_case_sensitive_and_needs_no_escaping() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        state.db.record_refused_move("Docs", "Papers").unwrap();
        state.db.record_refused_move("a%b", "c").unwrap();
        state.db.record_refused_move("My_Files", "Other").unwrap();

        // Traces that must NOT retain anything: differing only in case, and paths a wildcard
        // would have matched but a literal prefix does not.
        state
            .db
            .upsert_local_file("docs/unrelated.txt", "H", 1, 0)
            .unwrap();
        state
            .db
            .upsert_local_file("aXb/child.txt", "H", 1, 0)
            .unwrap();
        state
            .db
            .upsert_local_file("MyXFiles/child.txt", "H", 1, 0)
            .unwrap();

        state.db.prune_stale_refused_moves().unwrap();

        assert!(
            state.db.list_refused_moves().unwrap().is_empty(),
            "none of those traces belongs to the refused paths: {:?}",
            state.db.list_refused_moves().unwrap()
        );

        // And a literal prefix still matches when it genuinely should.
        state.db.record_refused_move("a%b", "c").unwrap();
        state
            .db
            .upsert_local_file("a%b/child.txt", "H", 1, 0)
            .unwrap();
        state.db.prune_stale_refused_moves().unwrap();
        assert_eq!(
            state.db.list_refused_moves().unwrap().len(),
            1,
            "a real child under a path containing '%' must still retain it"
        );
    }

    /// ...including when the in-flight work names only the folder's CHILDREN.
    ///
    /// A folder's fallback enqueues a delete per tracked descendant and an upload per child;
    /// a delete of the folder path itself is enqueued by `process_known_folder_deletion`,
    /// which is skipped outright when a `.cloudsc` placeholder exists. So an exact match on
    /// the folder path covers only part of the window, and the rest of it the refusal would
    /// be forgotten while its own fallback was still running.
    #[test]
    fn a_refusal_outlives_a_fallback_that_names_only_children() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        state.db.record_refused_move("Docs", "Papers").unwrap();
        // Nothing names `Docs` itself — only what is under it.
        state.db.enqueue_delete_job("Docs/one.txt", None).unwrap();

        state.db.prune_stale_refused_moves().unwrap();

        assert_eq!(
            state.db.list_refused_moves().unwrap().len(),
            1,
            "the fallback for this very refusal is still draining"
        );
    }

    /// A file refusal is forgotten on the same terms as a folder one.
    ///
    /// The first version of the prune tested `known_folders` alone, so a file entry was swept
    /// on the very next tick — one of the two halves that let a refused file move loop.
    #[test]
    fn a_file_refusal_survives_while_its_source_is_still_indexed() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        state.db.upsert_local_file("b.txt", "H", 5, 0).unwrap();
        state.db.record_refused_move("b.txt", "x.txt").unwrap();

        state.db.prune_stale_refused_moves().unwrap();
        assert_eq!(
            state.db.list_refused_moves().unwrap().len(),
            1,
            "the source is still indexed, so the correlator can still propose the pair"
        );

        state.db.remove_local_file("b.txt").unwrap();
        state.db.prune_stale_refused_moves().unwrap();
        assert!(
            state.db.list_refused_moves().unwrap().is_empty(),
            "and once it is not, the entry would only punish a genuine future rename"
        );
    }

    #[test]
    fn upload_of_a_vanished_source_is_a_noop_not_a_failure() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        // Tracked but never on remote (e.g. an editor temp) and gone from disk.
        state.db.upsert_local_file("~$doc.docx", "h", 1, 0).unwrap();

        // No token / network needed: the missing-file branch returns before auth.
        crate::dropbox_transfer::upload_local_file_internal(&state, "~$doc.docx", 1)
            .expect("vanished upload must not error");

        assert!(
            state.db.get_local_file("~$doc.docx").unwrap().is_none(),
            "the vanished file is forgotten"
        );
        assert!(
            delete_jobs(&state).is_empty(),
            "nothing on remote → no delete propagated"
        );
    }

    #[test]
    fn upload_of_a_vanished_synced_file_defers_to_the_guarded_deletion_path() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        // A real synced file (present on remote) whose upload source is absent.
        state
            .db
            .upsert_local_file("report.docx", "h2", 1, 0)
            .unwrap();
        state
            .db
            .upsert_remote_file("report.docx", "h1", "rev", 0, None)
            .unwrap();

        crate::dropbox_transfer::upload_local_file_internal(&state, "report.docx", 1)
            .expect("must not error");

        // MUST NOT enqueue a remote delete from this racy check (that could wipe a
        // live file during an atomic save). The index row is left so the guarded
        // deletion-detection path (classify_path NotFound-only + re-stat) decides.
        assert!(
            delete_jobs(&state).is_empty(),
            "no remote delete may be enqueued from the upload path"
        );
        assert!(
            state.db.get_local_file("report.docx").unwrap().is_some(),
            "the index row is kept for the guarded deletion path"
        );
    }

    #[test]
    fn scan_never_recursively_deletes_a_temp_named_folder() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        // A stale known-folder row from an older build whose leaf matches a temp
        // pattern, no longer present in the (empty) sync dir → must NOT be deleted.
        state.db.upsert_known_folder("backup.tmp").unwrap();

        run_sync_tick_internal(&state).expect("tick");

        let deletes: Vec<String> = state
            .db
            .list_recent_jobs(50)
            .unwrap()
            .into_iter()
            .filter(|j| j.job_type == "delete")
            .filter_map(|j| j.target_path)
            .collect();
        assert!(
            deletes.is_empty(),
            "a temp-named folder must never trigger a recursive remote delete, got {deletes:?}"
        );
    }

    #[test]
    fn cleanup_clears_editor_temp_rows_and_phantom_failed_jobs() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let sync = tmp.path().join("synced");

        // A tracked editor-temp row that should never have existed.
        state.db.upsert_local_file("~$doc.docx", "h", 1, 0).unwrap();
        // Three failed upload jobs: a temp file, a missing regular file, and a real
        // one that still exists on disk (a genuine failure — must be left alone).
        std::fs::write(sync.join("real.txt"), b"x").unwrap();
        for src in ["~$doc.docx", "gone.txt", "real.txt"] {
            state
                .db
                .enqueue_job("upload", Some(src), Some(src))
                .unwrap();
            let id = state
                .db
                .list_recent_jobs(50)
                .unwrap()
                .into_iter()
                .find(|j| j.source_path.as_deref() == Some(src))
                .unwrap()
                .id;
            state.db.mark_job_failed(id, 5, Some("boom")).unwrap();
        }

        let cleaned = cleanup_stale_upload_state(&state).expect("cleanup");
        assert_eq!(cleaned, 3, "temp row + temp job + missing-source job");

        assert!(state.db.get_local_file("~$doc.docx").unwrap().is_none());
        let failed: Vec<String> = state
            .db
            .list_recent_jobs(50)
            .unwrap()
            .into_iter()
            .filter(|j| j.status == "failed")
            .filter_map(|j| j.source_path)
            .collect();
        assert_eq!(
            failed,
            vec!["real.txt".to_string()],
            "a genuine failure with an existing, non-temp source is kept"
        );
    }

    // ── DBSYNC-35: conflict resolution ──────────────────────────────────────

    /// Writes `content` to `<sync_folder>/<rel>` and returns the absolute path.
    fn write_synced(state: &AppState, rel: &str, content: &str) -> std::path::PathBuf {
        let folder = state.db.get_sync_folder().unwrap().unwrap();
        let abs = std::path::Path::new(&folder).join(rel);
        if let Some(p) = abs.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(&abs, content).unwrap();
        abs
    }

    fn first_conflict_id(state: &AppState) -> i64 {
        state.db.list_recent_conflicts(1).unwrap()[0].id
    }

    fn unresolved_count(state: &AppState) -> usize {
        state.db.list_recent_conflicts(100).unwrap().len()
    }

    const COPY_REL: &str = "doc (conflicted copy 20260101000000).txt";

    #[test]
    fn keep_local_promotes_copy_over_primary_and_uploads() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());

        let primary = write_synced(&state, "doc.txt", "REMOTE");
        let copy = write_synced(&state, COPY_REL, "LOCAL_EDIT");
        state
            .db
            .add_conflict("doc.txt", "doc.txt", "r", Some(COPY_REL), false)
            .unwrap();

        resolve_conflict_internal(&state, first_conflict_id(&state), ConflictAction::KeepLocal)
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(&primary).unwrap(),
            "LOCAL_EDIT",
            "the local edit must win at the primary path"
        );
        assert!(
            !copy.exists(),
            "the conflicted copy is consumed by the promotion"
        );
        assert_eq!(job_targets(&state, "upload"), vec!["doc.txt".to_string()]);
        assert!(
            job_targets(&state, "delete").is_empty(),
            "copy was never on the remote → no remote delete"
        );
        assert_eq!(
            unresolved_count(&state),
            0,
            "conflict must be marked resolved"
        );
    }

    #[test]
    fn keep_local_deletes_remote_copy_when_it_was_uploaded() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        write_synced(&state, "doc.txt", "REMOTE");
        write_synced(&state, COPY_REL, "LOCAL_EDIT");
        state
            .db
            .upsert_remote_file(COPY_REL, "h", "rev1", 1, None)
            .unwrap();
        state
            .db
            .add_conflict("doc.txt", "doc.txt", "r", Some(COPY_REL), false)
            .unwrap();

        resolve_conflict_internal(&state, first_conflict_id(&state), ConflictAction::KeepLocal)
            .unwrap();

        assert_eq!(
            job_targets(&state, "delete"),
            vec![COPY_REL.to_string()],
            "a copy that reached Dropbox must be cleaned up remotely"
        );
    }

    #[test]
    fn use_remote_discards_local_copy_keeps_primary() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let primary = write_synced(&state, "doc.txt", "REMOTE");
        let copy = write_synced(&state, COPY_REL, "LOCAL_EDIT");
        state
            .db
            .add_conflict("doc.txt", "doc.txt", "r", Some(COPY_REL), false)
            .unwrap();

        resolve_conflict_internal(&state, first_conflict_id(&state), ConflictAction::UseRemote)
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(&primary).unwrap(),
            "REMOTE",
            "the remote-content primary is preserved untouched"
        );
        assert!(!copy.exists(), "the discarded local copy is deleted");
        assert_eq!(unresolved_count(&state), 0);
    }

    #[test]
    fn use_remote_when_remote_deleted_discards_local_primary() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let primary = write_synced(&state, "doc.txt", "LOCAL_ONLY");
        // Simulate the file having been tracked locally before the remote delete.
        state.db.upsert_local_file("doc.txt", "h", 10, 1).unwrap();
        state
            .db
            .add_conflict("doc.txt", "doc.txt", "r", None, true)
            .unwrap();

        resolve_conflict_internal(&state, first_conflict_id(&state), ConflictAction::UseRemote)
            .unwrap();

        assert!(
            !primary.exists(),
            "following the deleted remote removes the local file"
        );
        assert!(
            state.db.get_local_file("doc.txt").unwrap().is_none(),
            "the local index row must be untracked so the scan sees no tracked-file deletion"
        );
        assert_eq!(unresolved_count(&state), 0);
    }

    #[test]
    fn keep_both_keeps_files_and_uploads_the_copy() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        let primary = write_synced(&state, "doc.txt", "REMOTE");
        let copy = write_synced(&state, COPY_REL, "LOCAL_EDIT");
        state
            .db
            .add_conflict("doc.txt", "doc.txt", "r", Some(COPY_REL), false)
            .unwrap();

        resolve_conflict_internal(&state, first_conflict_id(&state), ConflictAction::KeepBoth)
            .unwrap();

        assert!(
            primary.exists() && copy.exists(),
            "both versions are kept on disk"
        );
        assert_eq!(
            job_targets(&state, "upload"),
            vec![COPY_REL.to_string()],
            "the preserved copy is pushed to Dropbox so both versions sync"
        );
        assert_eq!(unresolved_count(&state), 0);
    }

    #[test]
    fn resolving_twice_is_a_safe_no_op() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        write_synced(&state, "doc.txt", "LOCAL_ONLY");
        state
            .db
            .add_conflict("doc.txt", "doc.txt", "r", None, false)
            .unwrap();
        let id = first_conflict_id(&state);

        resolve_conflict_internal(&state, id, ConflictAction::KeepLocal).unwrap();
        // Second call finds no unresolved row → returns Ok without touching anything.
        resolve_conflict_internal(&state, id, ConflictAction::UseRemote).unwrap();

        assert_eq!(unresolved_count(&state), 0);
    }

    // ── DBSYNC-64: mass-deletion circuit breaker ─────────────────────────────

    #[test]
    fn is_mass_deletion_thresholds() {
        assert!(
            is_mass_deletion(200, 100_000),
            "absolute limit trips regardless of fraction"
        );
        assert!(is_mass_deletion(30, 100), ">= floor and >= 10% of tracked");
        assert!(!is_mass_deletion(24, 100), "below the absolute floor");
        assert!(!is_mass_deletion(30, 1000), "3% is under the 10% fraction");
        assert!(!is_mass_deletion(0, 0), "nothing to delete");
    }

    #[test]
    fn scan_blocks_mass_deletion_until_overridden() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        // 30 tracked files that no longer exist on disk → 30 deletion candidates.
        for i in 0..30 {
            state
                .db
                .upsert_local_file(&format!("f{i}.txt"), "h", 1, 1)
                .unwrap();
        }

        // First scan: the batch is a mass deletion → BLOCKED. Nothing propagated,
        // index rows kept, and a DURABLE pause flag persisted. Asserting the DURABLE
        // app_config flag, not the transient engine error, is what proves the pause
        // survives a full scan in production — `refresh_queue_depth_internal` reads it.
        //
        // DBSYNC-81: this calls the LOCAL half only. It used to call the full scan and
        // discard the Result, on the assumption that the remote-refresh step "errors
        // here with no token". That holds on a machine with an empty keychain and is
        // FALSE on a developer's: there the step finds a real token, prompts for it and
        // reads production credentials. Calling the local half means the Result can be
        // asserted instead of swallowed.
        scan_local_changes_only(&state).expect("local scan must succeed");
        assert!(
            job_targets(&state, "delete").is_empty(),
            "a mass deletion must be blocked — no remote deletes enqueued"
        );
        assert_eq!(
            state.db.list_local_files().unwrap().len(),
            30,
            "blocked deletion must NOT drop the index rows"
        );
        assert!(
            state
                .db
                .get_app_config("mass_delete_blocked_scan")
                .unwrap()
                .is_some_and(|s| !s.is_empty()),
            "a blocked mass deletion must persist a durable pause flag"
        );

        // User reviews and confirms → the next scan propagates the deletions AND
        // clears the pause flag.
        state
            .db
            .set_app_config("mass_delete_override_once", "1")
            .unwrap();
        scan_local_changes_only(&state).expect("local scan must succeed");
        assert_eq!(
            job_targets(&state, "delete").len(),
            30,
            "an explicit override lets the reviewed batch through"
        );
        assert!(
            state
                .db
                .get_app_config("mass_delete_blocked_scan")
                .unwrap()
                .unwrap_or_default()
                .is_empty(),
            "overriding the batch must clear the pause flag"
        );
    }

    #[test]
    fn per_direction_pause_flags_do_not_clobber_each_other() {
        // CTO fix: the local scan and the remote sweep must each own a SEPARATE
        // durable pause flag — clearing one direction's flag must never erase the
        // other direction's still-active pause message from earlier in the tick.
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());

        block_mass_deletion(&state, 40, 100, MassDeleteSource::LocalScan);
        assert!(
            state
                .db
                .get_app_config("mass_delete_blocked_scan")
                .unwrap()
                .is_some_and(|s| !s.is_empty()),
            "local scan block must set the scan key"
        );
        assert!(
            state
                .db
                .get_app_config("mass_delete_blocked_remote")
                .unwrap()
                .unwrap_or_default()
                .is_empty(),
            "local scan block must NOT touch the remote key"
        );

        // A benign remote sweep pass clears ONLY its own key.
        clear_mass_delete_blocked(&state, MassDeleteSource::RemoteSweep);
        assert!(
            state
                .db
                .get_app_config("mass_delete_blocked_scan")
                .unwrap()
                .is_some_and(|s| !s.is_empty()),
            "a remote-sweep clear must NOT erase the local scan's pause message"
        );

        // Now the remote sweep also blocks — both flags are set independently.
        block_mass_deletion(&state, 50, 100, MassDeleteSource::RemoteSweep);
        assert!(state
            .db
            .get_app_config("mass_delete_blocked_remote")
            .unwrap()
            .is_some_and(|s| !s.is_empty()));

        // Clearing the scan side leaves the remote pause intact, and vice versa.
        clear_mass_delete_blocked(&state, MassDeleteSource::LocalScan);
        assert!(
            state
                .db
                .get_app_config("mass_delete_blocked_scan")
                .unwrap()
                .unwrap_or_default()
                .is_empty(),
            "scan clear removes the scan flag"
        );
        assert!(
            state
                .db
                .get_app_config("mass_delete_blocked_remote")
                .unwrap()
                .is_some_and(|s| !s.is_empty()),
            "scan clear must leave the remote pause flag untouched"
        );
    }

    #[test]
    fn mass_delete_pause_active_reflects_either_direction_flag() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());

        assert!(
            !mass_delete_pause_active(&state).unwrap(),
            "neither key set → not paused"
        );

        block_mass_deletion(&state, 40, 100, MassDeleteSource::LocalScan);
        assert!(
            mass_delete_pause_active(&state).unwrap(),
            "scan key set (non-empty) → paused"
        );

        clear_mass_delete_blocked(&state, MassDeleteSource::LocalScan);
        assert!(
            !mass_delete_pause_active(&state).unwrap(),
            "scan key cleared back to empty string → not paused"
        );

        block_mass_deletion(&state, 50, 100, MassDeleteSource::RemoteSweep);
        assert!(
            mass_delete_pause_active(&state).unwrap(),
            "remote key set (non-empty) → paused"
        );
    }
}
