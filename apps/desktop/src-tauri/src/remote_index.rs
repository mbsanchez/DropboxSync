//! Remote-side observation and reconciliation: turning a Dropbox `list_folder`
//! snapshot or a cursor-delta batch into the local index's and `sync_jobs`' idea
//! of truth.
//!
//! ## The standing invariant
//!
//! Two defects found in this area, eighteen months apart, turned out to be the
//! same mistake wearing different clothes: trusting one piece of remote evidence
//! before checking whether something else in hand contradicts it. Writing the
//! rule down once, here, is what stops a fix for one from quietly reopening the
//! other.
//!
//! > No remote-driven observation may destroy local content or index identity
//! > while the same batch contains contradicting evidence, or while the path has
//! > pending work.
//!
//! The first clause is **DBSYNC-102**: sharing a folder from the Dropbox web UI
//! makes a `list_folder/continue` delta report its existing entries as `deleted`
//! and then immediately re-add them. Applied one entry at a time, the re-add
//! sits in the same batch as its own deletion and is invisible to the code that
//! decides — the "safe remote-wins delete" arm fires anyway and every local copy
//! is lost. The fix (`collapse_delta_entries`, below) resolves that contradiction
//! across the whole batch before any single path's fate is decided. This clause
//! protects against **acting on one path's evidence while a sibling entry in the
//! same batch says otherwise.**
//!
//! The second clause is **DBSYNC-101**, still open: the materialization sweep
//! can plant a `.cloudsc` sidecar over a path that has a pending delete queued
//! against it, silently dropping that delete. This clause protects against
//! **acting on a path while work already in flight for that same path has not
//! finished** — the sidecar write does not check for pending work the way
//! DBSYNC-102's collapse checks for contradicting batch evidence, but both are
//! instances of the same rule: look before you destroy.
//!
//! DBSYNC-101 is fixed **after** DBSYNC-102, deliberately, not merely later in
//! the backlog: DBSYNC-102's fix clears index rows that are stale or wrong, and
//! several of DBSYNC-101's symptoms sit downstream of those rows being missing
//! or stale in the first place. Fixing 101 first would mean building its
//! pending-work check on top of index state this ticket is about to change out
//! from under it. Writing the invariant here, once, in prose, is what keeps
//! DBSYNC-101's eventual fix from re-deciding the batch-contradiction half of
//! this rule differently than DBSYNC-102 already settled it — an unwritten
//! marker gets a different answer from each reader, and two readers separated
//! by months are exactly the case that produces a contradiction nobody notices
//! until it ships.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};

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

/// Collapses one batch of raw delta entries into exactly one surviving entry per
/// path, before any of them reach `apply_delta_entries` (DBSYNC-102 #173).
///
/// This is the fix. `apply_delta_entries` (#172) applies entries one at a time, so
/// when Dropbox reports a path as `deleted` and then immediately re-adds it in the
/// SAME page — exactly what happens when sharing a folder from the web converts it —
/// the removal arm (`reconcile_remote_absent`) only ever sees one path at a time and
/// takes its safe-looking remote-wins delete, with the re-add sitting right next to
/// it invisible until it is too late to retract the already-queued job. Collapsing
/// first removes the contradiction before either arm ever runs.
///
/// The rule bound at the architecture review is a SET predicate over the batch, not a
/// temporal one:
///
/// > A path that appears as both a `deleted` entry and a file entry within the same
/// > batch resolves to the file entry. The `deleted` entry is not actionable.
///
/// Deliberately **order-independent**: this does not implement "the last entry for a
/// path wins". Dropbox's within-page entry ordering is not documented anywhere this
/// project has found, so a last-wins rule would pass every observed sample while
/// quietly depending on an unverified contract. Instead, for every path, ANY entry
/// that resolves to `DeltaAction::Upsert` beats EVERY entry that resolves to
/// `DeltaAction::Remove` for that same path, regardless of which one appears first in
/// `entries`. See the removal/upsert test pair with reversed orderings below.
///
/// The winning upsert applies **unconditionally** — regardless of whether its content
/// hash matches the index's last-synced one. This matters because a genuine remote
/// edit can race a sharing conversion inside the same batch: if the collapse (or a
/// naive fix) dropped both the `deleted` entry AND the upsert whenever a `deleted`
/// entry existed for the path, a real content change would be silently lost instead
/// of merely surviving a false deletion. `reconcile_remote_present` is left to decide
/// whether a download is actually needed by comparing hashes itself, same as always.
///
/// Resolution per path:
///
/// | entries present for the path                  | result                      |
/// |-------------------------------------------------|------------------------------|
/// | `deleted` + `file` (either order)                | the `file` entry → Upsert    |
/// | `file` only                                      | the `file` entry → Upsert    |
/// | `deleted` only                                   | the `deleted` entry → Remove |
/// | `folder`-tagged (or any other unrecognised tag)  | ignored — never enters the map |
///
/// Folder entries are dropped here rather than resolved against anything, which is
/// safe only because `remote_file_index` holds no folder rows at all — DBSYNC-30
/// scoped the cursor-delta index to files only. If that ever changes, this silent
/// drop becomes a correctness bug, not just a missed case; it is written down here so
/// a future reader does not have to rediscover it the hard way. (In practice this
/// case needs no special-casing in the code below: `delta_action_from_entry` already
/// maps a `folder` tag — and anything else it does not recognise — to `Ignore`, and
/// `Ignore` never enters `winners`.)
///
/// Returns **references into `entries`**, not clones or a parallel `(entry, action)`
/// structure, because the Windows-only placeholder-materialisation call inside
/// `apply_delta_entries` needs `path_display`/`size` off the *raw* entry — `DeltaAction`
/// and `RemoteFileMeta` carry neither. Keeping one representation (raw entries,
/// resolved by path) all the way from the HTTP response to the Windows call avoids
/// ending up with both a collapsed shape and a raw shape that could drift apart.
///
/// Note for a future reader who reaches for `Resolution` from outside this function:
/// it does NOT leave this function. `apply_delta_entries` re-derives the action for
/// each surviving entry via a second call to `delta_action_from_entry`, rather than
/// trusting this function's internal `Upsert`/`Remove` tag — deliberately, since
/// trusting it would mean carrying `Resolution` (or an equivalent) across the
/// function boundary, which is exactly the second, parallel representation the
/// paragraph above says not to introduce. The one piece of information this
/// function's classification is NOT allowed to silently lose is "which entries are
/// no longer actionable" (the per-path filtering), not "what their action was" —
/// that part is cheap to recompute and `delta_action_from_entry` is pure, so
/// recomputing it is not a correctness risk, only a second, trivial pass over data
/// already in cache.
///
/// Returned in a deterministic order (lexicographic by the same case-INSENSITIVE key
/// the collapse resolves on — see below) purely so a multi-path batch gives tests a
/// stable `Vec` to assert against; nothing in `apply_delta_entries` depends on enqueue
/// order.
///
/// ## Collapse key is `path_lower`, not `path_display` (DBSYNC-102 review finding)
///
/// Dropbox paths are case-insensitive, and Dropbox's own docs promise correct casing
/// only on `path_display`'s LAST path component — an ANCESTOR folder's casing can
/// legitimately drift between two entries for what both name as the same item (a
/// rename of a parent folder elsewhere in the account, a client that cached an older
/// display casing). Resolving on the case-preserved `rel` that `delta_action_from_entry`
/// produces would then treat `Shared/Doc.txt`'s `deleted` entry and `shared/Doc.txt`'s
/// re-add as two DIFFERENT paths instead of the same one — reopening exactly the
/// DBSYNC-102 bug this function exists to close, just gated on ancestor-casing drift
/// instead of same-batch ordering. The collapse therefore keys on `path_lower`, which
/// Dropbox sends on every entry (`deleted` included) and which is already
/// case-normalized, falling back to a lowercased `path_display` only for a hand-built
/// entry that omits `path_lower` entirely (never observed from Dropbox itself).
///
/// This key is used ONLY to decide which entries are the same path; it is not a
/// parallel representation that could drift from `rel`, and it never reaches
/// `apply_delta_entries` — the winning entry is still applied under the case-preserved
/// `rel` `delta_action_from_entry` derives from *its own* `path_display`, exactly as
/// before. The residual this leaves: when the winning upsert's casing differs from an
/// existing index row's (e.g. the index holds `Shared/Doc.txt` and the winning re-add's
/// `path_display` is `shared/Doc.txt`), `reconcile_remote_present` looks up the new,
/// differently-cased `rel` and finds no previous row, so it records a SECOND,
/// differently-cased row rather than updating the first — no data is lost, but the
/// index can end up holding two case-variant rows for one Dropbox item. That is a
/// pre-existing limitation of the index being case-sensitive while Dropbox is not,
/// not something introduced here, and it is out of scope for this ticket.
///
/// **Scope note:** this collapses one PAGE of one `apply_remote_delta` invocation, not
/// the whole invocation. A `deleted` entry on page N and its re-add on page N+1 are
/// NOT collapsed together by this function alone — accumulating every page of one
/// invocation before collapsing is `apply_remote_delta`'s job (see its doc), which calls
/// this function exactly once per invocation, over every page's accumulated entries.
///
/// ## A genuine delete can be silently suppressed (DBSYNC-102 review finding #3)
///
/// The set predicate this function implements is deliberately "any upsert beats every
/// remove for that path", not "the temporally last entry wins" (see above). That means
/// a batch like `[file X, deleted X]` — a genuine edit immediately followed by a
/// genuine delete of the same path within one invocation — or
/// `[deleted X, file X, deleted X]` resolves to the upsert, and the real, final delete
/// is dropped with no download/no-op in its place: nothing downstream of this function
/// ever learns a deletion was intended.
///
/// **The set predicate stays** — this was the rule bound at architecture review, and
/// the asymmetry is deliberate: this failure mode is a missed delete, which the 300s
/// snapshot sweep (`reconcile_remote_snapshot_with_breaker`) heals on its next pass by
/// re-observing the path's true remote absence. "Last entry wins" would instead risk
/// reopening the DBSYNC-102 bug this function exists to close, if Dropbox ever emits a
/// re-add before its own deletion in the documented-nowhere within-page order. A missed
/// delete that self-heals is the safer failure to accept than a live file deleted by a
/// should-have-been-collapsed contradiction. Note the sweep's healing is itself gated by
/// the DBSYNC-64 mass-deletion circuit breaker — unlike the delta path, which is never
/// gated (see this module's doc on `reconcile_remote_snapshot_with_breaker`) — so a
/// large batch of suppressed deletes can end up waiting on a user override before the
/// sweep actually applies them, rather than healing unattended within one 300s pass.
///
/// What changes here is making the suppression VISIBLE instead of silent: every
/// `deleted` entry whose path resolves to an Upsert is logged at `debug`, naming the
/// path and the winning entry's `rev`; one `info`-level summary with the count fires
/// per batch when any were suppressed, so a log sweep over `info` alone still shows
/// that this happened without needing `debug` enabled.
pub(crate) fn collapse_delta_entries(entries: &[DropboxEntry]) -> Vec<&DropboxEntry> {
    enum Resolution<'a> {
        Upsert(&'a DropboxEntry),
        Remove(&'a DropboxEntry),
    }

    // Case-insensitive collapse key — see the doc above for why this must be
    // `path_lower`, not the case-preserved `rel`. The winning entry's `rel` for
    // actually applying the change is still derived case-preserved, below in
    // `apply_delta_entries`, from the winning raw entry's own `path_display`.
    fn collapse_key(entry: &DropboxEntry) -> Option<Cow<'_, str>> {
        let path_display = entry.path_display.as_deref()?;
        Some(match entry.path_lower.as_deref() {
            Some(lower) => Cow::Borrowed(lower.trim_start_matches('/')),
            None => Cow::Owned(path_display.trim_start_matches('/').to_lowercase()),
        })
    }

    let mut winners: BTreeMap<Cow<'_, str>, Resolution<'_>> = BTreeMap::new();

    for entry in entries {
        let Some(key) = collapse_key(entry) else {
            continue;
        };

        match delta_action_from_entry(entry) {
            DeltaAction::Upsert(..) => {
                // Unconditional: an Upsert always overwrites whatever is currently
                // recorded for this path — including a Remove recorded earlier in
                // this same loop — because the file entry wins regardless of order.
                winners.insert(key, Resolution::Upsert(entry));
            }
            DeltaAction::Remove(_) => {
                // `or_insert`, never a plain overwrite: if an Upsert for this path
                // already won (processed earlier in this same loop), this Remove
                // must NOT downgrade it back. If nothing has won yet, this Remove
                // becomes the current winner, and a later Upsert for the same path
                // will still overwrite it via the unconditional branch above.
                winners.entry(key).or_insert(Resolution::Remove(entry));
            }
            DeltaAction::Ignore => {}
        }
    }

    // DBSYNC-102 review finding #3: make a suppressed genuine delete visible. A
    // second, cheap pass — rather than logging inline above — because a Remove
    // processed early in the loop only learns it lost once a LATER Upsert for the
    // same path overwrites it, and `winners` holds the final, settled resolution.
    let mut suppressed = 0u32;
    for entry in entries {
        if !matches!(delta_action_from_entry(entry), DeltaAction::Remove(_)) {
            continue;
        }
        let Some(key) = collapse_key(entry) else {
            continue;
        };
        if let Some(Resolution::Upsert(winner)) = winners.get(&key) {
            suppressed += 1;
            tracing::debug!(
                path = %key,
                winning_rev = %winner.rev.as_deref().unwrap_or(""),
                "remote delta collapse: suppressing a deleted entry in favour of a \
                 same-batch upsert for the same path"
            );
        }
    }
    if suppressed > 0 {
        tracing::info!(
            suppressed,
            "remote delta collapse: suppressed deleted entries in favour of a \
             same-batch upsert for the same path(s)"
        );
    }

    winners
        .into_values()
        .map(|resolution| match resolution {
            Resolution::Upsert(entry) | Resolution::Remove(entry) => entry,
        })
        .collect()
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

/// Applies one invocation's worth of accumulated delta entries to the local index —
/// the per-entry arm of `apply_remote_delta`, extracted so it can be driven by a test
/// with synthetic entries and never touch the network (DBSYNC-102 #172).
///
/// First runs `collapse_delta_entries` over the WHOLE accumulated batch (DBSYNC-102
/// #173/#174) so a path reported both `deleted` and re-added anywhere in the batch —
/// whether in the same page or split across two — resolves to exactly one action —
/// the file entry wins — before anything below ever sees it. `apply_remote_delta`
/// guarantees every page of one invocation has already been accumulated into `entries`
/// before this function is ever called; this function itself has no notion of pages.
///
/// Classifies each surviving entry, skips `.cloudsc` sidecars and anything
/// `covered_by_active_job` (the prefix-shaped membership test — see its own doc comment
/// for why a plain `.contains()` is wrong here: the sharpest DBSYNC-99 round-5
/// regression was exactly this `Remove` arm letting a delete for a descendant of a
/// pending folder move through unfiltered), then applies via the two shared
/// reconcilers and, on Windows only, materialises a fresh placeholder / prunes a stale
/// sidecar.
///
/// `covered_by_active_job` runs AFTER the collapse and is not replaced by it: the
/// collapse only resolves a delete/re-add contradiction for a single path within this
/// invocation's batch, it says nothing about paths an in-flight job already owns.
///
/// Takes the RAW `DropboxEntry` values, not `Vec<DeltaAction>`. `DeltaAction`
/// and `RemoteFileMeta` carry neither `size` nor `path_display` — the
/// Windows-only materialisation call below reads both off the raw entry
/// (DBSYNC-59's near-instant placeholder path), and losing them would silently
/// break that call with nothing on macOS to notice, since `#[cfg(windows)]`
/// hides the breakage from every macOS-run test. Classifying inside this
/// function, right where the raw entry is still in scope, keeps that data
/// available without threading a second, parallel `(entry, action)` structure
/// through the caller for a single Windows-only read. `collapse_delta_entries`
/// preserves this: it hands back raw entry references, not a parallel shape.
///
/// `delta_action_from_entry`'s `rel` — what is actually applied below — is
/// `path_display.trim_start_matches('/')`, deliberately **not** lowercased (unlike
/// the snapshot path's `remote_meta_from_entry`); preserved here unchanged. This is
/// distinct from `collapse_delta_entries`'s own map key, which DOES resolve on a
/// lowercased `path_lower` internally (see its doc) purely to decide which entries
/// contradict each other — that internal key never escapes the collapse, so it has
/// no bearing on the case-preserved `rel` this function applies.
///
/// Performs no HTTP call and never reads the access token: every field it needs
/// is already in `entries`. This is the only function in this module a test may
/// call directly to exercise delta application — see the keychain-danger list on
/// `apply_remote_delta` below for everything a test must still never call.
///
/// `pending_job_types` is **diagnostic only** (DBSYNC-102 #175): pairs of
/// (covering path, `job_type`) for the same active jobs `pending_targets` already
/// names, used ONLY to decide whether a drop on the upsert arm deserves the
/// stale-delete warning below — see `pending_delete_covering`'s doc for why. It
/// changes no decision `covered_by_active_job` makes; pass `&[]` wherever that
/// context is not available (e.g. a test that only cares about the Remove arm).
///
/// Returns the number of jobs enqueued.
pub(crate) fn apply_delta_entries(
    state: &AppState,
    entries: &[DropboxEntry],
    pending_targets: &HashSet<String>,
    pending_job_types: &[(String, String)],
) -> AppResult<usize> {
    let mut enqueued = 0usize;

    for entry in collapse_delta_entries(entries) {
        match delta_action_from_entry(entry) {
            DeltaAction::Upsert(rel, meta) => {
                if rel.ends_with(".cloudsc") {
                    // Sidecar bookkeeping path, never actionable here — unchanged.
                } else if crate::sync_pipeline::covered_by_active_job(&rel, pending_targets) {
                    // DBSYNC-102 #175, diagnostic only: `covered_by_active_job` has
                    // already made its decision — the upsert is dropped here exactly as
                    // before this slice, with no change to that outcome. This block only
                    // explains WHY, for the one case that matters: a `local_delete` for
                    // this exact path (or an ancestor folder) is STILL active, so new
                    // remote truth (this re-add) is being discarded while that deletion is
                    // still in flight. This is a narrower leftover case than "any re-add
                    // after a local_delete" — see `pending_delete_covering`'s doc for why
                    // the ordinary case (delete already drained before the re-add arrives)
                    // is NOT caught here and fires no warning at all. The fix for the
                    // conflation is tracked separately as DBSYNC-109; this is only the
                    // detector for the leftover slice.
                    if let Some(covering_target) =
                        crate::sync_pipeline::pending_delete_covering(&rel, pending_job_types)
                    {
                        tracing::warn!(
                            dropped_path = %rel,
                            covering_target = %covering_target,
                            covering_job_type = "local_delete",
                            "remote delta: new remote truth discarded — upsert dropped \
                             while a local_delete is still pending for this path \
                             (DBSYNC-109)"
                        );
                    }
                } else {
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
                    && !crate::sync_pipeline::covered_by_active_job(&rel, pending_targets)
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

    Ok(enqueued)
}

/// What one page-fetch attempt returned to `run_remote_delta`'s pagination loop
/// (DBSYNC-102 #172/#174 testability seam). The production closure built inside
/// `apply_remote_delta`, below, is the only thing that ever constructs a `Reset` from
/// a real HTTP response; a test's fake page source constructs either variant
/// directly, with no HTTP involved at all.
pub(crate) enum DeltaFetchOutcome {
    /// One page of `list_folder/continue`, already parsed.
    Page(DropboxListFolderResponse),
    /// The polled cursor was invalidated (HTTP 409 `reset`). Carries no pages: the
    /// caller already knows how many it accumulated before this point.
    Reset,
}

/// What one whole `run_remote_delta` invocation produced, reported back to the outer
/// shell rather than acted on inside `run_remote_delta` itself — seeding a fresh
/// snapshot after a reset is network I/O and stays in `apply_remote_delta` (DBSYNC-102
/// #172 review finding: a network-free function a test drives directly must never
/// itself decide to seed).
pub(crate) enum DeltaRunOutcome {
    /// Every page was fetched, the accumulated batch was collapsed and applied
    /// exactly once, and the resulting cursor was persisted exactly once.
    Applied { enqueued: usize },
    /// A page fetch reported a cursor reset. Nothing accumulated so far was applied
    /// or persisted — the caller must discard the cursor and reseed.
    Reset {
        pages_fetched: usize,
        entries_discarded: usize,
    },
}

/// The testable core of `apply_remote_delta` (DBSYNC-102 #172/#174 review finding).
/// Takes an INJECTED page source — "fetch the page that follows this cursor" — instead
/// of making the HTTP call itself, so a test can drive the REAL pagination,
/// accumulation, collapse and cursor-persist logic with a fake source that never
/// touches the network or the keychain, rather than re-performing that logic inline
/// against its own fixture. `apply_remote_delta` below is the thin production shell:
/// it owns the token fetch, the actual HTTP call, and — on a reported `Reset` — the
/// reseed, which is genuine network I/O and does not belong in a function a test
/// calls directly.
///
/// Owns, and owns EXACTLY ONCE per invocation (not once per page):
///   - accumulating every page's entries into one `Vec` before collapsing any of them
///     (DBSYNC-102 #174 — a `deleted` entry on page N and its re-add on page N+1 are
///     only resolved by `collapse_delta_entries` if both are in the same call; applying
///     page-by-page, the behaviour before this slice, let that split-page case through
///     uncontested — the shape that cost 456 MB for a folder big enough to paginate its
///     sharing-conversion delta)
///   - the `pending_job_targets` read and the `Db::active_jobs_with_type` read
///   - the single `apply_delta_entries` call, over the whole accumulated batch
///   - the single cursor persist, after applying — never before, never per page
///
/// ## On a page-fetch error
///
/// Anything `fetch_page` returns as `Err` (other than a `Reset`, which is a distinct,
/// non-error outcome) propagates immediately via `?`. Nothing accumulated so far is
/// applied, and the cursor is NOT persisted — the previously-persisted cursor is still
/// current, so the next invocation re-fetches and re-applies from there. Logs how many
/// pages were fetched and how many entries were discarded before giving up (DBSYNC-102
/// review finding #4), so an operator reading logs can tell a flaky-network retry from
/// a silent stall; `remote_longpoll.rs`'s loop backs off on this `Err` using its
/// existing backoff, so this log line is also the only place that records WHY.
///
/// ## On a reset
///
/// Returns `DeltaRunOutcome::Reset` with the same two counts, WITHOUT seeding a fresh
/// snapshot itself — that is `apply_remote_delta`'s job, since seeding is network I/O
/// this function must never perform.
///
/// ## Why the cursor is persisted once, not per page, and why that is still crash-safe
///
/// The per-page persist used to carry the comment "Advance + persist per page so a
/// crash mid-stream resumes cleanly." That comment is now false — accumulation means
/// nothing is applied, and nothing enters `remote_file_index`/`sync_jobs`, until every
/// page has been fetched — so it is replaced, not left behind, with the reason the new
/// placement is still crash-safe: **replay of the whole invocation is idempotent in the
/// senses below, though not without two residual windows that predate, and are not
/// fixed by, this slice.** If the app dies after fetching some pages but before this
/// function returns, the persisted cursor is still the PREVIOUS one, so the next
/// invocation re-fetches and re-applies the same pages from scratch. That is safe
/// because:
///   - `upsert_remote_file` (`storage/db.rs`) is a genuine upsert —
///     `INSERT ... ON CONFLICT(relative_path) DO UPDATE SET ...` — so replaying the
///     same remote-row writes converges to the same row, never a duplicate.
///   - `Db::enqueue_job`'s partial-unique-index `ON CONFLICT` on `(job_type,
///     target_path)` collapses a re-enqueue for a path that already has an ACTIVE job
///     (`queued`/`retry_wait`/`running`) of the same type into an `UPDATE`, not a
///     second row.
///
/// Two things that does NOT cover, stated here rather than left implicit so "replay is
/// idempotent" is not read as "every replay is a no-op":
///   - The `ON CONFLICT` guard's `WHERE` only matches those three ACTIVE statuses. If a
///     replayed `local_delete` has already reached `failed` by the time the replay
///     runs, the guard no longer matches it and the replay inserts a SECOND
///     `local_delete` row for the same path — a harmless re-attempt of a legitimate
///     delete (it never fabricates a delete that should not happen), but still a
///     duplicate row. "No job is duplicated" would overclaim past this case.
///   - `reconcile_remote_present` (unchanged by this ticket) writes the new remote row
///     BEFORE enqueuing its download, not atomically with it. A crash between those two
///     statements — a window that exists on every invocation, not something this
///     accumulation change introduces or widens — leaves the next invocation's
///     `should_download` check comparing against the already-advanced row, finding no
///     difference, and never re-enqueueing the download. This ticket does not close
///     that window; it is pre-existing and stays open.
///
/// A crash mid-stream therefore costs, at worst, the two residuals above on top of
/// re-fetching pages — not an unbounded correctness loss. Proven for the ordinary case
/// (no `failed` job, no crash between that write and that enqueue) by
/// `apply_delta_entries_replaying_the_same_batch_is_idempotent` below, which drives
/// `Db::enqueue_job` for real rather than asserting the conflict clause's text.
fn run_remote_delta(
    state: &AppState,
    starting_cursor: &str,
    mut fetch_page: impl FnMut(&str) -> AppResult<DeltaFetchOutcome>,
) -> AppResult<DeltaRunOutcome> {
    let mut cursor = starting_cursor.to_string();
    // Accumulated across every page of this invocation; collapsed and applied once,
    // below, after the fetch loop — never inside it. See the doc above for why.
    let mut all_entries: Vec<DropboxEntry> = Vec::new();
    let mut pages_fetched = 0usize;

    loop {
        match fetch_page(&cursor) {
            Ok(DeltaFetchOutcome::Reset) => {
                return Ok(DeltaRunOutcome::Reset {
                    pages_fetched,
                    entries_discarded: all_entries.len(),
                });
            }
            Ok(DeltaFetchOutcome::Page(resp)) => {
                pages_fetched += 1;
                let has_more = resp.has_more;
                cursor = resp.cursor;
                all_entries.extend(resp.entries);
                if !has_more {
                    break;
                }
            }
            Err(e) => {
                tracing::warn!(
                    pages_fetched,
                    entries_discarded = all_entries.len(),
                    error = %e,
                    "remote delta: page fetch failed; discarding this invocation's \
                     accumulated entries, cursor left unchanged"
                );
                return Err(e);
            }
        }
    }

    // Collapse and apply the WHOLE invocation's entries in one call — this is the
    // fix. One read of in-flight jobs is enough; see the doc above.
    let pending_targets = pending_job_targets(state)?;
    // DBSYNC-102 #175, diagnostic only: a second, sibling read of the same active-job
    // set, this time keeping each job's `job_type` alongside its path.
    // `covered_by_active_job` above keeps deciding over the plain `pending_targets`
    // set — this is never fed into it and changes no decision. Cost: one extra
    // indexed SQL query (`Db::active_jobs_with_type`, same WHERE clause and index as
    // `active_job_paths`), run once per invocation — i.e. once per longpoll cycle, NOT
    // once per upsert and NOT once per page — so its cost scales with how often the
    // delta fires, not with the size of the batch it is applying.
    //
    // `pending_job_targets`/`active_jobs_with_type` are read exactly once here, not
    // once per page: the per-page recomputation this replaces existed so that a job
    // enqueued while applying page 1 was visible when page 2 was reconciled — each
    // page used to be applied immediately after it was fetched. Now application
    // happens exactly once, after every page has been fetched and before any of them
    // has been applied, so there is no longer an "earlier page's application" for a
    // later page's read to observe: the whole batch sees one consistent snapshot of
    // in-flight jobs, taken right before that single application. A job enqueued by
    // something else entirely (another sync pass, a user action) in between is still
    // picked up next invocation.
    let pending_job_types = state.db.active_jobs_with_type()?;
    let enqueued = apply_delta_entries(state, &all_entries, &pending_targets, &pending_job_types)?;

    // Persist once, after applying, not per page. Safe because replay is idempotent
    // (see the doc above); what a crash costs is re-fetching pages, not correctness.
    state.db.set_app_config(REMOTE_DELTA_CURSOR_KEY, &cursor)?;

    Ok(DeltaRunOutcome::Applied { enqueued })
}

/// Apply the remote changes since the persisted cursor (DBSYNC-30): the thin
/// production shell around `run_remote_delta` (DBSYNC-102 #172 review finding). Owns
/// the token fetch, the actual `list_folder/continue` HTTP call, and the reseed after
/// a reported cursor reset; `run_remote_delta` owns everything else (pagination,
/// accumulation, collapse, apply, cursor persist) and is what a test drives directly
/// with a fake page source instead of this function. Returns jobs enqueued. The
/// caller drains the queue.
///
/// ## Memory cost of accumulating a whole delta in one `Vec`
///
/// `size_of::<DropboxEntry>()` (`models.rs`) is 160+ bytes measured on this build
/// (`std::mem::size_of`, x86_64/aarch64 pointer width; the `path_lower` field added by
/// the DBSYNC-102 case-drift fix adds one more `Option<String>` slot) — that is the
/// inline cost per `Vec` slot alone, before counting the heap bytes each present
/// `String` field allocates (`tag`, and for a `file` entry typically `path_display`,
/// `path_lower`, `content_hash`, `rev`, `id` too — a `deleted` entry carries only
/// `tag`+`path_display`+`path_lower`). A `file` entry's heap strings run roughly
/// another 150-350 bytes (a 64-hex-char `content_hash` alone is 64 bytes, plus the two
/// paths, `rev`, and `id`), so call it roughly 350-550 bytes resident per `file` entry
/// and 250-350 bytes per `deleted` entry, all-in. This repo does not set an explicit
/// `limit` on `list_folder/continue` (grep confirms no `"limit"` parameter on that
/// call), so the entries-per-page figure is Dropbox's own default and not something
/// this codebase pins down — the honest number to report is therefore the per-entry
/// rate above, not a total that assumes an unverified page size. Concretely: ten
/// thousand accumulated entries, already a delta far larger than the 13-file / 456 MB
/// conversion this ticket exists for, costs on the order of 3-6 MB resident. The
/// snapshot path (`fetch_all_remote_file_metadata`) already holds a whole account's
/// metadata in a `HashMap` for the same reason, so there is precedent for holding a
/// full remote listing in memory; a delta's accumulated `Vec` is smaller per-entry
/// than that `HashMap` (no map overhead, no key duplication) and, by definition, only
/// covers the changes since the last cursor, not the whole account.
///
/// **Never call this from a test.** It fetches the access token (`get_access_token`),
/// which reads the real OS keychain and, with a token in hand, makes a live
/// `list_folder/continue` request against the maintainer's real Dropbox account —
/// `build_state()` leaves `token_cache` empty, so there is no fixture seam here, only
/// the live fallback. Drive `run_remote_delta` directly instead, with a fake page
/// source closure (DBSYNC-102 #172/#174) — that is the network-free seam this
/// function's pagination/accumulation/collapse/persist logic now lives behind — or
/// drive `apply_delta_entries` directly for a single already-collapsed batch with no
/// pagination concerns at all.
pub(crate) fn apply_remote_delta(state: &AppState) -> AppResult<usize> {
    let starting_cursor = match state.db.get_app_config(REMOTE_DELTA_CURSOR_KEY)? {
        Some(c) if !c.is_empty() => c,
        // No cursor yet: seed a fresh snapshot; the next longpoll continues from it.
        _ => {
            seed_remote_delta_cursor(state)?;
            return Ok(0);
        }
    };

    let token = get_access_token(state)?;
    let client = &state.http_client;

    let outcome = run_remote_delta(state, &starting_cursor, |cursor| {
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
                return Ok(DeltaFetchOutcome::Reset);
            }
            return Err(AppError::Dropbox {
                status,
                message: format!("list_folder/continue delta: {body}"),
            });
        }

        let resp: DropboxListFolderResponse = response
            .json()
            .map_err(|e| AppError::Other(format!("list_folder/continue parse failed: {e}")))?;
        Ok(DeltaFetchOutcome::Page(resp))
    })?;

    match outcome {
        DeltaRunOutcome::Applied { enqueued } => {
            if enqueued > 0 {
                tracing::info!(enqueued, "longpoll delta enqueued download/delete jobs");
            }
            Ok(enqueued)
        }
        DeltaRunOutcome::Reset {
            pages_fetched,
            entries_discarded,
        } => {
            // Cursor invalidated: discard it and re-snapshot from scratch.
            // `run_remote_delta` already discarded whatever it accumulated before
            // reporting this, so there is nothing further to roll back here.
            tracing::info!(
                pages_fetched,
                entries_discarded,
                "remote delta cursor reset; re-snapshotting"
            );
            state.db.set_app_config(REMOTE_DELTA_CURSOR_KEY, "")?;
            seed_remote_delta_cursor(state)?;
            Ok(0)
        }
    }
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
            // Matches what Dropbox actually sends: path_lower is always the
            // lowercased path_display, same casing drift or not. A test that
            // needs to exercise mismatched/absent path_lower overrides the field
            // on the returned value directly.
            path_lower: path_display.map(|p| p.to_lowercase()),
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

    // ---------------------------------------------------------------------------
    // Delta application seam (DBSYNC-102 #172): `apply_delta_entries` is the
    // network-free extraction of `apply_remote_delta`'s per-entry loop. Every test
    // below drives it directly with synthetic `DropboxEntry` values — never
    // `apply_remote_delta` itself, which fetches the access token and would make a
    // live `list_folder/continue` call against the real account.
    // ---------------------------------------------------------------------------

    /// **DBSYNC-102 fix, proven at the seam.** Until slice #173 landed, this exact
    /// input batch produced the data-loss defect described below; the assertions now
    /// pin the FIXED outcome, not the defect — do not read this test as documenting a
    /// live bug.
    ///
    /// Reproduces the share-conversion shape end to end at the seam: a single
    /// `list_folder/continue` page reports a path as `deleted`, and immediately
    /// re-adds it in the SAME page with the SAME content hash the local index already
    /// has recorded for it — exactly what Dropbox sends when sharing converts an
    /// existing folder (nothing about the bytes changed, only the folder's sharing
    /// status). Before #173, `apply_delta_entries` walked `entries` strictly in order,
    /// one at a time: by the time the `deleted` entry was applied, the re-add sitting
    /// right next to it was not yet visible to `reconcile_remote_absent`, which only
    /// ever saw one path at a time and therefore only ever saw "local hash ==
    /// last-synced remote hash" — the condition for its safe-looking remote-wins
    /// delete arm. `collapse_delta_entries` now runs first and resolves the
    /// deleted+file contradiction to the file entry before either reconciler ever
    /// sees the path, so the delete is never actionable in the first place.
    ///
    /// Asserted here: zero `local_delete` jobs, and the index row for the path
    /// survives (both reconcilers would have removed it, directly or via the delete
    /// job's eventual execution).
    #[test]
    fn apply_delta_entries_a_reshared_file_survives_its_own_delete_in_the_same_batch_dbsync_102() {
        let state = build_state();
        // Last-synced state: Dropbox and the local index already agree on hash "H".
        state
            .db
            .upsert_remote_file("shared/hidratado.txt", "H", "rev1", 0, None)
            .unwrap();
        state
            .db
            .upsert_local_file("shared/hidratado.txt", "H", 3, 0)
            .unwrap();

        // One list_folder/continue page, shaped exactly as Dropbox sends it on a
        // sharing conversion: the existing entry tagged `deleted`, then the same path
        // re-added right after with unchanged content (same hash, a fresh rev).
        let mut deleted = file_entry(Some("/shared/hidratado.txt"), None, None, None);
        deleted.tag = "deleted".to_string();
        let readded = file_entry(Some("/shared/hidratado.txt"), Some("H"), Some("rev2"), None);
        let entries = vec![deleted, readded];

        let enqueued = apply_delta_entries(&state, &entries, &HashSet::new(), &[])
            .expect("apply_delta_entries must not touch the network");

        assert_eq!(
            enqueued, 0,
            "the collapse resolves deleted+file to the file entry: same hash means no \
             download is needed either, so nothing is enqueued at all"
        );
        assert!(
            job_targets(&state, "local_delete").is_empty(),
            "a path reported present again in the same batch must never reach \
             reconcile_remote_absent's delete arm"
        );
        // Asserting the row ADVANCED to the re-add's rev, not merely that it still
        // exists (DBSYNC-102 review finding #8): a mutation where the delete and the
        // re-add instead cancelled each other out — neither applied — would leave the
        // row at "rev1" and still pass an `is_some()`-only check.
        assert_eq!(
            state
                .db
                .get_remote_file("shared/hidratado.txt")
                .unwrap()
                .expect(
                    "the index row must survive a sharing conversion, not be \
                         dropped with the deleted entry the collapse has already ruled \
                         not actionable"
                )
                .rev,
            "rev2",
            "the row must advance to the re-add's rev — a delete/re-add pair that \
             merely cancelled out (neither applied) would leave it stuck at rev1"
        );
    }

    /// Mirror of the test above with the two entries swapped: the file entry arrives
    /// FIRST in the page, the `deleted` entry SECOND. The resolution must be identical
    /// either way — this is what "order-independent, not last-entry-wins" means in
    /// practice. If `collapse_delta_entries` ever regressed to a last-wins rule, this
    /// test (file-first) would start failing while the deleted-first test above kept
    /// passing, which is exactly the asymmetry a last-wins implementation produces.
    #[test]
    fn apply_delta_entries_resolution_is_the_same_with_the_file_entry_first() {
        let state = build_state();
        state
            .db
            .upsert_remote_file("shared/order.txt", "H", "rev1", 0, None)
            .unwrap();
        state
            .db
            .upsert_local_file("shared/order.txt", "H", 3, 0)
            .unwrap();

        let readded = file_entry(Some("/shared/order.txt"), Some("H"), Some("rev2"), None);
        let mut deleted = file_entry(Some("/shared/order.txt"), None, None, None);
        deleted.tag = "deleted".to_string();
        // File entry listed BEFORE the deleted entry this time.
        let entries = vec![readded, deleted];

        let enqueued = apply_delta_entries(&state, &entries, &HashSet::new(), &[])
            .expect("apply_delta_entries must not touch the network");

        assert_eq!(
            enqueued, 0,
            "file-first must resolve exactly like deleted-first"
        );
        assert!(job_targets(&state, "local_delete").is_empty());
        // Same strengthening as the deleted-first test above (DBSYNC-102 review
        // finding #8): the row must have advanced to the re-add's rev, not merely
        // still exist, or a cancel-out mutation would pass silently.
        assert_eq!(
            state
                .db
                .get_remote_file("shared/order.txt")
                .unwrap()
                .expect("row must survive")
                .rev,
            "rev2"
        );
    }

    /// A genuine remote edit racing a sharing conversion inside the same batch must
    /// still be downloaded. The upsert's hash ("H2") differs from the index's
    /// last-synced hash ("H") — if a fix collapsed deleted+file by just dropping BOTH
    /// entries whenever a `deleted` entry existed for the path (the "naive drop"
    /// warned about in `collapse_delta_entries`'s doc), this real content change would
    /// be silently lost instead of merely surviving a false deletion. The upsert must
    /// win and `reconcile_remote_present` must still see it and decide a download is
    /// owed, exactly as it would outside any sharing-conversion scenario.
    #[test]
    fn apply_delta_entries_upsert_wins_and_still_enqueues_a_download_for_a_genuine_edit() {
        let state = build_state();
        state
            .db
            .upsert_remote_file("shared/edited.txt", "H", "rev1", 0, None)
            .unwrap();
        state
            .db
            .upsert_local_file("shared/edited.txt", "H", 3, 0)
            .unwrap();

        let mut deleted = file_entry(Some("/shared/edited.txt"), None, None, None);
        deleted.tag = "deleted".to_string();
        // Different hash: a genuine edit, not merely a re-add of the same bytes.
        let edited = file_entry(Some("/shared/edited.txt"), Some("H2"), Some("rev2"), None);
        let entries = vec![deleted, edited];

        let enqueued = apply_delta_entries(&state, &entries, &HashSet::new(), &[])
            .expect("apply_delta_entries must not touch the network");

        assert_eq!(
            enqueued, 1,
            "the upsert must win over the deleted entry regardless of hash match, and \
             its differing hash means a download is genuinely owed"
        );
        assert_eq!(
            job_targets(&state, "download"),
            vec!["shared/edited.txt".to_string()],
            "a real content change must still be downloaded, not silently dropped \
             alongside the deleted entry it happens to share a path with"
        );
        assert!(
            job_targets(&state, "local_delete").is_empty(),
            "no delete must ever be queued for a path the batch also reports as upserted"
        );
        assert_eq!(
            state
                .db
                .get_remote_file("shared/edited.txt")
                .unwrap()
                .unwrap()
                .content_hash,
            "H2",
            "the index row must advance to the new hash, same as any ordinary remote edit"
        );
    }

    /// Case-drift regression (DBSYNC-102 #2 review finding). Dropbox paths are
    /// case-insensitive, and Dropbox's docs only promise correct casing on
    /// `path_display`'s LAST component — an ANCESTOR folder's casing can legitimately
    /// drift between a `deleted` entry and its own re-add. This batch deletes
    /// `/Shared/Doc.txt` and re-adds the SAME item as `/shared/Doc.txt` (the ancestor
    /// `Shared`/`shared` drifted; the leaf `Doc.txt` did not) — exactly the shape a
    /// sharing conversion can produce. Before keying the collapse on `path_lower`,
    /// these resolved to two DIFFERENT map keys (`shared/doc.txt`'s case-preserved
    /// `rel` differs from `Shared/Doc.txt`'s), so the `deleted` entry won uncontested
    /// and the only physical file on a case-insensitive filesystem (APFS) was deleted
    /// out from under the re-add. Asserted here: zero `local_delete` — the collapse
    /// must recognise both entries as the SAME path despite the ancestor casing drift.
    #[test]
    fn apply_delta_entries_does_not_delete_a_path_whose_ancestor_casing_drifted_between_entries() {
        let state = build_state();
        state
            .db
            .upsert_remote_file("Shared/Doc.txt", "H", "rev1", 0, None)
            .unwrap();
        state
            .db
            .upsert_local_file("Shared/Doc.txt", "H", 3, 0)
            .unwrap();

        let mut deleted = file_entry(Some("/Shared/Doc.txt"), None, None, None);
        deleted.tag = "deleted".to_string();
        // Same item, re-added with the ANCESTOR folder's casing drifted:
        // "Shared" -> "shared". The leaf component ("Doc.txt") is unchanged, matching
        // Dropbox's own guarantee that only the last component's casing is reliable.
        let readded = file_entry(Some("/shared/Doc.txt"), Some("H"), Some("rev2"), None);
        let entries = vec![deleted, readded];

        let enqueued = apply_delta_entries(&state, &entries, &HashSet::new(), &[])
            .expect("apply_delta_entries must not touch the network");

        assert_eq!(
            enqueued, 0,
            "same hash, same item: the collapse resolves deleted+file to the file entry \
             despite the ancestor casing drift, so nothing needs downloading either"
        );
        assert!(
            job_targets(&state, "local_delete").is_empty(),
            "a path whose ancestor casing merely drifted between the deleted entry and \
             its own re-add must never be deleted — path_lower must still recognise \
             them as the same item"
        );
        // Residual, documented on `collapse_delta_entries`: the winning entry applies
        // under its OWN case-preserved `rel` ("shared/Doc.txt"), which differs from the
        // existing index row's casing ("Shared/Doc.txt"). `reconcile_remote_present`
        // therefore finds no previous row under the new casing and records a SECOND,
        // differently-cased row — no data loss, but two rows for one Dropbox item. A
        // pre-existing limitation of a case-sensitive index observing a case-insensitive
        // remote, out of scope for this ticket.
        assert!(
            state
                .db
                .get_remote_file("shared/Doc.txt")
                .unwrap()
                .is_some(),
            "the re-add is recorded under its own casing, even though that differs from \
             the pre-existing row's — see the residual note above"
        );
    }

    /// DBSYNC-99 round-5 regression guard, re-proven after the collapse (#173). A
    /// removal for a path underneath a pending folder move must still be filtered by
    /// `covered_by_active_job` — the collapse runs BEFORE that filter and is not a
    /// replacement for it. There is no competing upsert for this path in the batch,
    /// so the collapse hands the removal straight through unchanged; the filter is
    /// what must stop it.
    #[test]
    fn apply_delta_entries_descendant_removal_still_filtered_after_collapse() {
        let state = build_state();
        state
            .db
            .upsert_remote_file("pending/folder/child.txt", "H", "rev1", 0, None)
            .unwrap();
        state
            .db
            .upsert_local_file("pending/folder/child.txt", "H", 3, 0)
            .unwrap();

        let mut deleted = file_entry(Some("/pending/folder/child.txt"), None, None, None);
        deleted.tag = "deleted".to_string();

        // The active job names the FOLDER being moved, not the child directly —
        // `covered_by_active_job` is what has to recognise the child as covered.
        let mut pending_targets = HashSet::new();
        pending_targets.insert("pending/folder".to_string());

        let enqueued = apply_delta_entries(&state, &[deleted], &pending_targets, &[])
            .expect("apply_delta_entries must not touch the network");

        assert_eq!(
            enqueued, 0,
            "a removal for a descendant of a pending folder move must stay filtered"
        );
        assert!(job_targets(&state, "local_delete").is_empty());
        assert!(
            state
                .db
                .get_remote_file("pending/folder/child.txt")
                .unwrap()
                .is_some(),
            "the row must survive while the covering job is still pending"
        );
    }

    /// Folder-tag regression. A `folder`-tagged entry must never enter the
    /// collapse's per-path resolution — `delta_action_from_entry` already maps it to
    /// `Ignore`, and `Ignore` is dropped before it ever reaches `winners`. This is
    /// safe only because `remote_file_index` holds no folder rows in production; the
    /// row seeded here for "team" is deliberately unrealistic (a real folder would
    /// never get one) so the test can actually fail if a future change ever lets a
    /// folder entry be treated as a Remove — with no row, `reconcile_remote_absent`
    /// would have returned `Ok(0)` either way and the test would prove nothing.
    #[test]
    fn apply_delta_entries_folder_tagged_entry_is_ignored_by_the_collapse() {
        let state = build_state();
        // Contrived: a remote row for a path that is, in this batch, folder-tagged.
        // Real folders never have one; seeded only so a wrongly-resolved Remove would
        // be visible as an enqueued local_delete instead of silently no-op'ing.
        state
            .db
            .upsert_remote_file("team", "H", "rev1", 0, None)
            .unwrap();
        state.db.upsert_local_file("team", "H", 3, 0).unwrap();

        let mut folder = file_entry(Some("/team"), None, None, None);
        folder.tag = "folder".to_string();

        let enqueued = apply_delta_entries(&state, &[folder], &HashSet::new(), &[])
            .expect("apply_delta_entries must not touch the network");

        assert_eq!(
            enqueued, 0,
            "a folder-tagged entry must never be resolved to a Remove"
        );
        assert!(job_targets(&state, "local_delete").is_empty());
        assert!(
            state.db.get_remote_file("team").unwrap().is_some(),
            "the contrived row must be left untouched, proving the folder entry never \
             reached reconcile_remote_absent"
        );
    }

    /// The flip side of the folder-tag test above, and an explicit acceptance
    /// criterion (DBSYNC-102 #173): Dropbox's `deleted` tag carries no indication of
    /// whether the removed path used to be a file or a folder — there is nothing in
    /// a `deleted` entry to distinguish the two. The collapse must still resolve it
    /// to a `Remove`, not mistake "this path might have been a folder" for "ignore
    /// this". (The removal is harmless in production for an actual folder path only
    /// because `remote_file_index` never holds a folder row to delete — see the
    /// comment on `collapse_delta_entries` — but that is a property of the
    /// reconciler, not something the collapse is allowed to assume by special-casing
    /// `deleted` entries itself.)
    #[test]
    fn apply_delta_entries_deleted_tag_resolves_to_removal_for_a_former_folder_path() {
        let state = build_state();
        state
            .db
            .upsert_remote_file("was_a_folder/leftover_row", "H", "rev1", 0, None)
            .unwrap();
        state
            .db
            .upsert_local_file("was_a_folder/leftover_row", "H", 3, 0)
            .unwrap();

        let mut deleted = file_entry(Some("/was_a_folder/leftover_row"), None, None, None);
        deleted.tag = "deleted".to_string();

        let enqueued = apply_delta_entries(&state, &[deleted], &HashSet::new(), &[])
            .expect("apply_delta_entries must not touch the network");

        assert_eq!(
            enqueued, 1,
            "a deleted entry resolves to a removal regardless of whether the path \
             used to be a file or a folder — there is no tag that says which"
        );
        assert_eq!(
            job_targets(&state, "local_delete"),
            vec!["was_a_folder/leftover_row".to_string()]
        );
    }

    /// Direct unit test of `collapse_delta_entries` itself — no `AppState`, no
    /// database, no `apply_delta_entries` in between — exercising the pure function
    /// the acceptance criteria name explicitly. Covers the full resolution table in
    /// one batch: deleted+file (upsert wins), file-only (upsert), deleted-only
    /// (remove), and a folder entry (dropped, appears in neither the input's winner
    /// set nor the output).
    #[test]
    fn collapse_delta_entries_resolves_the_full_table_in_one_batch() {
        let mut deleted_and_readded = file_entry(Some("/a.txt"), None, None, None);
        deleted_and_readded.tag = "deleted".to_string();
        let readd = file_entry(Some("/a.txt"), Some("H"), Some("rev2"), None);

        let file_only = file_entry(Some("/b.txt"), Some("H"), Some("rev1"), None);

        let mut delete_only = file_entry(Some("/c.txt"), None, None, None);
        delete_only.tag = "deleted".to_string();

        let mut folder_only = file_entry(Some("/d"), None, None, None);
        folder_only.tag = "folder".to_string();

        let entries = vec![
            deleted_and_readded,
            readd,
            file_only,
            delete_only,
            folder_only,
        ];

        let collapsed = collapse_delta_entries(&entries);

        // Exactly two survivors: the "a.txt" upsert (not the delete) and "b.txt"'s
        // upsert, plus "c.txt"'s removal. The folder entry at "d" never appears.
        let mut actions: Vec<(String, bool)> = collapsed
            .iter()
            .map(|e| {
                let rel = e.path_display.as_deref().unwrap().trim_start_matches('/');
                (rel.to_string(), e.tag == "file")
            })
            .collect();
        actions.sort();

        assert_eq!(
            actions,
            vec![
                ("a.txt".to_string(), true),
                ("b.txt".to_string(), true),
                ("c.txt".to_string(), false),
            ],
            "deleted+file resolves to the file entry for a.txt, file-only stays a \
             file entry for b.txt, deleted-only stays a deleted entry for c.txt, and \
             the folder entry at d never survives into the output at all"
        );
    }

    /// DBSYNC-102 baseline — the regression guard that must survive slice #173
    /// unchanged. A plain remote delete with NO re-add anywhere in the same batch must
    /// still produce exactly one `local_delete`. The count is asserted, not mere
    /// presence, because a real remote deletion that silently stops reaching disk is
    /// worse than the bug this ticket fixes: it is silent and it accumulates. A future
    /// batch collapse that is over-eager about "contradicting evidence" must not pass
    /// this suite by suppressing a genuine, uncontradicted deletion.
    #[test]
    fn apply_delta_entries_plain_remove_with_no_readd_deletes_exactly_once() {
        let state = build_state();
        state
            .db
            .upsert_remote_file("gone.txt", "H", "rev1", 0, None)
            .unwrap();
        state.db.upsert_local_file("gone.txt", "H", 3, 0).unwrap();

        let mut deleted = file_entry(Some("/gone.txt"), None, None, None);
        deleted.tag = "deleted".to_string();

        let enqueued = apply_delta_entries(&state, &[deleted], &HashSet::new(), &[])
            .expect("apply_delta_entries must not touch the network");

        assert_eq!(
            enqueued, 1,
            "a genuine, uncontradicted remote delete must still \
             produce exactly one local_delete job"
        );
        assert_eq!(
            job_targets(&state, "local_delete"),
            vec!["gone.txt".to_string()]
        );
    }

    /// DBSYNC-102 review finding #3: `[file X, deleted X]` — a genuine edit
    /// immediately followed by a genuine delete of the SAME path within one batch —
    /// is exactly the shape where the set predicate's "any upsert beats every remove"
    /// rule suppresses a real deletion with no trace. The set predicate stays (see
    /// `collapse_delta_entries`'s doc for why), but the suppression must now be
    /// logged. Pins both halves: no delete is enqueued, AND the suppression is
    /// actually logged — a silent suppression with nothing in the logs would pass
    /// every other test in this file while leaving an operator with no way to tell a
    /// missed delete happened at all.
    #[test]
    fn apply_delta_entries_logs_a_suppressed_delete_when_an_upsert_for_the_same_path_wins() {
        let state = build_state();
        state
            .db
            .upsert_remote_file("shared/edited_then_deleted.txt", "H", "rev1", 0, None)
            .unwrap();
        state
            .db
            .upsert_local_file("shared/edited_then_deleted.txt", "H", 3, 0)
            .unwrap();

        let edited = file_entry(
            Some("/shared/edited_then_deleted.txt"),
            Some("H2"),
            Some("rev2"),
            None,
        );
        let mut deleted = file_entry(Some("/shared/edited_then_deleted.txt"), None, None, None);
        deleted.tag = "deleted".to_string();
        let entries = vec![edited, deleted];

        let log = captured_tracing_output(|| {
            let enqueued = apply_delta_entries(&state, &entries, &HashSet::new(), &[])
                .expect("apply_delta_entries must not touch the network");
            assert_eq!(
                enqueued, 1,
                "the upsert wins and its differing hash means a download is owed; the \
                 real delete that followed it in the batch is suppressed, not applied"
            );
        });

        assert!(
            job_targets(&state, "local_delete").is_empty(),
            "the set predicate suppresses the genuine delete in favour of the upsert \
             that beat it in this batch"
        );
        assert!(
            log.contains("DEBUG") && log.contains("suppressing a deleted entry"),
            "the suppressed delete must be logged at debug, naming it as suppressed; \
             got: {log}"
        );
        assert!(
            log.contains("shared/edited_then_deleted.txt"),
            "the debug log must name the suppressed path; got: {log}"
        );
        assert!(
            log.contains("rev2"),
            "the debug log must name the winning entry's rev; got: {log}"
        );
        assert!(
            log.contains("INFO") && log.contains("suppressed deleted entries"),
            "one info-level summary must also fire for this batch; got: {log}"
        );
    }

    // ---------------------------------------------------------------------------
    // `run_remote_delta` (DBSYNC-102 #172/#174 review finding #1): the REAL
    // pagination/accumulation/collapse/cursor-persist loop, driven through a fake
    // page source closure. Unlike the fixture-only test this section replaces (which
    // built two `Vec`s, `extend`ed them, and called `apply_delta_entries` once —
    // re-performing the accumulation inline and testing its own fixture, not
    // `apply_remote_delta`'s actual loop), every test below calls `run_remote_delta`
    // itself. A fake page source never touches the network or the keychain: it is a
    // plain closure over a pre-scripted `Vec` of outcomes, never `get_access_token`
    // or an HTTP client.
    // ---------------------------------------------------------------------------

    /// Builds a fake page source from a pre-scripted sequence of outcomes, one per
    /// call. Panics if `run_remote_delta` ever calls it more times than the test
    /// scripted — a wrong call count is itself a defect worth failing loudly on,
    /// not silently looping or returning a default.
    fn scripted_page_source(
        outcomes: Vec<AppResult<DeltaFetchOutcome>>,
    ) -> impl FnMut(&str) -> AppResult<DeltaFetchOutcome> {
        let mut outcomes = outcomes.into_iter();
        move |_cursor| {
            outcomes
                .next()
                .expect("fake page source called more times than this test scripted")
        }
    }

    /// **DBSYNC-102 #174, the defect this slice exists for, proven at the real loop.**
    /// `collapse_delta_entries` only resolves a `deleted` + re-add contradiction for
    /// paths it sees TOGETHER, in the same call. Before this slice, `apply_remote_delta`
    /// called `apply_delta_entries` once PER PAGE, so a `deleted` entry on page N and
    /// its re-add on page N+1 were never in the same call and the collapse never saw
    /// them together — exactly the shape that cost 456 MB for a share conversion big
    /// enough to paginate.
    ///
    /// This also proves the cursor persist contract in the same test: the ONE persist
    /// at the end lands on page 2's cursor, the LAST page's, never page 1's.
    ///
    /// Proof this test can fail: reverting `run_remote_delta` to apply-per-page (call
    /// `apply_delta_entries` once per `DeltaFetchOutcome::Page`, inside the loop,
    /// instead of accumulating into `all_entries` and applying once after the loop)
    /// turns this back into the data-loss case — page 1's lone `deleted` entry has no
    /// sibling to contradict it and enqueues the delete; page 2's lone re-add cannot
    /// retract an already-enqueued job. Observed directly while writing this test (see
    /// this slice's report); restored to the accumulate-then-apply-once form below
    /// before landing, confirmed byte-identical by hash.
    #[test]
    fn run_remote_delta_resolves_a_delete_and_readd_split_across_two_pages() {
        let state = build_state();
        state
            .db
            .upsert_remote_file("shared/paginated.txt", "H", "rev1", 0, None)
            .unwrap();
        state
            .db
            .upsert_local_file("shared/paginated.txt", "H", 3, 0)
            .unwrap();

        // "Page 1": the deleted entry only, has_more=true.
        let mut deleted = file_entry(Some("/shared/paginated.txt"), None, None, None);
        deleted.tag = "deleted".to_string();
        let page_one = DropboxListFolderResponse {
            entries: vec![deleted],
            cursor: "cursor_after_page_1".to_string(),
            has_more: true,
        };
        // "Page 2": the re-add, same content hash, has_more=false.
        let readded = file_entry(Some("/shared/paginated.txt"), Some("H"), Some("rev2"), None);
        let page_two = DropboxListFolderResponse {
            entries: vec![readded],
            cursor: "cursor_after_page_2".to_string(),
            has_more: false,
        };

        let outcome = run_remote_delta(
            &state,
            "starting_cursor",
            scripted_page_source(vec![
                Ok(DeltaFetchOutcome::Page(page_one)),
                Ok(DeltaFetchOutcome::Page(page_two)),
            ]),
        )
        .expect("run_remote_delta must not error on an ordinary two-page fetch");

        match outcome {
            DeltaRunOutcome::Applied { enqueued } => assert_eq!(
                enqueued, 0,
                "accumulated across both pages, the collapse resolves deleted+file to \
                 the file entry before either reconciler runs"
            ),
            DeltaRunOutcome::Reset { .. } => panic!("expected Applied, got Reset"),
        }
        assert!(
            job_targets(&state, "local_delete").is_empty(),
            "a page-split deleted+re-add must never produce a local_delete once both \
             pages are accumulated into the same collapse-and-apply call"
        );
        assert_eq!(
            state
                .db
                .get_remote_file("shared/paginated.txt")
                .unwrap()
                .expect("the index row must survive a share conversion that paginates")
                .rev,
            "rev2",
            "the row must advance to the re-add's rev, not merely still exist"
        );
        assert_eq!(
            state.db.get_app_config(REMOTE_DELTA_CURSOR_KEY).unwrap(),
            Some("cursor_after_page_2".to_string()),
            "the cursor must be persisted exactly once, to the LAST page's cursor"
        );
    }

    /// DBSYNC-102 #1 review finding: a page-fetch error on a LATER page must discard
    /// everything accumulated so far and leave the previously-persisted cursor
    /// untouched — the next invocation re-fetches from where it was, not from
    /// mid-batch. Page 1's genuine delete must never reach `sync_jobs` just because a
    /// later page in the SAME invocation failed to fetch.
    #[test]
    fn run_remote_delta_page_fetch_error_on_a_later_page_discards_everything_and_leaves_the_cursor_untouched(
    ) {
        let state = build_state();
        state
            .db
            .set_app_config(REMOTE_DELTA_CURSOR_KEY, "old_cursor")
            .unwrap();
        state
            .db
            .upsert_remote_file("gone.txt", "H", "rev1", 0, None)
            .unwrap();
        state.db.upsert_local_file("gone.txt", "H", 3, 0).unwrap();

        let mut deleted = file_entry(Some("/gone.txt"), None, None, None);
        deleted.tag = "deleted".to_string();
        let page_one = DropboxListFolderResponse {
            entries: vec![deleted],
            cursor: "cursor_after_page_1".to_string(),
            has_more: true,
        };

        let result = run_remote_delta(
            &state,
            "old_cursor",
            scripted_page_source(vec![
                Ok(DeltaFetchOutcome::Page(page_one)),
                Err(AppError::Network("simulated page 2 failure".to_string())),
            ]),
        );

        assert!(
            result.is_err(),
            "a page-fetch error must propagate, not be swallowed"
        );
        assert!(
            job_targets(&state, "local_delete").is_empty(),
            "page 1's delete must never be applied when a later page in the same \
             invocation fails to fetch"
        );
        assert_eq!(
            state.db.get_app_config(REMOTE_DELTA_CURSOR_KEY).unwrap(),
            Some("old_cursor".to_string()),
            "the cursor must stay exactly where it was before this invocation — never \
             advanced to page 1's cursor, which this invocation never got to persist"
        );
    }

    /// DBSYNC-102 #1 review finding: a reset on a LATER page must discard every
    /// earlier page's entries (nothing from page 1 is applied) and report the reset to
    /// the caller, rather than seeding a fresh snapshot itself — seeding is network
    /// I/O and belongs only in `apply_remote_delta`'s shell. This function's own
    /// contract on a reset is simply: apply nothing, persist nothing, say how much was
    /// thrown away.
    #[test]
    fn run_remote_delta_reset_on_a_later_page_discards_earlier_pages_and_reports_reset() {
        let state = build_state();
        state
            .db
            .set_app_config(REMOTE_DELTA_CURSOR_KEY, "old_cursor")
            .unwrap();
        state
            .db
            .upsert_remote_file("shared/resetting.txt", "H", "rev1", 0, None)
            .unwrap();
        state
            .db
            .upsert_local_file("shared/resetting.txt", "H", 3, 0)
            .unwrap();

        let mut deleted = file_entry(Some("/shared/resetting.txt"), None, None, None);
        deleted.tag = "deleted".to_string();
        let page_one = DropboxListFolderResponse {
            entries: vec![deleted],
            cursor: "cursor_after_page_1".to_string(),
            has_more: true,
        };

        let outcome = run_remote_delta(
            &state,
            "old_cursor",
            scripted_page_source(vec![
                Ok(DeltaFetchOutcome::Page(page_one)),
                Ok(DeltaFetchOutcome::Reset),
            ]),
        )
        .expect("a Reset outcome is Ok, not Err — it is a distinct, expected outcome");

        match outcome {
            DeltaRunOutcome::Reset {
                pages_fetched,
                entries_discarded,
            } => {
                assert_eq!(
                    pages_fetched, 1,
                    "exactly the one page fetched before reset"
                );
                assert_eq!(
                    entries_discarded, 1,
                    "page 1's single deleted entry, discarded"
                );
            }
            DeltaRunOutcome::Applied { .. } => panic!("expected Reset, got Applied"),
        }
        assert!(
            job_targets(&state, "local_delete").is_empty(),
            "page 1's delete must never be applied when the invocation resets before \
             completing"
        );
        assert_eq!(
            state.db.get_app_config(REMOTE_DELTA_CURSOR_KEY).unwrap(),
            Some("old_cursor".to_string()),
            "run_remote_delta never advances or clears the cursor itself on a reset — \
             it only reports the reset upward. Clearing it to \"\" and reseeding is \
             apply_remote_delta's job (network code a test cannot drive directly); this \
             layer's contract is only that nothing it accumulated gets persisted"
        );
    }

    /// DBSYNC-102 #1 review finding: the diagnostic warning must be fed the REAL
    /// `Db::active_jobs_with_type` result, not a placeholder — a test that fails if
    /// `run_remote_delta` were ever changed to pass `&[]` through instead of actually
    /// querying. Seeds a genuine pending `local_delete` for the exact path a re-add
    /// arrives for, so the DBSYNC-109 warning can only fire if the real query ran and
    /// its result reached `apply_delta_entries` unmodified.
    #[test]
    fn run_remote_delta_feeds_the_real_active_jobs_with_type_into_the_stale_delete_warning() {
        let state = build_state();
        state
            .db
            .upsert_remote_file("shared/stale.txt", "H", "rev1", 0, None)
            .unwrap();
        state
            .db
            .upsert_local_file("shared/stale.txt", "H", 3, 0)
            .unwrap();
        state
            .db
            .enqueue_job(
                "local_delete",
                Some("shared/stale.txt"),
                Some("shared/stale.txt"),
            )
            .unwrap();

        let readded = file_entry(Some("/shared/stale.txt"), Some("H2"), Some("rev2"), None);
        let page = DropboxListFolderResponse {
            entries: vec![readded],
            cursor: "cursor_after_page_1".to_string(),
            has_more: false,
        };

        let log = captured_tracing_output(|| {
            let outcome = run_remote_delta(
                &state,
                "starting_cursor",
                scripted_page_source(vec![Ok(DeltaFetchOutcome::Page(page))]),
            )
            .expect("run_remote_delta must not error");
            match outcome {
                DeltaRunOutcome::Applied { enqueued } => assert_eq!(
                    enqueued, 0,
                    "covered_by_active_job still drops the upsert, unchanged"
                ),
                DeltaRunOutcome::Reset { .. } => panic!("expected Applied, got Reset"),
            }
        });

        assert!(
            log.contains("WARN") && log.contains("discarded"),
            "the DBSYNC-109 warning can only fire if run_remote_delta actually read \
             Db::active_jobs_with_type and passed the REAL result through to \
             apply_delta_entries rather than an empty placeholder; got: {log}"
        );
        assert!(
            log.contains("shared/stale.txt"),
            "the warning must name the dropped path; got: {log}"
        );
    }

    /// **DBSYNC-102 #174, idempotent replay.** Justifies moving the cursor persist
    /// from per-page to once-at-the-end: if the app crashes after fetching pages but
    /// before this invocation returns, the cursor is still the PREVIOUS one, so the
    /// next invocation re-fetches and re-applies the very same accumulated batch.
    /// This drives `apply_delta_entries` — and through it, `Db::enqueue_job` and
    /// `Db::upsert_remote_file` for REAL, against a real on-disk `Db`, never a mock of
    /// either — twice with the identical batch and asserts no duplicate jobs and no
    /// duplicate/divergent index state, which is the actual claim being relied on,
    /// not merely that the second call does not error.
    ///
    /// The batch mixes a genuine delete (no re-add: `gone.txt`) with a genuine content
    /// change (`edited.txt`, hash "H" -> "H2") so the test exercises both of
    /// `apply_delta_entries`'s arms, not just one.
    #[test]
    fn apply_delta_entries_replaying_the_same_batch_is_idempotent() {
        let state = build_state();
        state
            .db
            .upsert_remote_file("gone.txt", "H", "rev1", 0, None)
            .unwrap();
        state.db.upsert_local_file("gone.txt", "H", 3, 0).unwrap();
        state
            .db
            .upsert_remote_file("edited.txt", "H", "rev1", 0, None)
            .unwrap();
        state.db.upsert_local_file("edited.txt", "H", 3, 0).unwrap();

        let mut deleted = file_entry(Some("/gone.txt"), None, None, None);
        deleted.tag = "deleted".to_string();
        let edited = file_entry(Some("/edited.txt"), Some("H2"), Some("rev2"), None);
        let entries = vec![deleted, edited];

        let first = apply_delta_entries(&state, &entries, &HashSet::new(), &[])
            .expect("first apply must not touch the network");
        assert_eq!(
            first, 2,
            "first application: one local_delete for gone.txt, one download for \
             edited.txt's genuine content change"
        );
        assert_eq!(
            job_targets(&state, "local_delete"),
            vec!["gone.txt".to_string()]
        );
        assert_eq!(
            job_targets(&state, "download"),
            vec!["edited.txt".to_string()]
        );
        assert_eq!(
            state
                .db
                .get_remote_file("edited.txt")
                .unwrap()
                .unwrap()
                .content_hash,
            "H2"
        );

        // Simulate a crash before the cursor was persisted: the SAME accumulated
        // batch is replayed from scratch, exactly as `apply_remote_delta` would redo
        // it on the next invocation since the cursor still points before this batch.
        let second = apply_delta_entries(&state, &entries, &HashSet::new(), &[])
            .expect("replay must not touch the network");

        // `Db::enqueue_job`'s partial-unique-index `ON CONFLICT(job_type, target_path)
        // WHERE status IN ('queued','retry_wait','running')` means re-enqueuing
        // local_delete for gone.txt collapses into an UPDATE of the existing row, not
        // a second one (the ACTIVE case this test exercises — see `apply_remote_delta`'s
        // doc for the `failed`-status residual this guard does NOT cover) — so even
        // though `reconcile_remote_absent` unconditionally
        // reports `Ok(1)` on this arm every time it is taken, no second row exists.
        // `edited.txt`'s download does not even re-fire: `upsert_remote_file` already
        // advanced the remote row to "H2" on the first pass, so the replay's
        // `should_download` check (`prev.content_hash != remote_meta.content_hash`)
        // is now false — the upsert itself is idempotent before enqueue is reached.
        assert_eq!(
            second, 1,
            "replay re-takes the local_delete arm (enqueue_job's own idempotence is \
             what prevents a duplicate row, not a short-circuit here), but the \
             download arm is now a no-op because the remote row already reflects H2"
        );

        let local_deletes = job_targets(&state, "local_delete");
        assert_eq!(
            local_deletes,
            vec!["gone.txt".to_string()],
            "exactly one local_delete row for gone.txt after replay, not two — proves \
             enqueue_job's ON CONFLICT collapse, not merely that replay did not error"
        );
        let downloads = job_targets(&state, "download");
        assert_eq!(
            downloads,
            vec!["edited.txt".to_string()],
            "exactly one download row for edited.txt, not two"
        );
        assert_eq!(
            state
                .db
                .get_remote_file("edited.txt")
                .unwrap()
                .unwrap()
                .content_hash,
            "H2",
            "replay converges to the same final index state, not a diverged one"
        );
    }

    // ---------------------------------------------------------------------------
    // DBSYNC-102 #175 — the stale-delete-filter diagnostic, wired at the real site
    // inside `apply_delta_entries`. `pending_delete_covering_*` in `sync_pipeline.rs`
    // already proves the pure predicate's four cases in isolation; the tests below
    // additionally prove the WIRING — that `apply_delta_entries` actually calls it at
    // the right moment and actually emits the warning with the right fields — by
    // capturing `tracing`'s real formatted output, not by inspecting internal state.
    // ---------------------------------------------------------------------------

    /// A `Write` implementation over a shared, clonable buffer, so a `tracing`
    /// subscriber installed for the duration of one test's closure can be read back
    /// afterwards. `tracing_subscriber::fmt`'s blanket `MakeWriter` impl for
    /// `Fn() -> W where W: Write` means a closure cloning this struct is enough; no
    /// new dependency, `tracing-subscriber` is already in `[dependencies]`.
    #[derive(Clone)]
    struct SharedBufWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for SharedBufWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Runs `f` with a `tracing` subscriber installed for the CURRENT THREAD only
    /// (`tracing::subscriber::with_default` is thread-local, so this is safe under
    /// the test runner's default parallelism) and returns everything it formatted,
    /// as plain text.
    fn captured_tracing_output(f: impl FnOnce()) -> String {
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let buf_for_writer = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || SharedBufWriter(buf_for_writer.clone()))
            .with_ansi(false)
            .without_time()
            // DBSYNC-102 review finding #3's test needs `debug!` captured too, not
            // only `warn!`/`info!`. The `fmt()` builder's maximum level defaults to
            // INFO, so without this a `debug!` event is filtered out — verified by
            // running that test alone with `--test-threads=1`, where it still fails.
            // `with_default` below scopes the subscriber to this thread, so raising
            // the level here does not leak into tests running in parallel.
            .with_max_level(tracing::Level::TRACE)
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        let bytes = buf.lock().unwrap().clone();
        String::from_utf8(bytes).expect("tracing output must be valid utf-8")
    }

    /// The defect this slice detects, driven end to end: an upsert for
    /// `shared/stale.txt` arrives while a `local_delete` for that exact path is
    /// still pending (left over, in the real residual, from an EARLIER
    /// `apply_remote_delta` invocation). `covered_by_active_job` drops it — same as
    /// it always has, proven by the zero-enqueued/no-op assertions below — and this
    /// slice's only addition is that the drop now says so.
    #[test]
    fn apply_delta_entries_warns_when_an_upsert_is_dropped_by_a_pending_local_delete() {
        let state = build_state();
        state
            .db
            .upsert_remote_file("shared/stale.txt", "H", "rev1", 0, None)
            .unwrap();
        state
            .db
            .upsert_local_file("shared/stale.txt", "H", 3, 0)
            .unwrap();

        let readded = file_entry(Some("/shared/stale.txt"), Some("H2"), Some("rev2"), None);

        let mut pending_targets = HashSet::new();
        pending_targets.insert("shared/stale.txt".to_string());
        let pending_job_types = vec![("shared/stale.txt".to_string(), "local_delete".to_string())];

        let log = captured_tracing_output(|| {
            let enqueued =
                apply_delta_entries(&state, &[readded], &pending_targets, &pending_job_types)
                    .expect("apply_delta_entries must not touch the network");
            assert_eq!(
                enqueued, 0,
                "zero behaviour change: covered_by_active_job still drops the upsert, \
                 exactly as it did before this slice"
            );
        });

        assert!(
            job_targets(&state, "download").is_empty(),
            "no behaviour change: nothing is enqueued for the dropped path"
        );
        assert_eq!(
            state
                .db
                .get_remote_file("shared/stale.txt")
                .unwrap()
                .unwrap()
                .content_hash,
            "H",
            "no behaviour change: the remote row is not advanced by a dropped upsert"
        );
        assert!(
            log.contains("WARN"),
            "the drop must be logged at warning level; got: {log}"
        );
        assert!(
            log.contains("shared/stale.txt"),
            "the warning must name the dropped path; got: {log}"
        );
        assert!(
            log.contains("local_delete"),
            "the warning must name the covering job's type; got: {log}"
        );
        assert!(
            log.contains("discarded"),
            "the warning must say plainly that new remote truth was discarded; got: {log}"
        );
    }

    /// The flip side, and the acceptance criterion that there is "no new noise on the
    /// normal double-enqueue path": the exact same drop, but the covering job is a
    /// `download` instead of a `local_delete`. `covered_by_active_job` drops the
    /// upsert exactly the same way — this is simply two jobs racing to reconcile the
    /// same path — and that is NOT the DBSYNC-102/109 residual, so no warning is owed.
    #[test]
    fn apply_delta_entries_does_not_warn_when_the_covering_job_is_a_download() {
        let state = build_state();
        state
            .db
            .upsert_remote_file("shared/busy.txt", "H", "rev1", 0, None)
            .unwrap();
        state
            .db
            .upsert_local_file("shared/busy.txt", "H", 3, 0)
            .unwrap();

        let readded = file_entry(Some("/shared/busy.txt"), Some("H2"), Some("rev2"), None);

        let mut pending_targets = HashSet::new();
        pending_targets.insert("shared/busy.txt".to_string());
        let pending_job_types = vec![("shared/busy.txt".to_string(), "download".to_string())];

        let log = captured_tracing_output(|| {
            let enqueued =
                apply_delta_entries(&state, &[readded], &pending_targets, &pending_job_types)
                    .expect("apply_delta_entries must not touch the network");
            assert_eq!(
                enqueued, 0,
                "the drop itself is identical regardless of the covering job's type"
            );
        });

        assert!(
            !log.contains("WARN"),
            "a covering download must never produce this warning — no new noise on \
             the ordinary double-enqueue path; got: {log}"
        );
    }

    /// Prefix-shaped cover, proven at the wiring level (the pure version lives in
    /// `sync_pipeline::tests::pending_delete_covering_warns_when_the_cover_is_an_ancestor_folder_delete`):
    /// the pending `local_delete` names the PARENT FOLDER, not the dropped file's own
    /// path — exactly the shape `covered_by_active_job` itself is prefix-shaped for
    /// (DBSYNC-99 round 5). The warning must still fire and must name the folder as
    /// the covering target, not the dropped file's own path.
    #[test]
    fn apply_delta_entries_warns_when_the_cover_is_an_ancestor_folder_local_delete() {
        let state = build_state();
        state
            .db
            .upsert_remote_file("shared/folder/child.txt", "H", "rev1", 0, None)
            .unwrap();
        state
            .db
            .upsert_local_file("shared/folder/child.txt", "H", 3, 0)
            .unwrap();

        let readded = file_entry(
            Some("/shared/folder/child.txt"),
            Some("H2"),
            Some("rev2"),
            None,
        );

        // The active job names the FOLDER, not the child file directly.
        let mut pending_targets = HashSet::new();
        pending_targets.insert("shared/folder".to_string());
        let pending_job_types = vec![("shared/folder".to_string(), "local_delete".to_string())];

        let log = captured_tracing_output(|| {
            let enqueued =
                apply_delta_entries(&state, &[readded], &pending_targets, &pending_job_types)
                    .expect("apply_delta_entries must not touch the network");
            assert_eq!(
                enqueued, 0,
                "the descendant upsert is still dropped, unchanged"
            );
        });

        assert!(
            log.contains("WARN") && log.contains("discarded"),
            "an ancestor folder's pending local_delete must still trigger the warning; \
             got: {log}"
        );
        assert!(
            log.contains("shared/folder/child.txt"),
            "the warning must name the dropped descendant path; got: {log}"
        );
        assert!(
            log.contains("covering_target=shared/folder ")
                || log.contains("covering_target=shared/folder\n"),
            "the warning must name the covering job's target as the FOLDER, not the \
             descendant file's own path; got: {log}"
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

    /// DBSYNC-102 #176 — pins the documented contract at this module's doc on
    /// `reconcile_remote_snapshot_with_breaker` (the delta path is "always
    /// authoritative and never gated"): the DBSYNC-64 mass-delete breaker
    /// (`is_mass_deletion`/`block_mass_deletion`) is invoked ONLY from that
    /// snapshot-path function, never from `apply_delta_entries`/`apply_remote_delta`.
    /// A delta batch of plain removals large enough to trip the breaker on the
    /// snapshot path (`MASS_DELETE_ABSOLUTE == 200`, see `sync_pipeline.rs`) must
    /// still apply every one of them, ungated, on the delta path.
    ///
    /// Proof this test can fail: temporarily gating the delta path's `Remove` arm
    /// behind `is_mass_deletion`/`block_mass_deletion` the way the snapshot path is
    /// gated turns `enqueued` into `0` and leaves all 200 `local_delete` jobs
    /// missing — observed directly while writing this test, then reverted; the
    /// delta path's ungated status is a deliberate contract (DBSYNC-64 scope
    /// decision), not an oversight this test should "fix".
    #[test]
    fn apply_delta_entries_large_delta_batch_of_removals_is_ungated_by_the_mass_delete_breaker() {
        let state = build_state();
        const BATCH: usize = 200; // == MASS_DELETE_ABSOLUTE; blocked outright on the snapshot path.

        let mut entries = Vec::with_capacity(BATCH);
        for i in 0..BATCH {
            let rel = format!("shared/{i}.txt");
            state
                .db
                .upsert_remote_file(&rel, "H", "rev", 0, None)
                .unwrap();
            state.db.upsert_local_file(&rel, "H", 3, 0).unwrap();

            let mut deleted = file_entry(Some(&format!("/{rel}")), None, None, None);
            deleted.tag = "deleted".to_string();
            entries.push(deleted);
        }

        let enqueued = apply_delta_entries(&state, &entries, &HashSet::new(), &[])
            .expect("apply_delta_entries must not touch the network");

        assert_eq!(
            enqueued, BATCH,
            "a delta batch this large must still apply every removal — the delta \
             path has no breaker call to block it"
        );
        assert_eq!(job_targets(&state, "local_delete").len(), BATCH);
        assert!(
            state
                .db
                .get_app_config("mass_delete_blocked_remote")
                .unwrap()
                .unwrap_or_default()
                .is_empty(),
            "the delta path must never set the remote-sweep breaker's pause flag"
        );
        assert!(
            state
                .db
                .get_app_config("mass_delete_blocked_scan")
                .unwrap()
                .unwrap_or_default()
                .is_empty(),
            "the delta path must never set the local-scan breaker's pause flag either"
        );
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
