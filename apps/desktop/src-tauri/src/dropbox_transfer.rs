use std::collections::HashMap;
use std::fs::{self, File};
use std::io;
use std::io::Read;
use std::path::{Path, PathBuf};

use reqwest::blocking::{Body, Client};
use tauri::{Emitter, EventTarget};

use crate::auth_session::get_access_token;
use crate::models::{
    DropboxListFolderResponse, ListRemoteFolderResponse, RemoteEntry, UploadProgressEvent,
    UploadSessionStartResponse,
};
use crate::path_util::{
    hash_file, is_path_allowed, normalize_dropbox_path, parse_prefix_csv,
};
use crate::state::AppState;
use crate::storage::db::FileIndexRow;

/// Atomically replaces `target` with the contents of `temp`, which must live in
/// the same directory as `target` so the replace is a single filesystem
/// metadata update rather than a cross-volume copy.
#[cfg(windows)]
fn atomic_replace(temp: &Path, target: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING};

    let temp_wide: Vec<u16> = temp
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let target_wide: Vec<u16> = target
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY: `temp_wide` and `target_wide` are valid, NUL-terminated UTF-16
    // buffers (LPCWSTR) owned by this function and kept alive for the
    // duration of the call; `MoveFileExW` only reads them and does not retain
    // the pointers past its return.
    let ok = unsafe {
        MoveFileExW(
            temp_wide.as_ptr(),
            target_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING,
        )
    };

    if ok == 0 {
        return Err(format!(
            "atomic replace failed ({} -> {}): {}",
            temp.display(),
            target.display(),
            io::Error::last_os_error()
        ));
    }

    Ok(())
}

/// Atomically replaces `target` with the contents of `temp` (same-directory rename).
#[cfg(not(windows))]
fn atomic_replace(temp: &Path, target: &Path) -> Result<(), String> {
    fs::rename(temp, target).map_err(|e| {
        format!(
            "atomic replace failed ({} -> {}): {e}",
            temp.display(),
            target.display()
        )
    })
}

/// Downloads `path_display` from Dropbox and writes it to `target` without
/// buffering the whole response body in memory: the body is streamed straight
/// to a same-directory temp file, which is then atomically renamed onto
/// `target` so a crash or interruption mid-download never leaves `target`
/// truncated or corrupted.
fn fetch_and_write_file(
    client: &Client,
    token: &str,
    path_display: &str,
    target: &Path,
) -> Result<(), String> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create parent directory: {e}"))?;
    }

    let file_name = target.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let temp = target.with_file_name(format!(
        "{file_name}.dropboxsync-tmp-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));

    let mut download_resp = client
        .post("https://content.dropboxapi.com/2/files/download")
        .bearer_auth(token)
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

    let write_result = File::create(&temp)
        .map_err(|e| format!("failed creating temp file: {e}"))
        .and_then(|mut tmp_file| {
            io::copy(&mut download_resp, &mut tmp_file)
                .map_err(|e| format!("failed writing local file: {e}"))
        });

    if let Err(e) = write_result {
        let _ = fs::remove_file(&temp);
        return Err(e);
    }

    if let Err(e) = atomic_replace(&temp, target) {
        let _ = fs::remove_file(&temp);
        return Err(e);
    }

    Ok(())
}

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

/// Size threshold above which uploads should switch from a single-shot
/// request to a chunked upload session (wired in a later slice). Dropbox
/// itself caps single-request uploads at 150 MiB.
const UPLOAD_SESSION_THRESHOLD_BYTES: u64 = 150 * 1024 * 1024;

/// Chunk size used when streaming a file through the Dropbox upload-session
/// API (`upload_session/start` + `append_v2` + `finish`). Chosen to bound
/// peak memory usage regardless of file size.
const UPLOAD_CHUNK_SIZE: usize = 8 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum UploadStrategy {
    SingleShot,
    Session,
}

/// Pure routing helper: decides which upload strategy a file of `size_bytes`
/// should use.
pub(crate) fn choose_upload_strategy(size_bytes: u64) -> UploadStrategy {
    if size_bytes >= UPLOAD_SESSION_THRESHOLD_BYTES {
        UploadStrategy::Session
    } else {
        UploadStrategy::SingleShot
    }
}

/// Starts a new Dropbox upload session and returns its `session_id`.
fn start_upload_session(client: &Client, token: &str) -> Result<String, String> {
    let resp = client
        .post("https://content.dropboxapi.com/2/files/upload_session/start")
        .bearer_auth(token)
        .header("Dropbox-API-Arg", serde_json::json!({ "close": false }).to_string())
        .header("Content-Type", "application/octet-stream")
        .body(Vec::<u8>::new())
        .send()
        .map_err(|e| format!("upload_session/start request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_else(|_| "<unreadable body>".to_string());
        return Err(format!(
            "upload_session/start status error: {status}; body: {body}"
        ));
    }

    let parsed: UploadSessionStartResponse = resp
        .json()
        .map_err(|e| format!("upload_session/start parse failed: {e}"))?;

    Ok(parsed.session_id)
}

/// Appends one chunk of data to an in-progress upload session at `offset`.
fn append_upload_chunk(
    client: &Client,
    token: &str,
    session_id: &str,
    offset: u64,
    chunk: &[u8],
) -> Result<(), String> {
    let resp = client
        .post("https://content.dropboxapi.com/2/files/upload_session/append_v2")
        .bearer_auth(token)
        .header(
            "Dropbox-API-Arg",
            serde_json::json!({
                "cursor": { "session_id": session_id, "offset": offset },
                "close": false
            })
            .to_string(),
        )
        .header("Content-Type", "application/octet-stream")
        .body(chunk.to_vec())
        .send()
        .map_err(|e| format!("upload_session/append_v2 request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_else(|_| "<unreadable body>".to_string());
        return Err(format!(
            "upload_session/append_v2 status error: {status}; body: {body}"
        ));
    }

    Ok(())
}

/// Finishes an upload session, committing the accumulated bytes plus
/// `last_chunk` to `dropbox_path` (overwrite mode).
fn finish_upload_session(
    client: &Client,
    token: &str,
    session_id: &str,
    offset: u64,
    dropbox_path: &str,
    last_chunk: &[u8],
) -> Result<(), String> {
    let resp = client
        .post("https://content.dropboxapi.com/2/files/upload_session/finish")
        .bearer_auth(token)
        .header(
            "Dropbox-API-Arg",
            serde_json::json!({
                "cursor": { "session_id": session_id, "offset": offset },
                "commit": {
                    "path": dropbox_path,
                    "mode": { ".tag": "overwrite" }
                }
            })
            .to_string(),
        )
        .header("Content-Type", "application/octet-stream")
        .body(last_chunk.to_vec())
        .send()
        .map_err(|e| format!("upload_session/finish request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_else(|_| "<unreadable body>".to_string());
        return Err(format!(
            "upload_session/finish status error: {status}; body: {body}"
        ));
    }

    Ok(())
}

/// Pure boundary check: is the chunk just read (`bytes_read` bytes, starting
/// at `offset`) the last one for a file of `total_len` bytes? Also treats an
/// overshoot (`offset + bytes_read > total_len`) as final, defensively.
fn is_final_chunk(offset: u64, bytes_read: usize, total_len: u64) -> bool {
    offset + bytes_read as u64 >= total_len
}

/// Emits an `upload-progress` event to the main webview, if an `AppHandle` has been set
/// on `state::APP_HANDLE` (see its doc comment). No-ops silently when no handle is set
/// yet, which keeps offline unit tests (which never call `setup()`) window/handle-free.
fn emit_upload_progress(path: &str, transferred: u64, total: u64) {
    let Some(handle) = crate::state::APP_HANDLE.get() else {
        return;
    };

    let event = UploadProgressEvent {
        path: path.to_string(),
        transferred,
        total,
    };

    match handle.emit_to(EventTarget::webview_window("main"), "upload-progress", event.clone()) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("emit_to webview_window(main): {e}");
            if let Err(e2) = handle.emit("upload-progress", event) {
                eprintln!("emit global upload-progress: {e2}");
            }
        }
    }
}

/// Uploads `file` (of `len` bytes) to `dropbox_path` via the Dropbox chunked
/// upload-session API, reading and sending at most `UPLOAD_CHUNK_SIZE` bytes
/// at a time so peak memory stays bounded regardless of file size.
///
/// The upload is a point-in-time snapshot of `len` bytes: `len` is captured
/// once by the caller. If the file is truncated concurrently, EOF is reached
/// before `offset` hits `len` and the session is finished with whatever bytes
/// were read (never a busy-loop). Concurrent growth beyond `len` is ignored.
fn upload_via_session(
    token: &str,
    mut file: File,
    len: u64,
    dropbox_path: &str,
) -> Result<(), String> {
    let client = Client::new();
    let session_id = start_upload_session(&client, token)?;

    let mut buf = vec![0u8; UPLOAD_CHUNK_SIZE];
    let mut offset: u64 = 0;

    loop {
        let mut n = 0usize;
        while n < buf.len() {
            let read = file
                .read(&mut buf[n..])
                .map_err(|e| format!("failed reading local file for upload: {e}"))?;
            if read == 0 {
                break;
            }
            n += read;
        }

        // Premature EOF (file shrank since `len` was read): finishing here
        // avoids an infinite loop of empty `append_v2` calls when `offset < len`.
        // When `is_final_chunk` is already true this is a no-op distinction.
        if n == 0 && !is_final_chunk(offset, n, len) {
            finish_upload_session(&client, token, &session_id, offset, dropbox_path, &[])?;
            emit_upload_progress(dropbox_path, offset, len);
            break;
        }

        if is_final_chunk(offset, n, len) {
            finish_upload_session(&client, token, &session_id, offset, dropbox_path, &buf[..n])?;
            offset += n as u64;
            emit_upload_progress(dropbox_path, offset, len);
            break;
        } else {
            append_upload_chunk(&client, token, &session_id, offset, &buf[..n])?;
            offset += n as u64;
            emit_upload_progress(dropbox_path, offset, len);
        }
    }

    Ok(())
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

    let file = File::open(&local_path)
        .map_err(|e| format!("failed opening local file for upload: {e}"))?;
    let len = file
        .metadata()
        .map_err(|e| format!("failed reading local file metadata: {e}"))?
        .len();

    let dropbox_path = normalize_dropbox_path(relative);

    match choose_upload_strategy(len) {
        UploadStrategy::SingleShot => {
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
                .body(Body::sized(file, len))
                .send()
                .map_err(|e| format!("upload request failed: {e}"))?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().unwrap_or_else(|_| "<unreadable body>".to_string());
                return Err(format!(
                    "upload status error for {dropbox_path}: {status}; body: {body}"
                ));
            }
        }
        UploadStrategy::Session => {
            upload_via_session(&token, file, len, &dropbox_path)?;
        }
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

    let target = PathBuf::from(&folder).join(&relative);
    fetch_and_write_file(&Client::new(), &token, path_display, &target)?;

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
            fetch_and_write_file(&client, &token, path_display, &target)?;

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

            if known_map.contains_key(&relative) && target.exists() {
                continue;
            }

            fetch_and_write_file(&client, &token, path_display, &target)?;
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

#[cfg(test)]
mod tests {
    use super::{
        atomic_replace, choose_upload_strategy, emit_upload_progress, is_final_chunk,
        UploadStrategy, UPLOAD_CHUNK_SIZE, UPLOAD_SESSION_THRESHOLD_BYTES,
    };

    // `fetch_and_write_file` performs live HTTP calls against the Dropbox API
    // and is intentionally left to manual QA (large-file download against a
    // real account, verifying flat memory usage and crash-safety).

    // The streamed single-shot upload body (`Body::sized` over an open file
    // handle) in `upload_local_file_internal` is likewise manual-QA-only: it
    // needs a live Dropbox account to exercise, and this test module only
    // covers the pure `choose_upload_strategy` routing helper below.

    // `start_upload_session`, `append_upload_chunk`, `finish_upload_session`
    // and `upload_via_session` all perform live HTTP calls against the
    // Dropbox API and are manual-QA-only (real >150 MB file, verifying
    // bounded memory usage and that a retry starts a fresh session). This
    // module covers the pure `is_final_chunk` boundary-math helper instead.

    #[test]
    fn choose_upload_strategy_single_shot_for_small_sizes() {
        assert_eq!(choose_upload_strategy(0), UploadStrategy::SingleShot);
        assert_eq!(choose_upload_strategy(1), UploadStrategy::SingleShot);
        assert_eq!(
            choose_upload_strategy(UPLOAD_SESSION_THRESHOLD_BYTES - 1),
            UploadStrategy::SingleShot
        );
    }

    #[test]
    fn choose_upload_strategy_session_at_and_above_threshold() {
        assert_eq!(
            choose_upload_strategy(UPLOAD_SESSION_THRESHOLD_BYTES),
            UploadStrategy::Session
        );
        assert_eq!(
            choose_upload_strategy(UPLOAD_SESSION_THRESHOLD_BYTES + 1),
            UploadStrategy::Session
        );
        assert_eq!(
            choose_upload_strategy(10 * 1024 * 1024 * 1024),
            UploadStrategy::Session
        );
    }

    #[test]
    fn is_final_chunk_empty_file_is_final() {
        assert!(is_final_chunk(0, 0, 0));
    }

    #[test]
    fn is_final_chunk_mid_file_is_not_final() {
        let chunk = UPLOAD_CHUNK_SIZE as u64;
        // First chunk read in full, more than one chunk remains.
        assert!(!is_final_chunk(0, UPLOAD_CHUNK_SIZE, 3 * chunk));
    }

    #[test]
    fn is_final_chunk_not_final_when_bytes_remain_after_exact_chunk() {
        let chunk = UPLOAD_CHUNK_SIZE as u64;
        // offset + read lands exactly on a chunk boundary, but a further
        // chunk still remains beyond it (3-chunk file, second chunk read).
        assert!(!is_final_chunk(chunk, UPLOAD_CHUNK_SIZE, 3 * chunk));
    }

    #[test]
    fn is_final_chunk_final_when_offset_plus_read_equals_total() {
        let chunk = UPLOAD_CHUNK_SIZE as u64;
        assert!(is_final_chunk(chunk, UPLOAD_CHUNK_SIZE, 2 * chunk));
    }

    #[test]
    fn is_final_chunk_final_with_one_byte_remainder() {
        // Total length is one byte more than an exact multiple of the chunk
        // size, so the last read is a 1-byte chunk.
        let chunk = UPLOAD_CHUNK_SIZE as u64;
        assert!(is_final_chunk(chunk, 1, chunk + 1));
    }

    #[test]
    fn is_final_chunk_treats_overshoot_as_final_defensively() {
        assert!(is_final_chunk(10, 5, 12));
    }

    #[test]
    fn premature_eof_guard_condition_detects_shrunk_file() {
        // The session loop's premature-EOF guard fires when a read returns 0
        // bytes (`n == 0`) while `offset < len` (file shrank mid-upload). This
        // is what prevents the infinite empty-`append_v2` loop (DBSYNC-11 M1).
        let (offset, n, len) = (16u64, 0usize, 24u64); // read 0 with 8 bytes still "expected"
        assert!(n == 0 && !is_final_chunk(offset, n, len));
        // At/after the expected end, a 0-byte read is a normal terminal finish,
        // not the premature-EOF path.
        assert!(is_final_chunk(24, 0, 24));
    }

    #[test]
    fn atomic_replace_overwrites_existing_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let temp = dir.path().join("file.tmp");
        let target = dir.path().join("file.txt");

        std::fs::write(&temp, b"new").expect("write temp");
        std::fs::write(&target, b"old").expect("write target");

        atomic_replace(&temp, &target).expect("atomic_replace");

        assert_eq!(std::fs::read(&target).expect("read target"), b"new");
        assert!(!temp.exists(), "temp file should no longer exist");
    }

    #[test]
    fn atomic_replace_creates_when_target_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let temp = dir.path().join("file.tmp");
        let target = dir.path().join("file.txt");

        std::fs::write(&temp, b"contents").expect("write temp");
        assert!(!target.exists());

        atomic_replace(&temp, &target).expect("atomic_replace");

        assert_eq!(std::fs::read(&target).expect("read target"), b"contents");
        assert!(!temp.exists(), "temp file should no longer exist");
    }

    #[test]
    fn emit_upload_progress_is_a_no_op_without_app_handle() {
        // `state::APP_HANDLE` is unset in this offline test binary (`setup()` never
        // runs), so this must not panic and must not attempt to emit anything.
        emit_upload_progress("x/y.txt", 10, 20);
    }
}
