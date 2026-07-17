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
    backoff_seconds, create_conflicted_copy, hash_file, is_builtin_ignored_local_path,
    is_editor_temp_path, is_ignored_local_path,
    normalize_dropbox_path, safe_join,
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

    let mut engine = state
        .sync_engine
        .lock()
        .map_err(|_| AppError::Sync("sync engine lock poisoned".to_string()))?;
    engine.set_queue_depth(queue_depth);
    match paused.or(failed_error) {
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
    let Ok(Some(folder)) = state.db.get_sync_folder() else {
        return false;
    };
    let root = std::path::Path::new(&folder);
    placeholder_exists(root, rel) || crate::path_util::is_dehydrated_placeholder(&root.join(rel))
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
    state.db.enqueue_delete_job(prev_rel, parent_rev.as_deref())?;
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

    let known = state.db.list_local_files()?;
    let known_map: HashMap<String, FileIndexRow> = known
        .iter()
        .map(|f| (f.relative_path.clone(), f.clone()))
        .collect();
    // DBSYNC-31: indexed active-job query instead of scanning list_recent_jobs(200)
    // (which silently missed pending jobs once the table grew past the limit).
    let mut pending_targets: HashSet<String> = state.db.active_job_paths()?;

    // Normalize + dedupe the incoming paths, dropping placeholders/ignored ones.
    let mut rels: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for p in paths {
        let rel = p.replace('\\', "/");
        let rel = rel.trim_start_matches('/').to_string();
        if rel.is_empty() || rel.ends_with(".cloudsc") || is_ignored_local_path(&rel) {
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
                    if child_rel.ends_with(".cloudsc") || is_ignored_local_path(&child_rel) {
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
            && !is_ignored_local_path(&prev.relative_path)
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
    if state.db.get_app_config(MASS_DELETE_OVERRIDE_KEY)?.as_deref() == Some("1") {
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

pub(crate) fn scan_local_changes_internal(state: &AppState) -> AppResult<usize> {
    let folder = state
        .db
        .get_sync_folder()?
        .ok_or_else(|| AppError::Sync("sync folder not configured".to_string()))?;
    let known = state.db.list_local_files()?;
    // DBSYNC-31: indexed active-job query instead of scanning list_recent_jobs(200).
    let pending_targets: HashSet<String> = state.db.active_job_paths()?;

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
        let file_deletions: Vec<String> = known
            .iter()
            .map(|f| f.relative_path.clone())
            .filter(|rel| {
                !rel.ends_with(".cloudsc")
                    && !is_ignored_local_path(rel)
                    && !seen_paths.contains(rel)
            })
            .collect();
        let folder_deletions: Vec<String> = state
            .db
            .list_known_folders()?
            .into_iter()
            .filter(|rel| !is_ignored_local_path(rel) && !seen_dirs.contains(rel))
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
                    Ok(())
                } else {
                    delete_remote_file_internal(state, rel, job.delete_parent_rev.as_deref())
                }
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
                .map(|p| !p.exists())
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
                .map(|p| !p.exists())
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
                state.db.enqueue_job("delete", Some(copy_rel), Some(copy_rel))?;
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
                state.db.enqueue_job("delete", Some(copy_rel), Some(copy_rel))?;
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
                state.db.enqueue_job("upload", Some(copy_rel), Some(copy_rel))?;
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    use chrono::{Duration, Utc};
    use tempfile::tempdir;

    use super::{
        block_mass_deletion, cleanup_stale_upload_state, clear_mass_delete_blocked,
        is_mass_deletion, mass_delete_pause_active, process_changed_paths,
        resolve_conflict_internal, run_sync_tick_internal, scan_local_changes_internal,
        ConflictAction, MassDeleteSource, SYNC_BATCH_CAP,
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
            .upsert_remote_file("foo.txt", "hash", "rev123", 0)
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
            .upsert_remote_file("dir", "hash", "rev999", 0)
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
        state.db.upsert_local_file("report.docx", "h2", 1, 0).unwrap();
        state.db.upsert_remote_file("report.docx", "h1", "rev", 0).unwrap();

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
            state.db.enqueue_job("upload", Some(src), Some(src)).unwrap();
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
        assert!(!copy.exists(), "the conflicted copy is consumed by the promotion");
        assert_eq!(job_targets(&state, "upload"), vec!["doc.txt".to_string()]);
        assert!(
            job_targets(&state, "delete").is_empty(),
            "copy was never on the remote → no remote delete"
        );
        assert_eq!(unresolved_count(&state), 0, "conflict must be marked resolved");
    }

    #[test]
    fn keep_local_deletes_remote_copy_when_it_was_uploaded() {
        let tmp = tempdir().expect("tempdir");
        let state = build_state(tmp.path());
        write_synced(&state, "doc.txt", "REMOTE");
        write_synced(&state, COPY_REL, "LOCAL_EDIT");
        state.db.upsert_remote_file(COPY_REL, "h", "rev1", 1).unwrap();
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

        assert!(!primary.exists(), "following the deleted remote removes the local file");
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

        assert!(primary.exists() && copy.exists(), "both versions are kept on disk");
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
        assert!(is_mass_deletion(200, 100_000), "absolute limit trips regardless of fraction");
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
        // index rows kept, and a DURABLE pause flag persisted. (The scan's later
        // remote-refresh step errors here with no token, but the deletion decision +
        // the pause-flag write run BEFORE it, so those side effects are committed —
        // ignore the Result and assert them. Asserting the DURABLE app_config flag,
        // not the transient engine error, is what proves the pause survives a full
        // scan in production — `refresh_queue_depth_internal` reads this flag.)
        let _ = scan_local_changes_internal(&state);
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
        let _ = scan_local_changes_internal(&state);
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
