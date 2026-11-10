//! What the File Provider extension asks of Dropbox when the user changes something in Finder (DBSYNC-95
//! slice 4, ADR-0004). macOS only. The requests arrive through the exchange
//! (`file_provider_exchange.rs`); this module turns one into Dropbox calls and into the projection row
//! the extension then returns to the system.
//!
//! Every write waits for Dropbox: the system keeps a change it could not deliver and asks again, so the
//! host keeps no queue of its own, and what the extension returns is the item as Dropbox now holds it.
//! The row is written from Dropbox's answer at once, through the same writer the delta uses, so the delta
//! that echoes the change later finds it already there.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::File;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::error::{AppError, AppResult};
use crate::models::DropboxEntry;
use crate::storage::db::Db;

/// The calls that reach Dropbox, so no test does.
pub(crate) trait DropboxWrites {
    /// Uploads `file` to a path nothing occupies, never renaming it.
    fn upload_new(&self, dropbox_path: &str, file: &Path) -> AppResult<DropboxEntry>;
    fn create_folder(&self, dropbox_path: &str) -> AppResult<DropboxEntry>;
    /// Replaces the file at `dropbox_path` only if Dropbox still holds `rev` there.
    fn upload_over(&self, dropbox_path: &str, rev: &str, file: &Path) -> AppResult<DropboxEntry>;
    /// Uploads `file` under `dropbox_path`, or under a free variant of it: a conflicted copy.
    fn upload_copy(&self, dropbox_path: &str, file: &Path) -> AppResult<DropboxEntry>;
    fn move_item(&self, from: &str, to: &str) -> AppResult<DropboxEntry>;
    /// Deletes `dropbox_path`; a file only if Dropbox still holds `rev`.
    fn delete(&self, dropbox_path: &str, rev: Option<&str>) -> AppResult<()>;
    /// What Dropbox holds at `dropbox_path` now.
    fn metadata(&self, dropbox_path: &str) -> AppResult<DropboxEntry>;
}

/// How a write ended, before it becomes a result file.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WriteOutcome {
    /// The row the extension returns.
    Written {
        item_id: i64,
    },
    /// The edit was made against a version Dropbox no longer holds. It went up as a conflicted copy, and
    /// the item stays Dropbox's version, which the system downloads again.
    Conflict {
        item_id: i64,
    },
    /// The item is gone, or was already.
    Deleted,
    /// The delete would lose an edit made on Dropbox meanwhile: the system restores the item.
    DeletionRejected,
    /// Something already holds the name: the system picks another and asks again.
    Collision,
    /// The item was deleted on Dropbox. Only the exchange decides what to answer
    /// ([`crate::file_provider_exchange`]): while a removal may still be the first half of a split rename
    /// it retries, after it the system hears `noSuchItem` and re-creates what was edited.
    NotLive,
    /// The same for the folder a create or a move names as the parent: the item itself is fine.
    ParentNotLive,
    /// Selective sync would hide the item: it would land on Dropbox and never show in Finder.
    Hidden,
    /// The parent is not a live folder of the projection, or the name cannot be one.
    Retry,
    Failed(WriteFailure),
}

/// A failure, in the classes the read path already uses.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WriteFailure {
    Offline,
    Auth,
    Retry,
}

/// Whether a name can be a Dropbox path component. The system never sends one with a slash, so this is a
/// guard and not a policy.
fn usable_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/') && !name.contains('\0')
}

fn classify(error: &AppError) -> WriteFailure {
    match error {
        AppError::Network(_) => WriteFailure::Offline,
        AppError::Auth(_) | AppError::Dropbox { status: 401, .. } => WriteFailure::Auth,
        _ => WriteFailure::Retry,
    }
}

/// Dropbox's `error_summary` names a conflict at the destination: `path/conflict/..` for an upload or a
/// new folder, `to/conflict/..` for a move (`sync_pipeline.rs` relies on the same shape).
fn is_name_conflict(error: &AppError) -> bool {
    matches!(error, AppError::Dropbox { status: 409, message }
        if crate::file_provider_exchange::error_summary(message)
            .is_some_and(|s| s.starts_with("path/conflict") || s.starts_with("to/conflict")))
}

/// Creates `name` under `parent_item`: a folder, or a file whose bytes are at `staged`.
pub(crate) fn create(
    db: &Db,
    dropbox: &dyn DropboxWrites,
    parent_item: i64,
    name: &str,
    folder: bool,
    staged: &Path,
) -> WriteOutcome {
    if !usable_name(name) {
        return WriteOutcome::Retry;
    }
    let parent = match db.projection_folder_path(parent_item) {
        Ok(Some(parent)) => parent,
        Ok(None) => return not_live(db, parent_item, WriteOutcome::ParentNotLive),
        Err(e) => {
            tracing::warn!(parent_item, error = %e, "File Provider write: cannot read the parent");
            return WriteOutcome::Retry;
        }
    };
    let path = format!("{parent}/{name}");
    if db.projection_would_hide(&path, folder).unwrap_or(false) {
        return WriteOutcome::Hidden;
    }
    // A create the system sends again after the host already made it (the extension gave up waiting,
    // or the answer was lost): the item is there, with these bytes. Without this the retry collides
    // and the system keeps both under a new name.
    if let Some(existing) = already_there(db, parent_item, name, folder, staged) {
        return WriteOutcome::Written { item_id: existing };
    }
    let landed = if folder {
        dropbox.create_folder(&path)
    } else {
        dropbox.upload_new(&path, staged)
    };
    let entry = match landed {
        Ok(entry) => entry,
        Err(e) if is_name_conflict(&e) => return WriteOutcome::Collision,
        Err(e) => {
            tracing::warn!(
                parent_item,
                class = crate::file_provider_exchange::error_class(&e),
                "File Provider write: create failed"
            );
            return WriteOutcome::Failed(classify(&e));
        }
    };
    record(db, &entry)
}

/// What a modification changes.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Change<'a> {
    /// A new parent and name, when either changed.
    pub(crate) place: Option<(i64, &'a str)>,
    /// New bytes, and the content version the edit was made against.
    pub(crate) content: Option<(&'a Path, &'a str)>,
}

/// A rename or move, then new content: each the Dropbox call it needs, in that order, so the upload goes
/// to the item's new place.
pub(crate) fn modify(
    db: &Db,
    dropbox: &dyn DropboxWrites,
    item_id: i64,
    change: &Change<'_>,
) -> WriteOutcome {
    let Some(mut target) = read_target(db, item_id) else {
        return not_live(db, item_id, WriteOutcome::NotLive);
    };
    if let Some((parent_item, name)) = change.place {
        if !usable_name(name) {
            return WriteOutcome::Retry;
        }
        let parent = match db.projection_folder_path(parent_item) {
            Ok(Some(parent)) => parent,
            Ok(None) => return not_live(db, parent_item, WriteOutcome::ParentNotLive),
            Err(_) => return WriteOutcome::Retry,
        };
        let to = format!("{parent}/{name}");
        if to != target.path && db.projection_would_hide(&to, target.is_folder).unwrap_or(false) {
            return WriteOutcome::Hidden;
        }
        if to != target.path {
            let old_lower = db.projection_path_lower(item_id).ok().flatten();
            let moved = match dropbox.move_item(&target.path, &to) {
                Ok(entry) => entry,
                Err(e) if is_name_conflict(&e) => return WriteOutcome::Collision,
                Err(e) => return failed("move", &e),
            };
            if let WriteOutcome::Failed(f) = record(db, &moved) {
                return WriteOutcome::Failed(f);
            }
            // The rows under a moved folder keep their place, but the paths the projection matches
            // deletes and sweeps by must follow it now, not when the echo arrives.
            if let (true, Some(old)) = (target.is_folder, old_lower) {
                if let Err(e) = db.projection_move_subtree(item_id, &old) {
                    tracing::warn!(error = %e, "File Provider write: cannot move the paths under a folder");
                }
            }
            let Some(moved) = read_target(db, item_id) else {
                return WriteOutcome::Retry;
            };
            target = moved;
        }
    }
    if let Some((bytes, base_version)) = change.content {
        if target.is_folder {
            return WriteOutcome::Retry;
        }
        let current = base_version.as_bytes() == target.content_version.as_slice();
        if current {
            match dropbox.upload_over(&target.path, &target.rev, bytes) {
                Ok(entry) => return record(db, &entry),
                Err(e) if !is_name_conflict(&e) => return failed("upload", &e),
                Err(_) => {
                    // Dropbox moved past the row: the edit may be the one that moved it, sent again
                    // after its answer was lost and before the delta caught up.
                    match dropbox.metadata(&target.path) {
                        Ok(now) if now.content_hash.as_deref().is_some_and(|h| same_bytes(bytes, h)) => {
                            return record(db, &now);
                        }
                        Ok(_) => {}
                        // Unknown: a conflicted copy now could be a copy of the edit itself. The system
                        // asks again.
                        Err(e) => return failed("metadata", &e),
                    }
                }
            }
        }
        // Not a conflict after all when Dropbox already holds these bytes: the system sent an edit again
        // after it landed. Hashed only here, so an ordinary edit is read once.
        if same_bytes(bytes, &target.content_hash) {
            return WriteOutcome::Written { item_id };
        }
        return keep_both(db, dropbox, item_id, &target.path, bytes);
    }
    WriteOutcome::Written { item_id }
}

/// The edit goes up beside Dropbox's version, under a conflicted-copy name, and is recorded as a
/// conflict. Nothing is lost and nothing is left refused: both are ordinary files.
fn keep_both(
    db: &Db,
    dropbox: &dyn DropboxWrites,
    item_id: i64,
    path: &str,
    bytes: &Path,
) -> WriteOutcome {
    let copy = conflicted_copy_path(path, chrono::Utc::now());
    let landed = match dropbox.upload_copy(&copy, bytes) {
        Ok(entry) => entry,
        Err(e) => return failed("conflicted copy", &e),
    };
    let copy = landed.path_display.clone().unwrap_or(copy);
    if let WriteOutcome::Failed(f) = record(db, &landed) {
        return WriteOutcome::Failed(f);
    }
    let key = |p: &str| crate::path_util::key_from_dropbox(p);
    if let Err(e) = db.add_conflict(
        &key(path),
        &key(path),
        "file_provider_stale_edit",
        Some(&key(&copy)),
        false,
    ) {
        tracing::warn!(error = %e, "File Provider write: cannot record the conflict");
    }
    WriteOutcome::Conflict { item_id }
}

/// `stem (conflicted copy <stamp>).ext`, the name the sync folder's resolver gives
/// (`path_util::unique_conflicted_name`). Dropbox picks a free variant if it is taken.
fn conflicted_copy_path(path: &str, now: chrono::DateTime<chrono::Utc>) -> String {
    let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
    let stamp = now.format("%Y%m%d%H%M%S");
    let named = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem} (conflicted copy {stamp}).{ext}"),
        _ => format!("{name} (conflicted copy {stamp})"),
    };
    format!("{parent}/{named}")
}

/// [`delete`], behind the breaker: what the item weighs is the files it takes with it.
pub(crate) fn delete_guarded(
    state: &crate::state::AppState,
    dropbox: &dyn DropboxWrites,
    item_id: i64,
    base_version: &str,
) -> WriteOutcome {
    let (weight, tracked) = match state.db.projection_delete_weight(item_id) {
        Ok(counts) => counts,
        Err(e) => {
            tracing::warn!(item_id, error = %e, "File Provider write: cannot weigh the delete");
            return WriteOutcome::Retry;
        }
    };
    let admission = DELETE_GUARD.admit(
        item_id,
        weight,
        tracked,
        &DeletePolicy::LIVE,
        &AppDecisions { state },
        &Instant::now,
        &std::thread::sleep,
    );
    match admission {
        Admission::Proceed => delete(&state.db, dropbox, item_id, base_version),
        Admission::ProceedConfirmed => {
            // Marked before the call, so a duplicate request arriving meanwhile takes the answer too,
            // and cleared once Dropbox answered for good.
            DELETE_GUARD.unfinished(item_id);
            let outcome = delete(&state.db, dropbox, item_id, base_version);
            if !matches!(outcome, WriteOutcome::Retry | WriteOutcome::Failed(_)) {
                DELETE_GUARD.finished(item_id);
            }
            outcome
        }
        Admission::Refuse => WriteOutcome::DeletionRejected,
        Admission::StillHeld => WriteOutcome::Retry,
    }
}

/// Deletes the item. `base_version` is the content the system had: a file edited on Dropbox since is
/// not deleted, and the system restores it with the new content.
pub(crate) fn delete(
    db: &Db,
    dropbox: &dyn DropboxWrites,
    item_id: i64,
    base_version: &str,
) -> WriteOutcome {
    let Some(target) = read_target(db, item_id) else {
        // Already gone, or never ours: either way, nothing on Dropbox stands for it.
        return WriteOutcome::Deleted;
    };
    if !target.is_folder && base_version.as_bytes() != target.content_version.as_slice() {
        return WriteOutcome::DeletionRejected;
    }
    // Deleting the folder on Dropbox would take what selective sync hides with it, which Finder never
    // showed: refused, as every folder delete of the sync folder is (DBSYNC-114, `path_util.rs`).
    if target.is_folder {
        match db.projection_folder_hides_content(item_id) {
            Ok(false) => {}
            Ok(true) => return WriteOutcome::DeletionRejected,
            Err(e) => {
                tracing::warn!(item_id, error = %e, "File Provider write: cannot read the folder");
                return WriteOutcome::Retry;
            }
        }
    }
    let rev = (!target.is_folder).then_some(target.rev.as_str());
    match dropbox.delete(&target.path, rev) {
        Ok(()) => {}
        Err(e) if is_not_found(&e) => {}
        // A file whose rev moved on since: Dropbox keeps it, and so does the system.
        Err(AppError::Dropbox { status: 409, .. }) => return WriteOutcome::DeletionRejected,
        Err(e) => return failed("delete", &e),
    }
    let gone = DropboxEntry {
        tag: "deleted".to_string(),
        path_display: Some(target.path.clone()),
        path_lower: Some(target.path.to_lowercase()),
        content_hash: None,
        rev: None,
        server_modified: None,
        size: None,
        id: None,
    };
    if let Err(e) = db.project_delta(std::slice::from_ref(&gone)) {
        tracing::warn!(error = %e, "File Provider write: cannot record the delete");
    }
    WriteOutcome::Deleted
}

/// The mass-deletion breaker (DBSYNC-64) for deletions made in Finder.
///
/// Finder sends a selection as concurrent `deleteItem` calls, one per item (measured: six within 0.7 s).
/// Each waits until no further delete has arrived for [`DeletePolicy::settle`], so every call of a burst
/// sees the whole burst. A burst that, with the deletes let through in the last [`DeletePolicy::window`],
/// the existing rule calls a mass deletion is held, still waiting, until the user confirms or refuses it
/// in the app; refused, the system restores the files. A deleted folder counts as the files under it.
///
/// The system keeps at most [`SATURATED_BURST`] deletes in flight and sends the next only as one returns
/// (measured: a 300-file selection arrived as 32, then one per answered call). A burst that fills every
/// slot may be the head of a larger selection the guard cannot see, so it is held whatever its size:
/// judged on the threshold alone, the head of a large selection went to Dropbox unasked, and in a large
/// account the stream that followed never added up to a mass deletion within the window.
///
/// An answer covers the operation it was asked about: the calls waiting when it comes, the rest of the
/// selection (new items that keep arriving within [`DeletePolicy::stream_gap`] of the answer or of the one
/// before them: the system sends the rest as the held calls return), and the items the system sends
/// again: a call that gave up waiting, or a confirmed delete Dropbox did not take. Those carry their answer
/// with them, so a later burst does not take it from them. Once the stream pauses, any other delete starts
/// a new burst. The system does not retry a `deletionRejected`, so the same files deleted again after a
/// restore are a new question.
pub(crate) struct DeleteGuard {
    state: Mutex<GuardState>,
}

struct GuardState {
    /// The burst being gathered or held. Numbered, so that a call can tell its burst from a later one.
    burst: u64,
    burst_items: BTreeSet<i64>,
    burst_weight: usize,
    last_arrival: Option<Instant>,
    /// The burst was looked at once it settled: held, or let through as not a mass deletion.
    judged: bool,
    held: bool,
    /// The last answer: which burst it was for, and when its stream last moved.
    decided: Option<Decided>,
    /// Items the system will send again, with their answer: `None` while their burst is held.
    pending: BTreeMap<i64, Option<bool>>,
    /// Weights of small bursts let through lately, for [`DeletePolicy::window`].
    recent: VecDeque<(Instant, usize)>,
    /// What `recent` held when the burst on screen was held, so its count includes it.
    held_base: usize,
}

#[derive(Clone, Copy)]
struct Decided {
    burst: u64,
    confirm: bool,
    /// The answer, then each new item of the stream: a retry does not move it, so retries cannot keep it
    /// open.
    stream_from: Instant,
}

/// What the breaker answers one delete.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Admission {
    /// Not a mass deletion.
    Proceed,
    /// Part of a mass deletion the user confirmed: if it does not reach Dropbox, its retry takes the
    /// same answer ([`DeleteGuard::unfinished`]).
    ProceedConfirmed,
    /// The user refused the burst: `deletionRejected`.
    Refuse,
    /// Still held when the wait ran out: a transient error, and the system asks again.
    StillHeld,
}

/// The decisions the user makes in the app, and the flag that shows them the question.
pub(crate) trait DeleteDecisions {
    fn hold(&self, weight: usize, tracked: usize);
    /// More of the held selection arrived: `weight` is the whole of it so far.
    fn recount(&self, weight: usize, tracked: usize);
    /// The user's answer, consumed: `Some(true)` to delete, `Some(false)` to restore.
    fn answer(&self) -> Option<bool>;
    fn release(&self);
}

pub(crate) struct DeletePolicy {
    pub(crate) settle: Duration,
    /// How long the rest of an answered selection may pause between two items and still take the answer.
    /// Over one Dropbox delete: a confirmed selection arrives one item per finished delete.
    pub(crate) stream_gap: Duration,
    /// How far back deletes let through still count towards the next burst.
    pub(crate) window: Duration,
    /// How long one call waits for the user before it answers `StillHeld`. Under the extension's own
    /// deadline.
    pub(crate) patience: Duration,
    pub(crate) poll: Duration,
}

impl DeletePolicy {
    pub(crate) const LIVE: Self = Self {
        settle: Duration::from_millis(1500),
        stream_gap: Duration::from_secs(5),
        window: Duration::from_secs(60),
        patience: Duration::from_secs(500),
        poll: Duration::from_millis(200),
    };
}

impl DeleteGuard {
    pub(crate) const fn new() -> Self {
        Self {
            state: Mutex::new(GuardState {
                burst: 0,
                burst_items: BTreeSet::new(),
                burst_weight: 0,
                last_arrival: None,
                judged: false,
                held: false,
                decided: None,
                pending: BTreeMap::new(),
                recent: VecDeque::new(),
                held_base: 0,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, GuardState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The confirmed delete of this item is done: nothing of it is sent again.
    pub(crate) fn finished(&self, item_id: i64) {
        self.lock().pending.remove(&item_id);
    }

    /// Whether a delete of this item is held, still gathering, or waiting to be sent again.
    pub(crate) fn involves(&self, item_id: i64) -> bool {
        let st = self.lock();
        st.pending.contains_key(&item_id)
            || (st.burst_items.contains(&item_id) && (st.held || !st.judged))
    }

    /// A delete the user confirmed did not reach Dropbox (rate-limited, offline): when the system
    /// retries it, it takes the same answer instead of opening a new question. Measured live: 40 of 50
    /// confirmed deletes answered 429, came back as a new burst, and the user was asked twice more.
    pub(crate) fn unfinished(&self, item_id: i64) {
        self.lock().pending.insert(item_id, Some(true));
    }

    /// Blocks until this delete may go, must be refused, or has waited long enough. The clock and the
    /// sleep are arguments so the tests can run the real waits fast.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn admit(
        &self,
        item_id: i64,
        weight: usize,
        tracked: usize,
        policy: &DeletePolicy,
        decisions: &dyn DeleteDecisions,
        now: &dyn Fn() -> Instant,
        sleep: &dyn Fn(Duration),
    ) -> Admission {
        let mine = {
            let mut st = self.lock();
            let t = now();
            match st.pending.get(&item_id).copied() {
                Some(Some(confirm)) => {
                    st.pending.remove(&item_id);
                    return answer_for(confirm);
                }
                // Its burst is still held: it waits again with it. Held or not, it is not pending now.
                Some(None) => {
                    st.pending.remove(&item_id);
                }
                None => {}
            }
            // An answer stored while nobody was waiting (every call gave up) is for the burst on screen
            // when it was given: it settles that burst. A call of that burst (a retry, or a duplicate of
            // a call still waiting) takes the answer; any other is not part of it.
            if st.held {
                if let Some(confirm) = decisions.answer() {
                    let member = st.burst_items.contains(&item_id);
                    decide(&mut st, confirm, t, decisions);
                    if member {
                        return answer_for(confirm);
                    }
                    start_burst(&mut st);
                }
            }
            if !st.held {
                let current = st.burst;
                if let Some(d) = st.decided.as_mut() {
                    let rest_of_selection = d.burst == current
                        && t.saturating_duration_since(d.stream_from) <= policy.stream_gap;
                    if rest_of_selection {
                        d.stream_from = t;
                        return answer_for(d.confirm);
                    }
                }
                let decided_burst = st.decided.is_some_and(|d| d.burst == st.burst);
                let settled = st
                    .last_arrival
                    .is_none_or(|at| t.saturating_duration_since(at) > policy.settle);
                // A burst that settled before any of its calls looked at it is judged here, before
                // another starts: starting one first would let a mass deletion's calls through unasked.
                if settled && !st.judged && st.last_arrival.is_some() {
                    judge(&mut st, t, tracked, policy, decisions);
                }
                if !st.held && (decided_burst || settled) {
                    start_burst(&mut st);
                }
            }
            if st.burst_items.insert(item_id) {
                st.burst_weight += weight;
                if st.held {
                    decisions.recount(st.held_base + st.burst_weight, tracked);
                }
            }
            st.last_arrival = Some(t);
            st.burst
        };
        // Let the burst finish arriving.
        loop {
            let st = self.lock();
            if st.burst != mine {
                return earlier_burst(&st, mine);
            }
            if st.held || st.judged {
                break;
            }
            let settled = st
                .last_arrival
                .is_none_or(|at| now().saturating_duration_since(at) >= policy.settle);
            if settled {
                break;
            }
            drop(st);
            sleep(policy.poll);
        }
        {
            let mut st = self.lock();
            if st.burst != mine {
                return earlier_burst(&st, mine);
            }
            if !st.judged {
                judge(&mut st, now(), tracked, policy, decisions);
            }
            if !st.held {
                return Admission::Proceed;
            }
        }
        let started = now();
        loop {
            {
                let mut st = self.lock();
                if let Some(d) = st.decided.filter(|d| d.burst == mine) {
                    return answer_for(d.confirm);
                }
                // Only a call of the burst on screen may take the answer.
                if st.held && st.burst == mine {
                    if let Some(confirm) = decisions.answer() {
                        decide(&mut st, confirm, now(), decisions);
                        return answer_for(confirm);
                    }
                }
                if now().saturating_duration_since(started) >= policy.patience {
                    st.pending.insert(item_id, None);
                    return Admission::StillHeld;
                }
            }
            sleep(policy.poll);
        }
    }
}

/// How many deletes the system keeps in flight at once (measured with 50- and 300-file selections).
const SATURATED_BURST: usize = 32;

/// What a call of an earlier burst answers once a later one began: that burst's answer if it had one,
/// or it was judged and let through.
fn earlier_burst(st: &GuardState, mine: u64) -> Admission {
    match st.decided.filter(|d| d.burst == mine) {
        Some(d) => answer_for(d.confirm),
        None => Admission::Proceed,
    }
}

/// Looks at the settled burst once: held when it fills every slot the system has ([`SATURATED_BURST`]),
/// or when it, with what was deleted in the last [`DeletePolicy::window`], is a mass deletion. The window
/// catches small selections made one after another; a burst let through joins it.
fn judge(
    st: &mut GuardState,
    now: Instant,
    tracked: usize,
    policy: &DeletePolicy,
    decisions: &dyn DeleteDecisions,
) {
    st.judged = true;
    while st
        .recent
        .front()
        .is_some_and(|(at, _)| now.saturating_duration_since(*at) > policy.window)
    {
        st.recent.pop_front();
    }
    let before: usize = st.recent.iter().map(|(_, w)| w).sum();
    let saturated = st.burst_items.len() >= SATURATED_BURST;
    if saturated || crate::sync_pipeline::is_mass_deletion(before + st.burst_weight, tracked) {
        st.held = true;
        st.held_base = before;
        decisions.hold(before + st.burst_weight, tracked);
    } else {
        let weight = st.burst_weight;
        st.recent.push_back((now, weight));
    }
}

fn start_burst(st: &mut GuardState) {
    st.burst += 1;
    st.burst_items.clear();
    st.burst_weight = 0;
    st.judged = false;
    st.last_arrival = None;
}

/// Records the answer for the held burst: its calls that gave up take it when they come back. The
/// burst's own weight does not stay in the window: confirmed, it is done; refused, it is back. A
/// confirmation also approves what the window held when the question was asked, which the banner
/// counted, so the window starts again from nothing.
fn decide(st: &mut GuardState, confirm: bool, at: Instant, decisions: &dyn DeleteDecisions) {
    st.held = false;
    if confirm {
        st.recent.clear();
    }
    st.decided = Some(Decided {
        burst: st.burst,
        confirm,
        stream_from: at,
    });
    for answer in st.pending.values_mut() {
        if answer.is_none() {
            *answer = Some(confirm);
        }
    }
    decisions.release();
}

fn answer_for(confirm: bool) -> Admission {
    if confirm {
        Admission::ProceedConfirmed
    } else {
        Admission::Refuse
    }
}

pub(crate) static DELETE_GUARD: DeleteGuard = DeleteGuard::new();

/// The app's decisions, through the same flags as the sync folder's breaker.
pub(crate) struct AppDecisions<'a> {
    pub(crate) state: &'a crate::state::AppState,
}

impl DeleteDecisions for AppDecisions<'_> {
    fn hold(&self, weight: usize, tracked: usize) {
        if let Err(e) = crate::sync_pipeline::forget_finder_answers(self.state) {
            tracing::warn!(error = %e, "File Provider write: cannot clear an earlier answer");
        }
        crate::sync_pipeline::block_mass_deletion(
            self.state,
            weight,
            tracked,
            crate::sync_pipeline::MassDeleteSource::Finder,
        );
    }

    fn recount(&self, weight: usize, tracked: usize) {
        crate::sync_pipeline::recount_finder_deletion(self.state, weight, tracked);
    }

    fn answer(&self) -> Option<bool> {
        crate::sync_pipeline::take_finder_answer(self.state).unwrap_or(None)
    }

    fn release(&self) {
        crate::sync_pipeline::clear_mass_delete_blocked(
            self.state,
            crate::sync_pipeline::MassDeleteSource::Finder,
        );
    }
}

fn is_not_found(error: &AppError) -> bool {
    matches!(error, AppError::Dropbox { status: 409, message }
        if crate::file_provider_exchange::error_summary(message)
            .is_some_and(|s| s.contains("not_found")))
}

fn read_target(db: &Db, item_id: i64) -> Option<crate::storage::db::projection::WriteTarget> {
    match db.projection_write_target(item_id) {
        Ok(target) => target,
        Err(e) => {
            tracing::warn!(item_id, error = %e, "File Provider write: cannot read the item");
            None
        }
    }
}

fn failed(step: &'static str, error: &AppError) -> WriteOutcome {
    tracing::warn!(
        step,
        class = crate::file_provider_exchange::error_class(error),
        "File Provider write: failed"
    );
    WriteOutcome::Failed(classify(error))
}

/// Writes the row from Dropbox's own answer and returns its id. A delta that saw the item first has
/// already made the row, and this lands on it: one id either way.
/// A row that is not live: `dead` when it is a tombstone (deleted on Dropbox), `Retry` otherwise
/// (hidden, unknown, or a read that failed).
fn not_live(db: &Db, item_id: i64, dead: WriteOutcome) -> WriteOutcome {
    match db.projection_row_state(item_id) {
        Ok(Some(crate::storage::db::projection::STATE_DELETED)) => dead,
        _ => WriteOutcome::Retry,
    }
}

/// Whether the staged bytes are the content Dropbox holds under `content_hash`.
fn same_bytes(staged: &Path, content_hash: &str) -> bool {
    !content_hash.is_empty()
        && crate::path_util::hash_file(staged).is_ok_and(|(hash, _, _)| hash == content_hash)
}

/// The live child of `parent_item` named `name` that is already what a create asks for: a folder, or a
/// file with these bytes.
fn already_there(db: &Db, parent_item: i64, name: &str, folder: bool, staged: &Path) -> Option<i64> {
    let (item_id, is_folder, content_hash) = db.projection_child(parent_item, name).ok().flatten()?;
    // An item whose delete is still held or waiting to be sent again is going: a file put back under
    // its name is a new item, or it would go with the delete.
    if DELETE_GUARD.involves(item_id) {
        return None;
    }
    let same = if folder {
        is_folder
    } else {
        !is_folder && same_bytes(staged, &content_hash)
    };
    same.then_some(item_id)
}

fn record(db: &Db, entry: &DropboxEntry) -> WriteOutcome {
    let Some(dropbox_id) = entry.id.clone() else {
        tracing::warn!("File Provider write: Dropbox answered without an id");
        return WriteOutcome::Failed(WriteFailure::Retry);
    };
    if let Err(e) = db.project_delta(std::slice::from_ref(entry)) {
        tracing::warn!(error = %e, "File Provider write: cannot record the item");
        return WriteOutcome::Failed(WriteFailure::Retry);
    }
    match db.projection_item_for_dropbox_id(&dropbox_id) {
        Ok(Some(item_id)) => WriteOutcome::Written { item_id },
        _ => WriteOutcome::Failed(WriteFailure::Retry),
    }
}

/// The real calls.
pub(crate) struct LiveDropbox<'a> {
    pub(crate) state: &'a crate::state::AppState,
    /// Whether the extension stopped waiting for this request: checked before anything is sent.
    pub(crate) abandoned: &'a dyn Fn() -> bool,
}

fn write_lane() -> AppResult<std::sync::MutexGuard<'static, ()>> {
    crate::dropbox_transfer::fp_write_lane()
}

impl LiveDropbox<'_> {
    /// The lane, unless the extension stopped waiting meanwhile.
    fn lane(&self) -> AppResult<std::sync::MutexGuard<'static, ()>> {
        let lane = write_lane()?;
        if (self.abandoned)() {
            return Err(AppError::Other("the extension stopped waiting".into()));
        }
        Ok(lane)
    }
}

impl LiveDropbox<'_> {
    fn upload(
        &self,
        dropbox_path: &str,
        mode: Value,
        autorename: bool,
        file: &Path,
    ) -> AppResult<DropboxEntry> {
        let handle = File::open(file)?;
        let len = handle.metadata()?.len();
        let arg = serde_json::json!({
            "path": dropbox_path,
            "mode": mode,
            "autorename": autorename,
            "strict_conflict": true,
        });
        // Anything past one chunk goes through a session, so the lane is held for the `finish` alone and
        // not while the bytes travel.
        let body = if len > crate::dropbox_transfer::UPLOAD_CHUNK_SIZE as u64 {
            crate::dropbox_transfer::post_content_session(self.state, &arg, handle, len, self.abandoned)?
        } else {
            let _lane = self.lane()?;
            crate::dropbox_transfer::post_content(self.state, "files/upload", &arg, handle, len)?
        };
        entry_from_metadata(body, "file")
    }

    fn rpc_metadata(&self, endpoint: &str, arg: &Value, tag: &str) -> AppResult<DropboxEntry> {
        let body = {
            let _lane = self.lane()?;
            crate::dropbox_transfer::post_rpc(self.state, endpoint, arg)?
        };
        let metadata = body
            .get("metadata")
            .cloned()
            .ok_or_else(|| AppError::Other(format!("{endpoint} answered without metadata")))?;
        entry_from_metadata(metadata, tag)
    }
}

impl DropboxWrites for LiveDropbox<'_> {
    fn upload_new(&self, dropbox_path: &str, file: &Path) -> AppResult<DropboxEntry> {
        self.upload(
            dropbox_path,
            serde_json::json!({ ".tag": "add" }),
            false,
            file,
        )
    }

    fn create_folder(&self, dropbox_path: &str) -> AppResult<DropboxEntry> {
        let arg = serde_json::json!({ "path": dropbox_path, "autorename": false });
        self.rpc_metadata("files/create_folder_v2", &arg, "folder")
    }

    fn upload_over(&self, dropbox_path: &str, rev: &str, file: &Path) -> AppResult<DropboxEntry> {
        let mode = serde_json::json!({ ".tag": "update", "update": rev });
        self.upload(dropbox_path, mode, false, file)
    }

    fn upload_copy(&self, dropbox_path: &str, file: &Path) -> AppResult<DropboxEntry> {
        self.upload(
            dropbox_path,
            serde_json::json!({ ".tag": "add" }),
            true,
            file,
        )
    }

    /// `move_v2` answers with the item's metadata, `.tag` included: a folder stays a folder.
    fn move_item(&self, from: &str, to: &str) -> AppResult<DropboxEntry> {
        let arg = serde_json::json!({ "from_path": from, "to_path": to, "autorename": false });
        self.rpc_metadata("files/move_v2", &arg, "file")
    }

    fn delete(&self, dropbox_path: &str, rev: Option<&str>) -> AppResult<()> {
        let arg = match rev {
            Some(rev) => serde_json::json!({ "path": dropbox_path, "parent_rev": rev }),
            None => serde_json::json!({ "path": dropbox_path }),
        };
        let _lane = self.lane()?;
        crate::dropbox_transfer::post_rpc(self.state, "files/delete_v2", &arg).map(|_| ())
    }

    /// A read: it takes no lane.
    fn metadata(&self, dropbox_path: &str) -> AppResult<DropboxEntry> {
        let arg = serde_json::json!({ "path": dropbox_path });
        let body = crate::dropbox_transfer::post_rpc(self.state, "files/get_metadata", &arg)?;
        let tag = body.get(".tag").and_then(Value::as_str).unwrap_or("file").to_string();
        entry_from_metadata(body, &tag)
    }
}

/// Dropbox's metadata as a delta entry: the upload answer carries no `.tag`, because only a file is
/// ever uploaded.
fn entry_from_metadata(mut metadata: Value, tag: &str) -> AppResult<DropboxEntry> {
    if let Some(object) = metadata.as_object_mut() {
        object
            .entry(".tag")
            .or_insert_with(|| Value::String(tag.to_string()));
    }
    Ok(serde_json::from_value(metadata)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db::projection::ProjectedEntry;
    use std::cell::RefCell;

    fn entry(tag: &str, display: &str, id: &str) -> DropboxEntry {
        DropboxEntry {
            tag: tag.to_string(),
            path_display: Some(display.to_string()),
            path_lower: Some(display.to_lowercase()),
            content_hash: (tag == "file").then(|| "h".repeat(64)),
            rev: (tag == "file").then(|| "015f0000000000000001".to_string()),
            server_modified: (tag == "file").then(|| "2026-11-08T10:00:00Z".to_string()),
            size: (tag == "file").then_some(5),
            id: Some(id.to_string()),
        }
    }

    /// A projection holding one folder, `/Docs`, written by the production snapshot writer.
    fn db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::new_at(&dir.path().join("app.db")).unwrap();
        db.set_projection_enabled(true);
        let listing = [entry("folder", "/Docs", "id:D")];
        let projected: Vec<ProjectedEntry> = listing
            .iter()
            .filter_map(ProjectedEntry::from_entry)
            .collect();
        db.project_snapshot(&projected).unwrap();
        (dir, db)
    }

    /// Answers like Dropbox, and records the paths it was sent.
    struct Fake {
        #[allow(dead_code)]
        answer: RefCell<Option<AppResult<DropboxEntry>>>,
        queue: RefCell<Vec<AppResult<DropboxEntry>>>,
        paths: RefCell<Vec<String>>,
    }

    impl Fake {
        fn answering(answer: AppResult<DropboxEntry>) -> Self {
            Self::answering_all(vec![answer])
        }

        fn answering_all(answers: Vec<AppResult<DropboxEntry>>) -> Self {
            Self {
                answer: RefCell::new(None),
                queue: RefCell::new(Vec::new()),
                paths: RefCell::new(Vec::new()),
            }
            .with(answers)
        }

        fn with(self, mut answers: Vec<AppResult<DropboxEntry>>) -> Self {
            answers.reverse();
            *self.queue.borrow_mut() = answers;
            self
        }

        fn take(&self, call: &str) -> AppResult<DropboxEntry> {
            self.paths.borrow_mut().push(call.to_string());
            self.queue
                .borrow_mut()
                .pop()
                .unwrap_or_else(|| Err(AppError::Other("not expected".into())))
        }
    }

    impl DropboxWrites for Fake {
        fn upload_new(&self, dropbox_path: &str, _file: &Path) -> AppResult<DropboxEntry> {
            self.take(&format!("new {dropbox_path}"))
        }
        fn create_folder(&self, dropbox_path: &str) -> AppResult<DropboxEntry> {
            self.take(&format!("new {dropbox_path}"))
        }
        fn upload_over(
            &self,
            dropbox_path: &str,
            rev: &str,
            _file: &Path,
        ) -> AppResult<DropboxEntry> {
            self.take(&format!("over {dropbox_path} {rev}"))
        }
        fn upload_copy(&self, dropbox_path: &str, _file: &Path) -> AppResult<DropboxEntry> {
            let stamp_free = dropbox_path.split(" (conflicted copy").next().unwrap_or("");
            self.take(&format!("copy {stamp_free}"))
        }
        fn move_item(&self, from: &str, to: &str) -> AppResult<DropboxEntry> {
            self.take(&format!("move {from} {to}"))
        }
        fn delete(&self, dropbox_path: &str, rev: Option<&str>) -> AppResult<()> {
            self.take(&format!("delete {dropbox_path} {}", rev.unwrap_or("-")))
                .map(|_| ())
        }
        fn metadata(&self, dropbox_path: &str) -> AppResult<DropboxEntry> {
            self.take(&format!("metadata {dropbox_path}"))
        }
    }

    fn docs(db: &Db) -> i64 {
        db.projection_item_id("/docs").unwrap()
    }

    #[test]
    fn a_new_file_lands_under_its_parent_and_comes_back_as_the_row_dropbox_described() {
        let (_dir, db) = db();
        let fake = Fake::answering(Ok(entry("file", "/Docs/a.txt", "id:A")));

        let outcome = create(&db, &fake, docs(&db), "a.txt", false, Path::new("/unused"));

        assert_eq!(*fake.paths.borrow(), vec!["new /Docs/a.txt".to_string()]);
        let item_id = db.projection_item_id("/docs/a.txt").unwrap();
        assert_eq!(outcome, WriteOutcome::Written { item_id });
    }

    #[test]
    fn a_new_folder_at_the_root_has_no_leading_parent() {
        let (_dir, db) = db();
        let fake = Fake::answering(Ok(entry("folder", "/New", "id:N")));

        let outcome = create(&db, &fake, 0, "New", true, Path::new("/unused"));

        assert_eq!(*fake.paths.borrow(), vec!["new /New".to_string()]);
        let item_id = db.projection_item_id("/new").unwrap();
        assert_eq!(outcome, WriteOutcome::Written { item_id });
    }

    /// The delta can report the new item before the upload's answer is recorded. The write lands on the
    /// row it made: the system never sees two items for one file.
    #[test]
    fn a_delta_that_saw_the_item_first_keeps_its_id() {
        let (_dir, db) = db();
        db.project_delta(&[entry("file", "/Docs/a.txt", "id:A")])
            .unwrap();
        let first = db.projection_item_id("/docs/a.txt").unwrap();
        let fake = Fake::answering(Ok(entry("file", "/Docs/a.txt", "id:A")));

        let outcome = create(&db, &fake, docs(&db), "a.txt", false, Path::new("/unused"));

        assert_eq!(outcome, WriteOutcome::Written { item_id: first });
    }

    #[test]
    fn a_taken_name_is_a_collision() {
        let (_dir, db) = db();
        let fake = Fake::answering(Err(AppError::Dropbox {
            status: 409,
            message:
                r#"upload for /docs/a.txt: {"error_summary": "path/conflict/file/..", "error": {}}"#
                    .into(),
        }));

        assert_eq!(
            create(&db, &fake, docs(&db), "a.txt", false, Path::new("/unused")),
            WriteOutcome::Collision
        );
    }

    #[test]
    fn failures_keep_the_read_path_classes() {
        let cases = [
            (AppError::Network("x".into()), WriteFailure::Offline),
            (AppError::Auth("x".into()), WriteFailure::Auth),
            (AppError::Io("x".into()), WriteFailure::Retry),
        ];
        for (error, expected) in cases {
            let (_dir, db) = db();
            let fake = Fake::answering(Err(error));
            assert_eq!(
                create(&db, &fake, docs(&db), "a.txt", false, Path::new("/unused")),
                WriteOutcome::Failed(expected)
            );
        }
    }

    /// Nothing reaches Dropbox for a parent the projection does not hold as a live folder.
    #[test]
    fn an_unknown_parent_or_an_unusable_name_sends_nothing() {
        let (_dir, db) = db();
        let fake = Fake::answering(Ok(entry("file", "/x", "id:X")));

        assert_eq!(
            create(&db, &fake, 9_999, "a.txt", false, Path::new("/u")),
            WriteOutcome::Retry
        );
        for name in ["", ".", "..", "a/b"] {
            assert_eq!(
                create(&db, &fake, docs(&db), name, false, Path::new("/u")),
                WriteOutcome::Retry
            );
        }

        assert!(fake.paths.borrow().is_empty());
    }

    fn file_with(display: &str, id: &str, rev: &str, hash: &str) -> DropboxEntry {
        DropboxEntry {
            rev: Some(rev.to_string()),
            content_hash: Some(hash.to_string()),
            ..entry("file", display, id)
        }
    }

    /// `/Docs/a.txt` at rev `r1`, through the delta writer.
    fn with_file(db: &Db) -> (i64, String) {
        db.project_delta(&[file_with("/Docs/a.txt", "id:A", "r1", &"1".repeat(64))])
            .unwrap();
        let item_id = db.projection_item_id("/docs/a.txt").unwrap();
        let version = db
            .projection_write_target(item_id)
            .unwrap()
            .unwrap()
            .content_version;
        (item_id, String::from_utf8(version).unwrap())
    }

    const BYTES: &str = "/staged";

    #[test]
    fn an_edit_on_the_current_version_replaces_that_rev() {
        let (_dir, db) = db();
        let (item_id, base) = with_file(&db);
        let fake = Fake::answering(Ok(file_with("/Docs/a.txt", "id:A", "r2", &"2".repeat(64))));
        let change = Change {
            content: Some((Path::new(BYTES), &base)),
            ..Change::default()
        };

        assert_eq!(
            modify(&db, &fake, item_id, &change),
            WriteOutcome::Written { item_id }
        );

        assert_eq!(
            *fake.paths.borrow(),
            vec!["over /Docs/a.txt r1".to_string()]
        );
        assert_eq!(
            db.projection_write_target(item_id).unwrap().unwrap().rev,
            "r2"
        );
    }

    /// The system edited a version Dropbox no longer has: nothing overwrites Dropbox's, the edit goes up
    /// as a copy, and the conflict is recorded.
    ///
    /// Proof this test can fail: comparing nothing sends `upload_over` on the stale base.
    #[test]
    fn an_edit_on_a_stale_version_keeps_both() {
        let (_dir, db) = db();
        let (item_id, _) = with_file(&db);
        let fake = Fake::answering(Ok(file_with(
            "/Docs/a (conflicted copy 1).txt",
            "id:C",
            "r9",
            &"9".repeat(64),
        )));
        let change = Change {
            content: Some((Path::new(BYTES), "c1:stale")),
            ..Change::default()
        };

        assert_eq!(
            modify(&db, &fake, item_id, &change),
            WriteOutcome::Conflict { item_id }
        );

        assert_eq!(*fake.paths.borrow(), vec!["copy /Docs/a".to_string()]);
        assert_eq!(
            db.projection_write_target(item_id).unwrap().unwrap().rev,
            "r1"
        );
        assert!(db.projection_item_for_dropbox_id("id:C").unwrap().is_some());
        assert_eq!(db.list_recent_conflicts(10).unwrap().len(), 1);
    }

    /// The projection had not caught up, and Dropbox refuses the rev: the same outcome, both kept.
    #[test]
    fn an_edit_dropbox_refuses_as_stale_keeps_both() {
        let (dir, db) = db();
        let (item_id, base) = with_file(&db);
        let (bytes, _) = staged(&dir, b"my edit");
        let fake = Fake::answering_all(vec![
            Err(AppError::Dropbox {
                status: 409,
                message: r#"files/upload: {"error_summary": "path/conflict/file/..", "error": {}}"#
                    .into(),
            }),
            // What Dropbox holds now: someone else's edit.
            Ok(file_with("/Docs/a.txt", "id:A", "r8", &"8".repeat(64))),
            Ok(file_with(
                "/Docs/a (conflicted copy 1).txt",
                "id:C",
                "r9",
                &"9".repeat(64),
            )),
        ]);
        let change = Change {
            content: Some((bytes.as_path(), &base)),
            ..Change::default()
        };

        assert_eq!(
            modify(&db, &fake, item_id, &change),
            WriteOutcome::Conflict { item_id }
        );
        assert_eq!(fake.paths.borrow().len(), 3);
        let fake = Fake::answering_all(vec![
            Err(conflict_409("path/conflict/file/..")),
            Err(AppError::Network("down".into())),
        ]);
        assert_eq!(
            modify(&db, &fake, item_id, &change),
            WriteOutcome::Failed(WriteFailure::Offline),
            "unknown is not a conflict: found in review"
        );
    }

    #[test]
    fn a_rename_moves_the_item_and_keeps_its_id() {
        let (_dir, db) = db();
        let (item_id, _) = with_file(&db);
        let fake = Fake::answering(Ok(file_with("/Docs/b.txt", "id:A", "r1", &"1".repeat(64))));
        let change = Change {
            place: Some((docs(&db), "b.txt")),
            ..Change::default()
        };

        assert_eq!(
            modify(&db, &fake, item_id, &change),
            WriteOutcome::Written { item_id }
        );

        assert_eq!(
            *fake.paths.borrow(),
            vec!["move /Docs/a.txt /Docs/b.txt".to_string()]
        );
        assert_eq!(db.projection_item_id("/docs/b.txt"), Some(item_id));
    }

    /// A child of a folder renamed here is written at the folder's new name before the delta's echo
    /// rewrites its stored path: a write to the old path would recreate the old folder on Dropbox.
    #[test]
    fn a_write_under_a_folder_renamed_here_uses_the_new_name() {
        let (_dir, db) = db();
        let (item_id, _) = with_file(&db);
        let docs_id = docs(&db);
        let rename = Fake::answering(Ok(entry("folder", "/Papers", "id:D")));
        modify(
            &db,
            &rename,
            docs_id,
            &Change {
                place: Some((0, "Papers")),
                ..Change::default()
            },
        );

        assert_eq!(
            db.projection_write_target(item_id).unwrap().unwrap().path,
            "/Papers/a.txt"
        );
    }

    #[test]
    fn a_delete_names_the_rev_the_system_had() {
        let (_dir, db) = db();
        let (item_id, base) = with_file(&db);
        let fake = Fake::answering(Ok(entry("file", "/unused", "id:unused")));

        assert_eq!(delete(&db, &fake, item_id, &base), WriteOutcome::Deleted);

        assert_eq!(
            *fake.paths.borrow(),
            vec!["delete /Docs/a.txt r1".to_string()]
        );
        assert!(db.projection_write_target(item_id).unwrap().is_none());
    }

    /// A delete racing an edit on Dropbox: Dropbox keeps the edit and the system restores the file.
    ///
    /// Proof this test can fail: without the version check the delete is sent.
    #[test]
    fn a_delete_of_a_version_dropbox_no_longer_has_is_rejected_and_sends_nothing() {
        let (_dir, db) = db();
        let (item_id, _) = with_file(&db);
        let fake = Fake::answering(Ok(entry("file", "/unused", "id:unused")));

        assert_eq!(
            delete(&db, &fake, item_id, "c1:stale"),
            WriteOutcome::DeletionRejected
        );

        assert!(fake.paths.borrow().is_empty());
        assert!(db.projection_write_target(item_id).unwrap().is_some());
    }

    #[test]
    fn a_delete_dropbox_refuses_for_its_rev_is_rejected() {
        let (_dir, db) = db();
        let (item_id, base) = with_file(&db);
        let fake = Fake::answering(Err(AppError::Dropbox {
            status: 409,
            message: r#"files/delete_v2: {"error_summary": "path_lookup/..", "error": {}}"#.into(),
        }));

        assert_eq!(
            delete(&db, &fake, item_id, &base),
            WriteOutcome::DeletionRejected
        );
    }

    #[test]
    fn a_delete_of_something_already_gone_succeeds() {
        let (_dir, db) = db();
        let (item_id, base) = with_file(&db);
        let gone = Fake::answering(Err(AppError::Dropbox {
            status: 409,
            message:
                r#"files/delete_v2: {"error_summary": "path_lookup/not_found/..", "error": {}}"#
                    .into(),
        }));
        assert_eq!(delete(&db, &gone, item_id, &base), WriteOutcome::Deleted);

        let unknown = Fake::answering_all(vec![]);
        assert_eq!(delete(&db, &unknown, 9_999, "x"), WriteOutcome::Deleted);
        assert!(unknown.paths.borrow().is_empty());
    }

    #[test]
    fn a_conflicted_copy_keeps_the_extension_and_the_folder() {
        let at = chrono::DateTime::parse_from_rfc3339("2026-11-08T10:11:12Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(
            conflicted_copy_path("/Docs/a.b.txt", at),
            "/Docs/a.b (conflicted copy 20261108101112).txt"
        );
        assert_eq!(
            conflicted_copy_path("/README", at),
            "/README (conflicted copy 20261108101112)"
        );
        assert_eq!(
            conflicted_copy_path("/.env", at),
            "/.env (conflicted copy 20261108101112)"
        );
    }

    // ── review findings (slice 4) ────────────────────────────────────────────────────────────

    fn conflict_409(summary: &str) -> AppError {
        AppError::Dropbox {
            status: 409,
            message: format!(r#"x: {{"error_summary": "{summary}", "error": {{}}}}"#),
        }
    }

    /// A staged file with `bytes`, and its Dropbox content hash.
    fn staged(dir: &tempfile::TempDir, bytes: &[u8]) -> (std::path::PathBuf, String) {
        let path = dir.path().join("staged.up");
        std::fs::write(&path, bytes).unwrap();
        let (hash, _, _) = crate::path_util::hash_file(&path).unwrap();
        (path, hash)
    }

    /// Found in review: `move_v2` names a taken destination `to/conflict`, which was retried forever.
    ///
    /// Proof this test can fail: matching `path/conflict` only.
    #[test]
    fn a_rename_onto_a_taken_name_is_a_collision() {
        let (_dir, db) = db();
        let (item_id, _) = with_file(&db);
        let fake = Fake::answering(Err(conflict_409("to/conflict/file/..")));

        let change = Change { place: Some((docs(&db), "b.txt")), content: None };

        assert_eq!(modify(&db, &fake, item_id, &change), WriteOutcome::Collision);
    }

    /// Found in review: an edit of a file deleted on Dropbox, or a create in a folder deleted there, was
    /// answered "retry" forever; the exchange now answers `noSuchItem`, and the system re-creates it.
    ///
    /// Proof this test can fail: answering `Retry` for a tombstone.
    #[test]
    fn a_write_against_something_deleted_on_dropbox_is_not_live() {
        let (dir, db) = db();
        let (item_id, base) = with_file(&db);
        let (bytes, _) = staged(&dir, b"edit");
        db.project_delta(&[DropboxEntry { tag: "deleted".into(), ..entry("file", "/Docs/a.txt", "x") }])
            .unwrap();
        let fake = Fake::answering_all(vec![]);

        let edit = Change { place: None, content: Some((bytes.as_path(), base.as_str())) };
        assert_eq!(modify(&db, &fake, item_id, &edit), WriteOutcome::NotLive);
        let parent = docs(&db);
        db.project_delta(&[DropboxEntry { tag: "deleted".into(), ..entry("folder", "/Docs", "x") }])
            .unwrap();
        assert_eq!(create(&db, &fake, parent, "b.txt", false, &bytes), WriteOutcome::ParentNotLive);
        assert_eq!(create(&db, &fake, 9_999, "b.txt", false, &bytes), WriteOutcome::Retry);
        assert!(fake.paths.borrow().is_empty());
    }

    /// Found in the second review: a move into a folder deleted on Dropbox named the item gone, and the
    /// system dropped a live file; a move where selective sync hides things landed and then looped.
    ///
    /// Proof this test can fail: answering `NotLive` for the parent, or not asking the filter.
    #[test]
    fn a_move_into_a_deleted_or_hidden_place_names_the_place() {
        let (_dir, db) = db();
        let (item_id, _) = with_file(&db);
        db.project_delta(&[entry("folder", "/Gone", "id:G")]).unwrap();
        let gone = db.projection_item_id("/gone").unwrap();
        db.project_delta(&[DropboxEntry { tag: "deleted".into(), ..entry("folder", "/Gone", "x") }]).unwrap();
        let fake = Fake::answering_all(vec![]);

        let into_gone = Change { place: Some((gone, "a.txt")), content: None };
        assert_eq!(modify(&db, &fake, item_id, &into_gone), WriteOutcome::ParentNotLive);

        db.set_app_config("include_prefixes_csv", "Docs/Keep").unwrap();
        let out_of_scope = Change { place: Some((0, "a.txt")), content: None };
        assert_eq!(modify(&db, &fake, item_id, &out_of_scope), WriteOutcome::Hidden);
        assert!(fake.paths.borrow().is_empty());
    }

    /// Found in review: a create the system sent again after the host made it (the extension had given
    /// up waiting) collided, and the system kept a second copy under a new name.
    ///
    /// Proof this test can fail: uploading without looking for the item first.
    #[test]
    fn a_create_sent_again_after_it_landed_is_the_same_item() {
        let (dir, db) = db();
        let (bytes, hash) = staged(&dir, b"same bytes");
        db.project_delta(&[file_with("/Docs/a.txt", "id:A", "r1", &hash)]).unwrap();
        let existing = db.projection_item_id("/docs/a.txt").unwrap();
        let fake = Fake::answering_all(vec![]);

        let outcome = create(&db, &fake, docs(&db), "A.TXT", false, &bytes);

        assert_eq!(outcome, WriteOutcome::Written { item_id: existing });
        assert!(fake.paths.borrow().is_empty());
        let (other, _) = staged(&dir, b"other bytes");
        let fake = Fake::answering(Err(conflict_409("path/conflict/file/..")));
        assert_eq!(create(&db, &fake, docs(&db), "a.txt", false, &other), WriteOutcome::Collision);
    }

    /// Found in review: an edit sent again after it landed found a newer rev and became a conflicted
    /// copy of itself.
    ///
    /// Proof this test can fail: comparing the base version only.
    #[test]
    fn an_edit_sent_again_after_it_landed_writes_nothing() {
        let (dir, db) = db();
        let (bytes, hash) = staged(&dir, b"the edit");
        db.project_delta(&[file_with("/Docs/a.txt", "id:A", "r2", &hash)]).unwrap();
        let item_id = db.projection_item_id("/docs/a.txt").unwrap();
        let fake = Fake::answering_all(vec![]);

        let edit = Change { place: None, content: Some((bytes.as_path(), "an older version")) };

        assert_eq!(modify(&db, &fake, item_id, &edit), WriteOutcome::Written { item_id });
        assert!(fake.paths.borrow().is_empty());
    }

    /// Found in review: a recursive delete of a folder takes what Finder never showed with it (DBSYNC-114
    /// refuses that for the sync folder).
    ///
    /// Proof this test can fail: deleting the folder without looking under it.
    #[test]
    fn a_folder_holding_content_finder_never_showed_is_not_deleted() {
        let (_dir, db) = db();
        // A file whose folder the projection does not hold yet: an orphan under /Docs.
        db.project_delta(&[entry("file", "/Docs/sub/x.txt", "id:X")]).unwrap();
        let fake = Fake::answering_all(vec![]);

        assert_eq!(delete(&db, &fake, docs(&db), ""), WriteOutcome::DeletionRejected);
        assert!(fake.paths.borrow().is_empty());
    }

    /// Found in review: a file created where selective sync hides files landed on Dropbox, and its
    /// hidden row looped the system forever.
    ///
    /// Proof this test can fail: uploading without asking the filter.
    #[test]
    fn a_create_selective_sync_would_hide_is_refused() {
        let (dir, db) = db();
        db.set_app_config("include_prefixes_csv", "Docs/Keep").unwrap();
        let (bytes, _) = staged(&dir, b"x");
        let fake = Fake::answering_all(vec![]);

        assert_eq!(create(&db, &fake, docs(&db), "loose.txt", false, &bytes), WriteOutcome::Hidden);
        assert!(fake.paths.borrow().is_empty());
    }

    /// Found in review: after a folder rename made in Finder, the rows under it kept the old path until
    /// the echo, so deleting one of them matched nothing and it stayed live.
    ///
    /// Proof this test can fail: not moving the paths under the folder.
    #[test]
    fn the_rows_under_a_folder_renamed_here_follow_it() {
        let (_dir, db) = db();
        let (file_id, base) = with_file(&db);
        let docs_id = docs(&db);
        let fake = Fake::answering_all(vec![
            Ok(entry("folder", "/Papers", "id:D")),
            Ok(entry("file", "/unused", "id:unused")),
        ]);

        let rename = Change { place: Some((0, "Papers")), content: None };
        assert_eq!(modify(&db, &fake, docs_id, &rename), WriteOutcome::Written { item_id: docs_id });
        assert_eq!(db.projection_path_lower(file_id).unwrap().as_deref(), Some("/papers/a.txt"));

        assert_eq!(delete(&db, &fake, file_id, &base), WriteOutcome::Deleted);
        assert_eq!(
            db.projection_row_state(file_id).unwrap(),
            Some(crate::storage::db::projection::STATE_DELETED)
        );
    }

    /// Found in the third review: an edit whose answer was lost landed before the delta caught up; the
    /// retry, refused for the old rev, became a conflicted copy of itself.
    ///
    /// Proof this test can fail: comparing with the row's hash only.
    #[test]
    fn an_edit_sent_again_before_the_delta_caught_up_writes_nothing() {
        let (dir, db) = db();
        let (item_id, base) = with_file(&db);
        let (bytes, hash) = staged(&dir, b"the edit");
        let fake = Fake::answering_all(vec![
            Err(conflict_409("path/conflict/file/..")),
            Ok(file_with("/Docs/a.txt", "id:A", "r2", &hash)),
        ]);

        let edit = Change { place: None, content: Some((bytes.as_path(), base.as_str())) };

        assert_eq!(modify(&db, &fake, item_id, &edit), WriteOutcome::Written { item_id });
        assert_eq!(fake.paths.borrow().len(), 2, "no conflicted copy: {:?}", fake.paths.borrow());
    }

    // ── the breaker ─────────────────────────────────────────────────────────────────────────────

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[derive(Default)]
    struct Decisions {
        holds: AtomicUsize,
        counted: AtomicUsize,
        /// 0 none, 1 confirm, 2 restore.
        answer: AtomicUsize,
        released: AtomicUsize,
    }

    impl Decisions {
        fn confirm(&self) {
            self.answer.store(1, Ordering::SeqCst);
        }
        fn restore(&self) {
            self.answer.store(2, Ordering::SeqCst);
        }
    }

    impl DeleteDecisions for Decisions {
        fn hold(&self, weight: usize, _tracked: usize) {
            self.holds.fetch_add(1, Ordering::SeqCst);
            self.counted.store(weight, Ordering::SeqCst);
        }
        fn recount(&self, weight: usize, _tracked: usize) {
            self.counted.store(weight, Ordering::SeqCst);
        }
        fn answer(&self) -> Option<bool> {
            match self.answer.swap(0, Ordering::SeqCst) {
                1 => Some(true),
                2 => Some(false),
                _ => None,
            }
        }
        fn release(&self) {
            self.released.fetch_add(1, Ordering::SeqCst);
        }
    }

    const FAST: DeletePolicy = DeletePolicy {
        settle: Duration::from_millis(80),
        stream_gap: Duration::from_millis(120),
        window: Duration::from_millis(600),
        patience: Duration::from_secs(5),
        poll: Duration::from_millis(5),
    };

    /// Concurrent deletes of `items`, `weight` each, against `tracked` files, as Finder sends a selection.
    fn burst_of(
        guard: &Arc<DeleteGuard>,
        decisions: &Arc<Decisions>,
        items: impl IntoIterator<Item = i64>,
        weight: usize,
        tracked: usize,
    ) -> Vec<std::thread::JoinHandle<Admission>> {
        items
            .into_iter()
            .map(|item| {
                let (guard, decisions) = (guard.clone(), decisions.clone());
                std::thread::spawn(move || {
                    guard.admit(item, weight, tracked, &FAST, &*decisions, &Instant::now, &std::thread::sleep)
                })
            })
            .collect()
    }

    /// Items 1..=n.
    fn burst(
        guard: &Arc<DeleteGuard>,
        decisions: &Arc<Decisions>,
        n: i64,
        weight: usize,
        tracked: usize,
    ) -> Vec<std::thread::JoinHandle<Admission>> {
        burst_of(guard, decisions, 1..=n, weight, tracked)
    }

    fn results(handles: Vec<std::thread::JoinHandle<Admission>>) -> Vec<Admission> {
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    }

    fn pair() -> (Arc<DeleteGuard>, Arc<Decisions>) {
        (Arc::new(DeleteGuard::new()), Arc::new(Decisions::default()))
    }

    /// A burst of 30 that the user answers.
    fn decided(confirm: bool) -> (Arc<DeleteGuard>, Arc<Decisions>) {
        let (guard, decisions) = pair();
        let handles = burst(&guard, &decisions, 30, 1, 100);
        std::thread::sleep(Duration::from_millis(300));
        if confirm {
            decisions.confirm();
        } else {
            decisions.restore();
        }
        results(handles);
        (guard, decisions)
    }

    #[test]
    fn a_small_burst_goes_through_without_a_question() {
        let (guard, decisions) = pair();

        let out = results(burst(&guard, &decisions, 5, 1, 1000));

        assert!(out.iter().all(|a| *a == Admission::Proceed));
        assert_eq!(decisions.holds.load(Ordering::SeqCst), 0);
    }

    /// Nothing of a mass deletion reaches Dropbox before the user confirms: every call of the burst
    /// waits, the first ones included.
    ///
    /// Proof this test can fail: deciding per call instead of after the burst settles lets the first
    /// calls through before the count reaches the threshold.
    #[test]
    fn a_mass_deletion_waits_whole_until_confirmed() {
        let (guard, decisions) = pair();

        let handles = burst(&guard, &decisions, 30, 1, 100);
        std::thread::sleep(Duration::from_millis(300));
        assert!(handles.iter().all(|h| !h.is_finished()), "nothing may go before the user decides");
        assert_eq!(decisions.holds.load(Ordering::SeqCst), 1);
        decisions.confirm();

        let out = results(handles);
        assert!(out.iter().all(|a| *a == Admission::ProceedConfirmed));
        assert_eq!(decisions.released.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_refused_mass_deletion_is_refused_whole() {
        let (guard, decisions) = pair();

        let handles = burst(&guard, &decisions, 30, 1, 100);
        std::thread::sleep(Duration::from_millis(300));
        decisions.restore();

        assert!(results(handles).iter().all(|a| *a == Admission::Refuse));
    }

    /// A straggler of the selection, arriving just after the answer, takes it.
    ///
    /// Proof this test can fail: dropping the straggler window lets it through.
    #[test]
    fn a_straggler_of_the_selection_takes_the_answer() {
        let (guard, decisions) = pair();
        let handles = burst(&guard, &decisions, 30, 1, 100);
        std::thread::sleep(FAST.settle + Duration::from_millis(20));
        decisions.restore();
        results(handles);

        let late = guard.admit(31, 1, 100, &FAST, &*decisions, &Instant::now, &std::thread::sleep);

        assert_eq!(late, Admission::Refuse);
    }

    /// Found in review: every returning retry moved the straggler window, so retries kept it open and an
    /// unrelated delete took a refusal meant for others. The answer ends when the stream pauses.
    ///
    /// Proof this test can fail: a stream that never ends refuses the user's own later delete.
    #[test]
    fn a_delete_after_the_window_is_its_own_question() {
        let (guard, decisions) = decided(false);
        std::thread::sleep(FAST.settle * 3);

        let own = guard.admit(999, 1, 100, &FAST, &*decisions, &Instant::now, &std::thread::sleep);

        assert_eq!(own, Admission::Proceed);
    }

    /// Found in review: a confirmed item's retry, followed at once by a new selection, sent the new
    /// selection to Dropbox without a question.
    ///
    /// Proof this test can fail: letting the retry move the straggler window.
    #[test]
    fn a_retry_of_a_confirmed_item_does_not_confirm_a_new_selection() {
        let (guard, decisions) = decided(true);
        guard.unfinished(1);
        std::thread::sleep(FAST.settle * 3);

        let retry = guard.admit(1, 1, 100, &FAST, &*decisions, &Instant::now, &std::thread::sleep);
        assert_eq!(retry, Admission::ProceedConfirmed);
        let fresh = burst_of(&guard, &decisions, 101..=130, 1, 100);
        std::thread::sleep(Duration::from_millis(300));

        assert!(fresh.iter().all(|h| !h.is_finished()), "the new selection is held");
        assert_eq!(decisions.holds.load(Ordering::SeqCst), 2);
        decisions.restore();
        assert!(results(fresh).iter().all(|a| *a == Admission::Refuse));
    }

    /// Measured live: after Restore files, deleting the same 50 files again was refused without a
    /// question, every time. Deleting a restored selection again is a new question.
    ///
    /// Proof this test can fail: keeping the answer until a quiet period refuses the second burst.
    #[test]
    fn deleting_a_refused_selection_again_asks_again() {
        let (guard, decisions) = decided(false);
        std::thread::sleep(FAST.settle * 3);

        let again = burst(&guard, &decisions, 30, 1, 100);
        std::thread::sleep(Duration::from_millis(300));
        assert!(again.iter().all(|h| !h.is_finished()), "the second burst is held, not answered");
        assert_eq!(decisions.holds.load(Ordering::SeqCst), 2);
        decisions.confirm();

        assert!(results(again).iter().all(|a| *a == Admission::ProceedConfirmed));
    }

    /// An item that gave up waiting takes the answer when the system retries it, however late, and an
    /// unrelated delete in between does not make it forget.
    ///
    /// Proof this test can fail: not remembering the item, or clearing what is remembered when a new
    /// burst starts.
    #[test]
    fn an_item_that_gave_up_waiting_takes_the_answer_on_its_retry() {
        let (guard, decisions) = pair();
        let impatient = DeletePolicy {
            patience: Duration::from_millis(50),
            ..FAST
        };
        let out = guard.admit(7, 300, 1000, &impatient, &*decisions, &Instant::now, &std::thread::sleep);
        assert_eq!(out, Admission::StillHeld);
        let waiting = burst_of(&guard, &decisions, [8], 1, 1000);
        std::thread::sleep(Duration::from_millis(100));
        decisions.confirm();
        assert_eq!(results(waiting), vec![Admission::ProceedConfirmed]);
        std::thread::sleep(FAST.settle * 3);
        let unrelated = guard.admit(50, 1, 1000, &FAST, &*decisions, &Instant::now, &std::thread::sleep);
        assert_eq!(unrelated, Admission::Proceed);

        let retry = guard.admit(7, 300, 1000, &FAST, &*decisions, &Instant::now, &std::thread::sleep);

        assert_eq!(retry, Admission::ProceedConfirmed);
        assert_eq!(decisions.holds.load(Ordering::SeqCst), 1, "no second question");
    }

    /// Measured live: 40 of 50 confirmed deletes answered 429 and came back as a new burst, which asked
    /// the user again, twice. Found in review: an unrelated delete meanwhile made the guard forget them.
    ///
    /// Proof this test can fail: not remembering unfinished items, or forgetting them on a new burst.
    #[test]
    fn a_confirmed_delete_that_did_not_finish_is_not_asked_about_again() {
        let (guard, decisions) = decided(true);
        for item in 1..=30 {
            guard.unfinished(item);
        }
        std::thread::sleep(FAST.settle * 3);
        let unrelated = guard.admit(500, 1, 100, &FAST, &*decisions, &Instant::now, &std::thread::sleep);
        assert_eq!(unrelated, Admission::Proceed);

        let retries = burst(&guard, &decisions, 30, 1, 100);

        assert!(results(retries).iter().all(|a| *a == Admission::ProceedConfirmed));
        assert_eq!(decisions.holds.load(Ordering::SeqCst), 1, "asked once");
    }

    /// Measured live: the banner said 32 of a 50-file selection. The rest join the held burst, and the
    /// count follows them; found in review, a retry of a held item was counted twice.
    ///
    /// Proof this test can fail: not counting calls that join a held burst, or counting an item twice.
    #[test]
    fn the_count_follows_the_selection_once_per_item() {
        let (guard, decisions) = pair();
        let first = burst(&guard, &decisions, 30, 1, 100);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(decisions.counted.load(Ordering::SeqCst), 30);

        let more = burst_of(&guard, &decisions, 25..=50, 1, 100);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(decisions.counted.load(Ordering::SeqCst), 50);
        decisions.confirm();
        results(first);
        results(more);
    }

    /// Found in review: a call of a small burst still settling was pulled into a later burst that
    /// started after it, and held or refused with it.
    ///
    /// Proof this test can fail: judging a call by whatever burst is current instead of its own.
    #[test]
    fn a_call_is_judged_with_its_own_burst() {
        let (guard, decisions) = pair();
        let slow_sleep = |d: Duration| std::thread::sleep(d * 40);
        let small = {
            let (guard, decisions) = (guard.clone(), decisions.clone());
            std::thread::spawn(move || {
                guard.admit(1, 1, 100, &FAST, &*decisions, &Instant::now, &slow_sleep)
            })
        };
        std::thread::sleep(FAST.settle * 2);
        let big = burst_of(&guard, &decisions, 100..=130, 1, 100);

        assert_eq!(small.join().unwrap(), Admission::Proceed);
        std::thread::sleep(Duration::from_millis(300));
        decisions.restore();
        assert!(results(big).iter().all(|a| *a == Admission::Refuse));
    }

    /// Found in the second review: a delete arriving after a mass deletion settled, before any of its
    /// calls looked at it, started a new burst, and the mass deletion's calls all went through unasked.
    ///
    /// Proof this test can fail: starting a new burst before the settled one is judged.
    #[test]
    fn a_mass_deletion_is_judged_before_another_burst_starts() {
        let (guard, decisions) = pair();
        let slow = |d: Duration| std::thread::sleep(d * 40);
        let mass: Vec<_> = (1..=30)
            .map(|item| {
                let (guard, decisions) = (guard.clone(), decisions.clone());
                std::thread::spawn(move || guard.admit(item, 1, 100, &FAST, &*decisions, &Instant::now, &slow))
            })
            .collect();
        std::thread::sleep(FAST.settle + Duration::from_millis(40));
        let other = burst_of(&guard, &decisions, [999], 1, 100);
        std::thread::sleep(Duration::from_millis(600));

        assert_eq!(decisions.holds.load(Ordering::SeqCst), 1, "the mass deletion is asked about");
        decisions.restore();
        assert!(results(mass).iter().all(|a| *a == Admission::Refuse));
        assert_eq!(results(other), vec![Admission::Refuse], "it arrived before the burst was judged");
    }

    /// Measured live: a 300-file selection arrived as 32 calls, then one per call answered. The answer
    /// covers the rest of the selection, however long, while it keeps coming.
    ///
    /// Proof this test can fail: capping what one answer covers (the second review's rule) asked again,
    /// or let the rest of a refused selection through.
    #[test]
    fn the_answer_covers_the_rest_of_the_selection_as_slots_free() {
        for confirm in [false, true] {
            let (guard, decisions) = pair();
            let head = burst(&guard, &decisions, 32, 1, 100_000);
            std::thread::sleep(Duration::from_millis(300));
            assert_eq!(decisions.holds.load(Ordering::SeqCst), 1);
            if confirm {
                decisions.confirm();
            } else {
                decisions.restore();
            }
            results(head);

            let rest: Vec<Admission> = (33..=300)
                .map(|item| {
                    // One Dropbox delete between two arrivals when confirmed, none when refused.
                    if confirm {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    guard.admit(item, 1, 100_000, &FAST, &*decisions, &Instant::now, &std::thread::sleep)
                })
                .collect();

            assert!(rest.iter().all(|a| *a == answer_for(confirm)), "{confirm}: {rest:?}");
            assert_eq!(decisions.holds.load(Ordering::SeqCst), 1, "asked once");
        }
    }

    /// A confirmed selection arrives one item per finished Dropbox delete, which can take longer than
    /// `settle`; it is still the same selection.
    ///
    /// Proof this test can fail: ending the stream at `settle` sends the slow rest as new bursts.
    #[test]
    fn a_slow_confirmed_stream_keeps_its_answer() {
        let (guard, decisions) = decided(true);
        let gap = FAST.settle + (FAST.stream_gap - FAST.settle) / 2;

        let rest: Vec<Admission> = (100..105)
            .map(|item| {
                std::thread::sleep(gap);
                guard.admit(item, 1, 100, &FAST, &*decisions, &Instant::now, &std::thread::sleep)
            })
            .collect();

        assert!(rest.iter().all(|a| *a == Admission::ProceedConfirmed), "{rest:?}");
    }

    /// Measured live: in an account of 440 files the first 32 of a 300-file selection were under the
    /// threshold of 44 and went to Dropbox before the question; with 2,000 files or more the one-by-one
    /// stream that follows can never reach 200 within the window, so nothing would have been asked.
    /// A burst that fills every slot is held whatever its size; one short of that is judged as before.
    ///
    /// Proof this test can fail: judging a full burst on the threshold alone lets it through.
    #[test]
    fn a_burst_that_fills_every_slot_is_held_below_the_threshold() {
        let (guard, decisions) = pair();
        let short = results(burst(&guard, &decisions, SATURATED_BURST as i64 - 1, 1, 100_000));
        assert!(short.iter().all(|a| *a == Admission::Proceed));
        std::thread::sleep(FAST.window + FAST.settle);

        let full = burst_of(&guard, &decisions, 1000..1000 + SATURATED_BURST as i64, 1, 100_000);
        std::thread::sleep(Duration::from_millis(300));

        assert!(full.iter().all(|h| !h.is_finished()), "held, not sent");
        assert_eq!(decisions.holds.load(Ordering::SeqCst), 1);
        decisions.restore();
        assert!(results(full).iter().all(|a| *a == Admission::Refuse));
    }

    /// Found in the second review: an answer given while every call had given up was taken by the next
    /// delete to arrive, however unrelated, and confirmed it.
    ///
    /// Proof this test can fail: letting a newcomer join the held burst before settling it.
    #[test]
    fn an_answer_nobody_waited_for_settles_its_own_burst_only() {
        let (guard, decisions) = pair();
        let impatient = DeletePolicy {
            patience: Duration::from_millis(50),
            ..FAST
        };
        let gave_up: Vec<Admission> = std::thread::scope(|scope| {
            let calls: Vec<_> = (1..=30)
                .map(|item| {
                    let (guard, decisions, impatient) = (&guard, &decisions, &impatient);
                    scope.spawn(move || {
                        guard.admit(item, 1, 100, impatient, &**decisions, &Instant::now, &std::thread::sleep)
                    })
                })
                .collect();
            calls.into_iter().map(|c| c.join().unwrap()).collect()
        });
        assert!(gave_up.iter().all(|a| *a == Admission::StillHeld));
        decisions.confirm();

        let unrelated = burst_of(&guard, &decisions, 200..=230, 1, 100);
        std::thread::sleep(Duration::from_millis(300));
        assert!(unrelated.iter().all(|h| !h.is_finished()), "a new question, not the old answer");
        assert_eq!(decisions.holds.load(Ordering::SeqCst), 2);
        let retry = guard.admit(5, 1, 100, &FAST, &*decisions, &Instant::now, &std::thread::sleep);
        assert_eq!(retry, Admission::ProceedConfirmed, "the old burst got its answer");
        decisions.restore();
        assert!(results(unrelated).iter().all(|a| *a == Admission::Refuse));
    }

    /// Found in the third review: after Restore with every call given up, the first retry to arrive
    /// was judged as a new burst and deleted.
    ///
    /// Proof this test can fail: dropping the arrival out of the burst it belongs to.
    #[test]
    fn the_first_retry_after_an_answer_nobody_waited_for_takes_it() {
        let (guard, decisions) = pair();
        let impatient = DeletePolicy {
            patience: Duration::from_millis(50),
            ..FAST
        };
        std::thread::scope(|scope| {
            let calls: Vec<_> = (1..=30)
                .map(|item| {
                    let (guard, decisions, impatient) = (&guard, &decisions, &impatient);
                    scope.spawn(move || {
                        guard.admit(item, 1, 100, impatient, &**decisions, &Instant::now, &std::thread::sleep)
                    })
                })
                .collect();
            for call in calls {
                call.join().unwrap();
            }
        });
        decisions.restore();

        let first = guard.admit(5, 1, 100, &FAST, &*decisions, &Instant::now, &std::thread::sleep);
        let second = guard.admit(6, 1, 100, &FAST, &*decisions, &Instant::now, &std::thread::sleep);

        assert_eq!((first, second), (Admission::Refuse, Admission::Refuse));
    }

    /// Found in the third review: small selections made one after another add up within the window.
    ///
    /// Proof this test can fail: judging each burst alone.
    #[test]
    fn waves_of_one_selection_add_up() {
        let (guard, decisions) = pair();
        let first = results(burst_of(&guard, &decisions, 1..=15, 1, 100));
        assert!(first.iter().all(|a| *a == Admission::Proceed));
        std::thread::sleep(FAST.settle * 2);

        let second = burst_of(&guard, &decisions, 16..=30, 1, 100);
        std::thread::sleep(Duration::from_millis(300));

        assert_eq!(decisions.holds.load(Ordering::SeqCst), 1);
        assert_eq!(decisions.counted.load(Ordering::SeqCst), 30, "the banner counts both waves");
        decisions.restore();
        results(second);
    }

    /// Found in the fourth review: what the window held stayed after a confirmation, so ordinary deletes
    /// right after it were asked about again and again.
    ///
    /// Proof this test can fail: keeping the window after a confirmation.
    #[test]
    fn an_ordinary_delete_after_a_confirmation_is_not_asked_about() {
        let (guard, decisions) = pair();
        let policy = DeletePolicy {
            window: Duration::from_secs(30),
            ..FAST
        };
        let wave = |items: std::ops::RangeInclusive<i64>, answer: Option<bool>| -> Vec<Admission> {
            std::thread::scope(|scope| {
                let calls: Vec<_> = items
                    .map(|item| {
                        let (guard, decisions, policy) = (&guard, &decisions, &policy);
                        scope.spawn(move || {
                            guard.admit(item, 1, 100, policy, &**decisions, &Instant::now, &std::thread::sleep)
                        })
                    })
                    .collect();
                if let Some(confirm) = answer {
                    std::thread::sleep(Duration::from_millis(300));
                    if confirm {
                        decisions.confirm();
                    } else {
                        decisions.restore();
                    }
                }
                calls.into_iter().map(|c| c.join().unwrap()).collect()
            })
        };
        // 20 let through, then 10 that tip the window into a question, then 20 more of the selection:
        // the window, kept, would tip 7 more over the threshold of 25.
        assert!(wave(1..=20, None).iter().all(|a| *a == Admission::Proceed));
        std::thread::sleep(FAST.settle * 2);
        assert!(wave(21..=30, Some(true)).iter().all(|a| *a == Admission::ProceedConfirmed));
        assert!(wave(41..=60, None).iter().all(|a| *a == Admission::ProceedConfirmed), "stragglers");
        std::thread::sleep(FAST.settle * 2);

        let later = wave(70..=76, None);

        assert!(later.iter().all(|a| *a == Admission::Proceed), "{later:?}");
        assert_eq!(decisions.holds.load(Ordering::SeqCst), 1);
    }

    /// A folder counts as the files it takes: one call can be a mass deletion.
    #[test]
    fn one_folder_holding_many_files_is_held() {
        let (guard, decisions) = pair();

        let handles = burst(&guard, &decisions, 1, 250, 1000);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(decisions.holds.load(Ordering::SeqCst), 1);
        decisions.confirm();

        assert_eq!(results(handles), vec![Admission::ProceedConfirmed]);
    }

    #[test]
    fn a_held_call_gives_up_after_its_patience_and_stays_held() {
        let guard = DeleteGuard::new();
        let decisions = Decisions::default();
        let policy = DeletePolicy {
            patience: Duration::from_millis(50),
            ..FAST
        };

        let out = guard.admit(1, 300, 1000, &policy, &decisions, &Instant::now, &std::thread::sleep);

        assert_eq!(out, Admission::StillHeld);
        let again = guard.admit(2, 1, 1000, &policy, &decisions, &Instant::now, &std::thread::sleep);
        assert_eq!(again, Admission::StillHeld, "a retry joins the held burst");
    }

    #[test]
    fn a_folder_weighs_the_files_under_it() {
        let (_dir, db) = db();
        let (file_id, _) = with_file(&db);
        db.project_delta(&[file_with("/Docs/b.txt", "id:B", "r1", &"1".repeat(64))])
            .unwrap();

        assert_eq!(db.projection_delete_weight(docs(&db)).unwrap(), (2, 2));
        assert_eq!(db.projection_delete_weight(file_id).unwrap(), (1, 2));
    }

    #[test]
    fn the_upload_answer_becomes_a_file_entry() {
        let metadata = serde_json::json!({
            "name": "a.txt", "id": "id:A", "path_lower": "/a.txt", "path_display": "/a.txt",
            "rev": "015f", "size": 5, "content_hash": "abc", "server_modified": "2026-11-08T10:00:00Z"
        });

        let entry = entry_from_metadata(metadata, "file").unwrap();

        assert_eq!(entry.tag, "file");
        assert_eq!(entry.id.as_deref(), Some("id:A"));
    }
}
