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
}

#[derive(Deserialize)]
pub(crate) struct DropboxListFolderResponse {
    pub entries: Vec<DropboxEntry>,
    pub cursor: String,
    pub has_more: bool,
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
