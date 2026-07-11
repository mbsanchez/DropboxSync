use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use reqwest::blocking::Client;

use crate::auth_session::get_access_token;
use crate::models::{
    DropboxListFolderResponse, ListRemoteFolderResponse, RemoteEntry,
};
use crate::path_util::{
    hash_file, is_path_allowed, normalize_dropbox_path, parse_prefix_csv,
};
use crate::state::AppState;
use crate::storage::db::FileIndexRow;

pub(crate) fn list_remote_folder(
    state: &AppState,
    path: String,
) -> Result<ListRemoteFolderResponse, String> {
    let token = get_access_token(state)?;

    let folder = state
        .db
        .get_sync_folder()?
        .ok_or_else(|| "sync folder not configured".to_string())?;

    let include_prefixes = parse_prefix_csv(state.db.get_include_prefixes_csv()?);
    let exclude_prefixes = parse_prefix_csv(state.db.get_exclude_prefixes_csv()?);

    let dropbox_path = normalize_dropbox_path(&path);

    let client = Client::new();
    let response = client
        .post("https://api.dropboxapi.com/2/files/list_folder")
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "path": dropbox_path,
            "recursive": false,
            "include_deleted": false
        }))
        .send()
        .map_err(|e| format!("list_folder request failed: {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_else(|_| "<unreadable body>".to_string());
        return Err(format!("list_folder status error: {status}; body: {body}"));
    }

    let entries_resp: DropboxListFolderResponse = response
        .json()
        .map_err(|e| format!("list_folder parse failed: {e}"))?;

    let mut entries = Vec::new();
    for entry in entries_resp.entries {
        let Some(path_display) = entry.path_display else {
            continue;
        };
        let relative = path_display.trim_start_matches('/').to_string();
        let local_target = PathBuf::from(&folder).join(&relative);
        let is_synced = local_target.exists();
        let is_excluded = !is_path_allowed(&relative, &include_prefixes, &exclude_prefixes);

        entries.push(RemoteEntry {
            tag: entry.tag,
            path_display,
            size: entry.size,
            is_synced,
            is_excluded,
        });
    }

    Ok(ListRemoteFolderResponse {
        current_path: dropbox_path,
        entries,
    })
}

pub(crate) fn upload_local_file_internal(state: &AppState, relative: &str) -> Result<(), String> {
    if relative.ends_with(".cloudsc") {
        return Ok(());
    }

    let token = get_access_token(state)?;
    let folder = state
        .db
        .get_sync_folder()?
        .ok_or_else(|| "sync folder not configured".to_string())?;

    let local_path = PathBuf::from(&folder).join(relative);
    if !local_path.exists() {
        return Err(format!("local file missing for upload: {relative}"));
    }

    // Skip the upload when Dropbox already holds identical content. This avoids
    // re-uploading files that originated from Dropbox (e.g. after a sync-state
    // reset re-indexes existing downloads as "new" local files).
    if let Some(local) = state.db.get_local_file(relative)? {
        if let Some(remote) = crate::remote_index::fetch_remote_file_metadata(state, relative)? {
            if remote.content_hash == local.hash {
                state.db.upsert_remote_file(
                    relative,
                    &remote.content_hash,
                    &remote.rev,
                    remote.modified_ts,
                )?;
                return Ok(());
            }
        }
    }

    let bytes = fs::read(&local_path)
        .map_err(|e| format!("failed reading local file bytes for upload: {e}"))?;

    let dropbox_path = normalize_dropbox_path(relative);
    let client = Client::new();
    let resp = client
        .post("https://content.dropboxapi.com/2/files/upload")
        .bearer_auth(&token)
        .header(
            "Dropbox-API-Arg",
            serde_json::json!({
                "path": dropbox_path,
                "mode": { ".tag": "overwrite" }
            })
            .to_string(),
        )
        .header("Content-Type", "application/octet-stream")
        .body(bytes)
        .send()
        .map_err(|e| format!("upload request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_else(|_| "<unreadable body>".to_string());
        return Err(format!(
            "upload status error for {dropbox_path}: {status}; body: {body}"
        ));
    }

    Ok(())
}

pub(crate) fn delete_remote_file_internal(state: &AppState, relative: &str) -> Result<(), String> {
    if relative.ends_with(".cloudsc") {
        return Ok(());
    }

    let token = get_access_token(state)?;
    let dropbox_path = normalize_dropbox_path(relative);
    let client = Client::new();
    let resp = client
        .post("https://api.dropboxapi.com/2/files/delete_v2")
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "path": dropbox_path
        }))
        .send()
        .map_err(|e| format!("delete request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_else(|_| "<unreadable body>".to_string());
        return Err(format!(
            "delete status error for {dropbox_path}: {status}; body: {body}"
        ));
    }

    Ok(())
}

/// Removes a local file that was deleted on the remote (remote-wins deletion).
/// Cleans both index tables so the file is not re-detected on the next tick.
pub(crate) fn delete_local_file_internal(state: &AppState, relative: &str) -> Result<(), String> {
    let folder = state
        .db
        .get_sync_folder()?
        .ok_or_else(|| "sync folder not configured".to_string())?;

    let local_path = PathBuf::from(&folder).join(relative);
    if local_path.exists() {
        fs::remove_file(&local_path)
            .map_err(|e| format!("failed to delete local file {relative}: {e}"))?;
    }

    state.db.remove_local_file(relative)?;
    state.db.remove_remote_file(relative)?;
    Ok(())
}

pub(crate) fn download_remote_file_internal(state: &AppState, path_display: &str) -> Result<(), String> {
    let token = get_access_token(state)?;
    let folder = state
        .db
        .get_sync_folder()?
        .ok_or_else(|| "sync folder not configured".to_string())?;

    let relative = path_display.trim_start_matches('/').to_string();
    let include_prefixes = parse_prefix_csv(state.db.get_include_prefixes_csv()?);
    let exclude_prefixes = parse_prefix_csv(state.db.get_exclude_prefixes_csv()?);
    if !is_path_allowed(&relative, &include_prefixes, &exclude_prefixes) {
        return Ok(());
    }

    let client = Client::new();
    let download_resp = client
        .post("https://content.dropboxapi.com/2/files/download")
        .bearer_auth(&token)
        .header(
            "Dropbox-API-Arg",
            serde_json::json!({ "path": path_display }).to_string(),
        )
        .send()
        .map_err(|e| format!("download request failed: {e}"))?;

    if !download_resp.status().is_success() {
        let status = download_resp.status();
        let body = download_resp.text().unwrap_or_else(|_| "<unreadable body>".to_string());
        return Err(format!(
            "download status error for {path_display}: {status}; body: {body}"
        ));
    }

    let bytes = download_resp
        .bytes()
        .map_err(|e| format!("download bytes failed: {e}"))?;

    let target = PathBuf::from(&folder).join(&relative);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create parent directory: {e}"))?;
    }

    fs::write(&target, &bytes).map_err(|e| format!("failed writing local file: {e}"))?;
    let (hash, size_bytes, modified_ts) = hash_file(&target)?;
    state
        .db
        .upsert_local_file(&relative, &hash, size_bytes, modified_ts)?;
    Ok(())
}

pub(crate) fn hydrate_remote_folder_internal(
    state: &AppState,
    folder_path_display: &str,
) -> Result<usize, String> {
    let token = get_access_token(state)?;
    let folder = state
        .db
        .get_sync_folder()?
        .ok_or_else(|| "sync folder not configured".to_string())?;

    let include_prefixes = parse_prefix_csv(state.db.get_include_prefixes_csv()?);
    let exclude_prefixes = parse_prefix_csv(state.db.get_exclude_prefixes_csv()?);

    fs::create_dir_all(&folder).map_err(|e| format!("failed to create sync folder: {e}"))?;

    let client = Client::new();
    let mut downloaded = 0usize;

    let response = client
        .post("https://api.dropboxapi.com/2/files/list_folder")
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "path": folder_path_display,
            "recursive": true,
            "include_deleted": false
        }))
        .send()
        .map_err(|e| format!("list_folder request failed: {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_else(|_| "<unreadable body>".to_string());
        return Err(format!(
            "list_folder status error for {folder_path_display}: {status}; body: {body}"
        ));
    }

    let mut entries_resp: DropboxListFolderResponse = response
        .json()
        .map_err(|e| format!("list_folder parse failed: {e}"))?;

    loop {
        for entry in &entries_resp.entries {
            if entry.tag != "file" {
                continue;
            }
            let Some(path_display) = &entry.path_display else {
                continue;
            };
            let relative = path_display.trim_start_matches('/').to_string();
            if !is_path_allowed(&relative, &include_prefixes, &exclude_prefixes) {
                continue;
            }

            let target = PathBuf::from(&folder).join(&relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("failed to create parent directory: {e}"))?;
            }

            let download_resp = client
                .post("https://content.dropboxapi.com/2/files/download")
                .bearer_auth(&token)
                .header(
                    "Dropbox-API-Arg",
                    serde_json::json!({ "path": path_display }).to_string(),
                )
                .send()
                .map_err(|e| format!("download request failed: {e}"))?;

            if !download_resp.status().is_success() {
                let status = download_resp.status();
                let body = download_resp.text().unwrap_or_else(|_| "<unreadable body>".to_string());
                return Err(format!(
                    "download status error for {path_display}: {status}; body: {body}"
                ));
            }

            let bytes = download_resp
                .bytes()
                .map_err(|e| format!("download bytes failed: {e}"))?;
            fs::write(&target, &bytes).map_err(|e| format!("failed writing local file: {e}"))?;
            let (hash, size_bytes, modified_ts) = hash_file(&target)?;
            state.db.upsert_local_file(&relative, &hash, size_bytes, modified_ts)?;
            downloaded += 1;
        }

        if !entries_resp.has_more {
            break;
        }

        entries_resp = client
            .post("https://api.dropboxapi.com/2/files/list_folder/continue")
            .bearer_auth(&token)
            .json(&serde_json::json!({ "cursor": entries_resp.cursor.clone() }))
            .send()
            .map_err(|e| format!("list_folder/continue request failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("list_folder/continue status error: {e}"))?
            .json()
            .map_err(|e| format!("list_folder/continue parse failed: {e}"))?;
    }

    Ok(downloaded)
}

#[allow(dead_code)]
pub(crate) fn pull_remote_snapshot_internal(state: &AppState) -> Result<usize, String> {
    let token = match get_access_token(state) {
        Ok(token) => token,
        Err(_) => return Ok(0),
    };
    let folder = state
        .db
        .get_sync_folder()?
        .ok_or_else(|| "sync folder not configured".to_string())?;
    fs::create_dir_all(&folder).map_err(|e| format!("failed to create sync folder: {e}"))?;

    let client = Client::new();
    let mut downloaded = 0usize;
    let known_map: HashMap<String, FileIndexRow> = state
        .db
        .list_local_files()?
        .into_iter()
        .map(|f| (f.relative_path.clone(), f))
        .collect();

    let list_folder_resp = client
        .post("https://api.dropboxapi.com/2/files/list_folder")
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "path": "",
            "recursive": true,
            "include_deleted": false
        }))
        .send()
        .map_err(|e| format!("list_folder request failed: {e}"))?;
    if !list_folder_resp.status().is_success() {
        let status = list_folder_resp.status();
        let body = list_folder_resp.text().unwrap_or_else(|_| "<unreadable body>".to_string());
        return Err(format!("list_folder status error: {status}; body: {body}"));
    }
    let mut response: DropboxListFolderResponse = list_folder_resp
        .json()
        .map_err(|e| format!("list_folder parse failed: {e}"))?;

    loop {
        for entry in &response.entries {
            if entry.tag != "file" {
                continue;
            }
            let Some(path_display) = &entry.path_display else {
                continue;
            };

            let relative = path_display.trim_start_matches('/').to_string();
            let target = PathBuf::from(&folder).join(&relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("failed to create parent directory: {e}"))?;
            }

            if known_map.contains_key(&relative) && target.exists() {
                continue;
            }

            let download_resp = client
                .post("https://content.dropboxapi.com/2/files/download")
                .bearer_auth(&token)
                .header(
                    "Dropbox-API-Arg",
                    serde_json::json!({ "path": path_display }).to_string(),
                )
                .send()
                .map_err(|e| format!("download request failed: {e}"))?;
            if !download_resp.status().is_success() {
                let status = download_resp.status();
                let body = download_resp.text().unwrap_or_else(|_| "<unreadable body>".to_string());
                return Err(format!(
                    "download status error for {path_display}: {status}; body: {body}"
                ));
            }
            let bytes = download_resp
                .bytes()
                .map_err(|e| format!("download bytes failed: {e}"))?;

            fs::write(&target, &bytes).map_err(|e| format!("failed writing local file: {e}"))?;
            let (hash, size_bytes, modified_ts) = hash_file(&target)?;
            state
                .db
                .upsert_local_file(&relative, &hash, size_bytes, modified_ts)?;
            downloaded += 1;
        }

        if !response.has_more {
            break;
        }

        let continue_resp = client
            .post("https://api.dropboxapi.com/2/files/list_folder/continue")
            .bearer_auth(&token)
            .json(&serde_json::json!({ "cursor": response.cursor.clone() }))
            .send()
            .map_err(|e| format!("list_folder/continue request failed: {e}"))?;
        if !continue_resp.status().is_success() {
            let status = continue_resp.status();
            let body = continue_resp.text().unwrap_or_else(|_| "<unreadable body>".to_string());
            return Err(format!(
                "list_folder/continue status error: {status}; body: {body}"
            ));
        }
        response = continue_resp
            .json()
            .map_err(|e| format!("list_folder/continue parse failed: {e}"))?;
    }

    Ok(downloaded)
}
