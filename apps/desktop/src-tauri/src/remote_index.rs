use std::collections::HashSet;

use chrono::Utc;
use reqwest::blocking::Client;

use crate::auth_session::get_access_token;
use crate::models::DropboxEntry;
use crate::path_util::normalize_dropbox_path;
use crate::state::AppState;

#[derive(Clone)]
pub(crate) struct RemoteFileMeta {
    pub content_hash: String,
    pub rev: String,
    pub modified_ts: i64,
}

pub(crate) fn parse_rfc3339_ts_to_unix(input: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(input)
        .map(|v| v.with_timezone(&Utc).timestamp())
        .unwrap_or(0)
}

pub(crate) fn fetch_remote_file_metadata(
    state: &AppState,
    relative: &str,
) -> Result<Option<RemoteFileMeta>, String> {
    let token = get_access_token(state)?;
    let client = Client::new();
    let dropbox_path = normalize_dropbox_path(relative);

    let response = client
        .post("https://api.dropboxapi.com/2/files/get_metadata")
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "path": dropbox_path,
            "include_media_info": false,
            "include_deleted": false
        }))
        .send()
        .map_err(|e| format!("get_metadata request failed for {relative}: {e}"))?;

    if response.status().is_success() {
        let entry: DropboxEntry = response
            .json()
            .map_err(|e| format!("get_metadata parse failed for {relative}: {e}"))?;
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
        }));
    }

    let status = response.status();
    let body = response.text().unwrap_or_else(|_| "<unreadable body>".to_string());
    if status.as_u16() == 409 && (body.contains("not_found") || body.contains("path")) {
        return Ok(None);
    }
    Err(format!(
        "get_metadata status error for {relative}: {status}; body: {body}"
    ))
}

pub(crate) fn refresh_remote_index_and_enqueue_downloads_internal(
    state: &AppState,
) -> Result<usize, String> {
    let local_files = state.db.list_local_files()?;
    if local_files.is_empty() {
        return Ok(0);
    }

    let existing_jobs = state.db.list_recent_jobs(400)?;
    let pending_targets: HashSet<String> = existing_jobs
        .iter()
        .filter(|j| j.status == "queued" || j.status == "retry_wait" || j.status == "running")
        .filter_map(|j| j.target_path.clone().or(j.source_path.clone()))
        .collect();

    let mut enqueued = 0usize;
    for local in local_files {
        let rel = local.relative_path;
        if rel.ends_with(".cloudsc") {
            continue;
        }
        if pending_targets.contains(&rel) {
            continue;
        }

        let prev_remote = state.db.get_remote_file(&rel)?;
        let remote_meta = fetch_remote_file_metadata(state, &rel)?;
        let Some(remote_meta) = remote_meta else {
            // Remote copy is gone. Only propagate the deletion when we had
            // previously indexed a remote copy (otherwise the file may simply
            // never have been uploaded yet).
            if let Some(prev) = prev_remote {
                if local.hash == prev.content_hash {
                    // Local matches the last-synced remote content: safe to
                    // delete locally (remote-wins).
                    state.db.enqueue_job("local_delete", Some(&rel), Some(&rel))?;
                    enqueued += 1;
                } else {
                    // Local was modified while the remote was deleted: keep the
                    // local copy and flag a conflict instead of losing data.
                    state.db.add_conflict(
                        &rel,
                        &rel,
                        "remote deleted while local had unsynced changes",
                    )?;
                    state.db.remove_remote_file(&rel)?;
                    if let Ok(mut engine) = state.sync_engine.lock() {
                        engine.record_conflict();
                    }
                }
            }
            continue;
        };

        let should_download = match prev_remote {
            None => false,
            Some(prev) => prev.content_hash != remote_meta.content_hash,
        };

        state
            .db
            .upsert_remote_file(
                &rel,
                &remote_meta.content_hash,
                &remote_meta.rev,
                remote_meta.modified_ts,
            )?;

        if should_download && local.hash != remote_meta.content_hash {
            state.db.enqueue_job("download", Some(&rel), Some(&rel))?;
            enqueued += 1;
        }
    }

    Ok(enqueued)
}
