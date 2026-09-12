use serde::{Deserialize, Serialize};

use crate::storage::db::{ConflictRow, SyncJobRow};
use crate::sync::engine::SyncStatus;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OauthStartResponse {
    pub auth_url: String,
    pub state: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SyncDashboard {
    pub status: SyncStatus,
    pub jobs: Vec<SyncJobRow>,
    pub conflicts: Vec<ConflictRow>,
    /// DBSYNC-64: true while the mass-deletion circuit breaker has paused sync
    /// (either direction's durable `mass_delete_blocked_*` flag is set), so the
    /// frontend can show a "review & confirm deletions" button that calls
    /// `confirm_pending_deletions`. See `sync_pipeline::mass_delete_pause_active`.
    pub mass_delete_paused: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SyncTickResult {
    pub scanned_files: usize,
    pub enqueued_jobs: usize,
    pub processed_job: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TriggerSyncResponse {
    pub accepted: bool,
    pub reason: TriggerSyncReason,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TriggerSyncReason {
    Started,
    AlreadyRunning,
}

#[derive(Deserialize)]
pub(crate) struct DropboxListFolderResponse {
    pub entries: Vec<DropboxEntry>,
    pub cursor: String,
    pub has_more: bool,
}

#[derive(Deserialize)]
pub(crate) struct UploadSessionStartResponse {
    pub session_id: String,
}

/// Response from `/2/files/list_folder/longpoll` (DBSYNC-30).
#[derive(Deserialize)]
pub(crate) struct DropboxLongpollResponse {
    pub changes: bool,
    /// If present, wait at least this many seconds before longpolling again.
    #[serde(default)]
    pub backoff: Option<u64>,
}

/// The 200 body of `files/upload` and `upload_session/finish` (DBSYNC-99).
///
/// A **bare** `FileMetadata`, with no `.tag` — unlike `list_folder` entries, which are union
/// members, and unlike `move_v2`, which wraps one under `metadata`. Deserializing it as a
/// `DropboxEntry` fails on the missing `.tag`, and because the caller treats a parse failure
/// as best-effort that failure would have been silent: the remote row simply would not have
/// been written and the fix would have been a no-op. Caught by its test before it shipped.
#[derive(Deserialize)]
pub(crate) struct UploadCommitResponse {
    pub id: Option<String>,
    pub content_hash: Option<String>,
    pub rev: Option<String>,
    pub server_modified: Option<String>,
}

/// The 200 body of `files/move_v2` and `files/create_folder_v2`: the moved item's
/// metadata under a `metadata` key (DBSYNC-99). Reusing `DropboxEntry` for the inner
/// value means a `rev` change on a move is recorded rather than discarded.
#[derive(Deserialize)]
pub(crate) struct MoveV2Response {
    pub metadata: DropboxEntry,
}

#[derive(Deserialize)]
pub(crate) struct DropboxEntry {
    #[serde(rename = ".tag")]
    pub tag: String,
    pub path_display: Option<String>,
    pub content_hash: Option<String>,
    pub rev: Option<String>,
    pub server_modified: Option<String>,
    pub size: Option<i64>,
    /// Dropbox's stable item identifier, e.g. `id:eTyPGjL6NDAAAAAAAAABwg` (DBSYNC-99).
    /// Present on every `file` and `folder` entry and unchanged by a rename or a move —
    /// verified against a live account on 2026-09-11, which is the observation ADR-0003
    /// was waiting on. `Option` because `deleted` entries carry no metadata, not because
    /// a live item might lack one.
    pub id: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RemoteEntry {
    pub tag: String,
    pub path_display: String,
    pub size: Option<i64>,
    pub is_synced: bool,
    pub is_excluded: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ListRemoteFolderResponse {
    pub current_path: String,
    pub entries: Vec<RemoteEntry>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TriggerActionResponse {
    pub accepted: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StartupRequirementsResponse {
    pub auth_ok: bool,
    pub sync_folder_ok: bool,
    pub sync_folder: Option<String>,
    /// Whether the Finder Sync extension is switched on (DBSYNC-86). Rides here rather than in
    /// its own command because the UI already re-reads this shape on window focus, which is
    /// exactly when `FinderSync.h` says to re-check.
    pub finder_extension: crate::finder_extension::FinderExtensionState,
}

/// Emitted to the webview after the localhost OAuth redirect is handled (success or failure).
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DropboxOauthFinishedEvent {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Emitted to the webview while `upload_via_session` streams a large file, so the UI can
/// show progress without polling.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UploadProgressEvent {
    pub path: String,
    pub transferred: u64,
    pub total: u64,
}

/// Emitted to the webview when a remote download would clobber unsynced local
/// changes, so the UI can notify the user without polling.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SyncConflictEvent {
    pub path: String,
    pub conflict_path: String,
    pub reason: String,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct CloudscMeta {
    pub version: u8,
    pub tag: String,
    pub remote_path_display: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CloudscPlaceholderInfo {
    pub local_path_display: String,
    pub tag: String,
    pub remote_path_display: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SelectiveSyncFilters {
    pub include_csv: String,
    pub exclude_csv: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct IgnoreGlobs {
    pub csv: String,
}
