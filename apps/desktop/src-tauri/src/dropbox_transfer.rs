use std::collections::HashMap;
use std::fs::{self, File};
use std::io;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Duration;

use reqwest::blocking::{Body, Client};
use tauri::{Emitter, EventTarget};

use crate::auth_session::get_access_token;
use crate::error::{AppError, AppResult};
use crate::models::{
    DropboxListFolderResponse, ListRemoteFolderResponse, RemoteEntry, SyncConflictEvent,
    UploadProgressEvent, UploadSessionStartResponse,
};
use crate::path_util::{
    create_conflicted_copy, hash_file, is_ignored_local_path, is_path_allowed,
    normalize_dropbox_path, parse_prefix_csv, relpath_under, safe_join,
};
use crate::state::AppState;
use crate::storage::db::FileIndexRow;

/// Atomically replaces `target` with the contents of `temp`, which must live in
/// the same directory as `target` so the replace is a single filesystem
/// metadata update rather than a cross-volume copy.
#[cfg(windows)]
fn atomic_replace(temp: &Path, target: &Path) -> AppResult<()> {
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
        return Err(AppError::Io(format!(
            "atomic replace failed ({} -> {}): {}",
            temp.display(),
            target.display(),
            io::Error::last_os_error()
        )));
    }

    Ok(())
}

/// Atomically replaces `target` with the contents of `temp` (same-directory rename).
#[cfg(not(windows))]
fn atomic_replace(temp: &Path, target: &Path) -> AppResult<()> {
    fs::rename(temp, target).map_err(|e| {
        AppError::Io(format!(
            "atomic replace failed ({} -> {}): {e}",
            temp.display(),
            target.display()
        ))
    })
}

/// Walks a `reqwest::Error`'s `source()` chain and joins every message so the
/// real underlying cause (connection reset, timed out, TLS failure, etc.) is
/// surfaced instead of the generic top-level "error sending request for url".
fn describe_reqwest_error(e: &reqwest::Error) -> String {
    let mut msg = e.to_string();
    let mut cause = std::error::Error::source(e);
    while let Some(err) = cause {
        msg.push_str(" | caused by: ");
        msg.push_str(&err.to_string());
        cause = err.source();
    }
    msg
}

/// Retries `f` up to `max_attempts` times when it fails with a *transient*
/// error per [`AppError::is_transient`]: a transport-level failure
/// (`AppError::Network`, set by every `.map_err` around a `.send()` call), a
/// Dropbox rate-limit (`too_many_write_operations` / `too_many_requests` /
/// HTTP 429), or a server-side error (HTTP 5xx) — all of which are worth
/// backing off and retrying rather than failing the whole job outright.
/// Other HTTP status errors (e.g. plain 4xx) are not retried here since those
/// are either handled by session-resume logic or should bubble straight up.
/// Uses a linear backoff (`2s * attempt`) between attempts.
fn retry_transient<T>(
    label: &str,
    max_attempts: u32,
    mut f: impl FnMut() -> AppResult<T>,
) -> AppResult<T> {
    let mut attempt: u32 = 1;
    loop {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) => {
                let transient = e.is_transient();
                if transient && attempt < max_attempts {
                    let wait_secs = 2u64 * attempt as u64;
                    tracing::warn!(
                        label,
                        attempt,
                        max_attempts,
                        wait_secs,
                        error = %e,
                        "transient error, retrying"
                    );
                    std::thread::sleep(Duration::from_secs(wait_secs));
                    attempt += 1;
                    continue;
                }
                return Err(e);
            }
        }
    }
}

/// Downloads `path_display` from Dropbox and writes it to `target` without
/// buffering the whole response body in memory: the body is streamed straight
/// to a same-directory temp file, which is then atomically renamed onto
/// `target` so a crash or interruption mid-download never leaves `target`
/// truncated or corrupted.
/// Download a remote file's full bytes to an arbitrary local `target` path, for
/// CfAPI on-demand hydration (DBSYNC-59): the fetch-data callback streams the
/// bytes back to the platform via `CfExecute`. Reuses the tested whole-file
/// download; deliberately does NOT touch the DB index, the sync-root target path,
/// or the conflict guard (that is `download_remote_file_internal`'s job).
#[cfg(windows)]
pub(crate) fn download_to_path(state: &AppState, path_display: &str, target: &Path) -> AppResult<()> {
    let token = get_access_token(state)?;
    fetch_and_write_file(&state.http_client, &token, path_display, target)
}

fn fetch_and_write_file(
    client: &Client,
    token: &str,
    path_display: &str,
    target: &Path,
) -> AppResult<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| AppError::Io(format!("failed to create parent directory: {e}")))?;
    }

    let file_name = target.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let temp = target.with_file_name(format!(
        "{file_name}.dropboxsync-tmp-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));

    // 300s (not the 120s used for upload-session/single-shot-upload requests):
    // reqwest's blocking `timeout` bounds the *entire* request including
    // streaming the full response body, and this is a single-request
    // whole-file download — a large placeholder hydration could legitimately
    // exceed 120s on a slow link, so 120s would regress large-file hydration.
    // 300s still bounds a true stall. A future ranged/chunked download could
    // tighten this back down.
    let mut download_resp = client
        .post("https://content.dropboxapi.com/2/files/download")
        .bearer_auth(token)
        .header(
            "Dropbox-API-Arg",
            serde_json::json!({ "path": path_display }).to_string(),
        )
        .timeout(Duration::from_secs(300))
        .send()
        .map_err(|e| {
            AppError::Network(format!(
                "download request failed: {}",
                describe_reqwest_error(&e)
            ))
        })?;

    if !download_resp.status().is_success() {
        let status = download_resp.status();
        let body = download_resp.text().unwrap_or_else(|_| "<unreadable body>".to_string());
        return Err(AppError::Dropbox {
            status: status.as_u16(),
            message: format!("download for {path_display}: {body}"),
        });
    }

    // Stream the body to disk in chunks (instead of io::copy) so we can emit
    // throttled `download-progress` events for the sync-progress UI (DBSYNC-34).
    let total = download_resp.content_length().unwrap_or(0);
    let write_result: AppResult<()> = (|| {
        let mut tmp_file = File::create(&temp)
            .map_err(|e| AppError::Io(format!("failed creating temp file: {e}")))?;
        let mut buf = vec![0u8; 64 * 1024];
        let mut transferred: u64 = 0;
        let mut last_emit = std::time::Instant::now();
        let mut last_emit_bytes: u64 = 0;
        emit_download_progress(path_display, 0, total);
        loop {
            let n = download_resp
                .read(&mut buf)
                .map_err(|e| AppError::Io(format!("failed reading download stream: {e}")))?;
            if n == 0 {
                break;
            }
            tmp_file
                .write_all(&buf[..n])
                .map_err(|e| AppError::Io(format!("failed writing local file: {e}")))?;
            transferred += n as u64;
            // Throttle: emit at most every ~256 KiB or ~100ms.
            if transferred - last_emit_bytes >= 256 * 1024
                || last_emit.elapsed() >= Duration::from_millis(100)
            {
                emit_download_progress(path_display, transferred, total);
                last_emit = std::time::Instant::now();
                last_emit_bytes = transferred;
            }
        }
        // Final 100% tick (total may be 0/unknown; report the actual byte count).
        emit_download_progress(path_display, transferred, transferred.max(total));
        Ok(())
    })();

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
) -> AppResult<ListRemoteFolderResponse> {
    let token = get_access_token(state)?;

    let folder = state
        .db
        .get_sync_folder()?
        .ok_or_else(|| AppError::Sync("sync folder not configured".to_string()))?;

    let include_prefixes = parse_prefix_csv(state.db.get_include_prefixes_csv()?);
    let exclude_prefixes = parse_prefix_csv(state.db.get_exclude_prefixes_csv()?);

    let dropbox_path = normalize_dropbox_path(&path)?;

    let client = &state.http_client;
    let response = client
        .post("https://api.dropboxapi.com/2/files/list_folder")
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({
            "path": dropbox_path,
            "recursive": false,
            "include_deleted": false
        }))
        .send()
        .map_err(|e| {
            AppError::Network(format!(
                "list_folder request failed: {}",
                describe_reqwest_error(&e)
            ))
        })?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_else(|_| "<unreadable body>".to_string());
        return Err(AppError::Dropbox {
            status: status.as_u16(),
            message: format!("list_folder: {body}"),
        });
    }

    let entries_resp: DropboxListFolderResponse = response
        .json()
        .map_err(|e| AppError::Other(format!("list_folder parse failed: {e}")))?;

    let mut entries = Vec::new();
    for entry in entries_resp.entries {
        let Some(path_display) = entry.path_display else {
            continue;
        };
        let relative = path_display.trim_start_matches('/').to_string();
        // A poisoned remote entry that tries to traverse out of the sync root is
        // never "synced"; refuse the join and treat it as not present locally.
        let is_synced = safe_join(Path::new(&folder), &relative)
            .map(|p| p.exists())
            .unwrap_or(false);
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
fn start_upload_session(client: &Client, token: &str) -> AppResult<String> {
    let resp = client
        .post("https://content.dropboxapi.com/2/files/upload_session/start")
        .bearer_auth(token)
        .header("Dropbox-API-Arg", serde_json::json!({ "close": false }).to_string())
        .header("Content-Type", "application/octet-stream")
        .body(Vec::<u8>::new())
        .timeout(Duration::from_secs(120))
        .send()
        .map_err(|e| {
            AppError::Network(format!(
                "upload_session/start request failed: {}",
                describe_reqwest_error(&e)
            ))
        })?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_else(|_| "<unreadable body>".to_string());
        return Err(AppError::Dropbox {
            status: status.as_u16(),
            message: format!("upload_session/start: {body}"),
        });
    }

    let parsed: UploadSessionStartResponse = resp
        .json()
        .map_err(|e| AppError::Other(format!("upload_session/start parse failed: {e}")))?;

    Ok(parsed.session_id)
}

/// Appends one chunk of data to an in-progress upload session at `offset`.
fn append_upload_chunk(
    client: &Client,
    token: &str,
    session_id: &str,
    offset: u64,
    chunk: &[u8],
) -> AppResult<()> {
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
        .timeout(Duration::from_secs(120))
        .send()
        .map_err(|e| {
            AppError::Network(format!(
                "upload_session/append_v2 request failed: {}",
                describe_reqwest_error(&e)
            ))
        })?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_else(|_| "<unreadable body>".to_string());
        return Err(AppError::Dropbox {
            status: status.as_u16(),
            message: format!("upload_session/append_v2: {body}"),
        });
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
) -> AppResult<()> {
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
        .timeout(Duration::from_secs(120))
        .send()
        .map_err(|e| {
            AppError::Network(format!(
                "upload_session/finish request failed: {}",
                describe_reqwest_error(&e)
            ))
        })?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_else(|_| "<unreadable body>".to_string());
        return Err(AppError::Dropbox {
            status: status.as_u16(),
            message: format!("upload_session/finish: {body}"),
        });
    }

    Ok(())
}

/// Pure boundary check: is the chunk just read (`bytes_read` bytes, starting
/// at `offset`) the last one for a file of `total_len` bytes? Also treats an
/// overshoot (`offset + bytes_read > total_len`) as final, defensively.
fn is_final_chunk(offset: u64, bytes_read: usize, total_len: u64) -> bool {
    offset + bytes_read as u64 >= total_len
}

/// Would writing the freshly-downloaded remote content over `target` destroy
/// local changes that were never synced? True only when the file exists on
/// disk, we have a previously-synced baseline hash for it, and the current
/// on-disk hash differs from that baseline (i.e. the user edited the file
/// locally since the last sync, and the remote copy is not simply
/// re-affirming what we already have).
fn download_would_conflict(
    target_exists: bool,
    on_disk_hash: &str,
    baseline_hash: Option<&str>,
) -> bool {
    if !target_exists {
        return false;
    }
    match baseline_hash {
        None => false, // never indexed locally — nothing to protect against
        Some(baseline) => on_disk_hash != baseline,
    }
}

/// Does a conflicted-copy sibling of `target` already hold exactly `content_hash`?
///
/// Dedupe guard for the download-conflict path: a `download` job that fails its
/// network fetch is retried (up to `max_attempts`), and each retry re-observes the
/// same unsynced local edit — without this check it would spawn a fresh
/// `(conflicted copy …)` file and a duplicate `sync_conflicts` row every attempt.
/// Matching by CONTENT (not by path/existence) means a genuinely different *later*
/// local edit still produces a new copy, and it survives app restarts since the
/// copies live on disk. Best-effort: any read/hash error is treated as "not found"
/// so it can never suppress a real conflict copy.
fn conflict_copy_with_content_exists(target: &Path, content_hash: &str) -> AppResult<bool> {
    let (Some(parent), Some(stem)) = (target.parent(), target.file_stem()) else {
        return Ok(false);
    };
    let prefix = format!("{} (conflicted copy ", stem.to_string_lossy());
    let entries = match std::fs::read_dir(parent) {
        Ok(e) => e,
        Err(_) => return Ok(false),
    };
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        if !entry.file_name().to_string_lossy().starts_with(&prefix) {
            continue;
        }
        if let Ok((hash, _size, _mtime)) = hash_file(&entry.path()) {
            if hash == content_hash {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Emits an `upload-progress` event to the main webview, if an `AppHandle` has been set
/// on `state::APP_HANDLE` (see its doc comment). No-ops silently when no handle is set
/// yet, which keeps offline unit tests (which never call `setup()`) window/handle-free.
fn emit_upload_progress(path: &str, transferred: u64, total: u64) {
    emit_transfer_progress("upload-progress", path, transferred, total);
}

/// Emits a `download-progress` event with the same payload shape as
/// `upload-progress`, so the flyout can show live download progress. The caller
/// throttles emits (bytes/time) to avoid flooding the event bus.
fn emit_download_progress(path: &str, transferred: u64, total: u64) {
    emit_transfer_progress("download-progress", path, transferred, total);
}

/// Shared emitter for `upload-progress` / `download-progress` events. No-ops
/// silently when no `AppHandle` is set on `state::APP_HANDLE` yet (keeps offline
/// unit tests, which never call `setup()`, handle-free).
fn emit_transfer_progress(event_name: &'static str, path: &str, transferred: u64, total: u64) {
    let Some(handle) = crate::state::APP_HANDLE.get() else {
        return;
    };

    let event = UploadProgressEvent {
        path: path.to_string(),
        transferred,
        total,
    };

    match handle.emit_to(EventTarget::webview_window("main"), event_name, event.clone()) {
        Ok(()) => {}
        Err(e) => {
            tracing::error!(error = %e, event = event_name, "emit_to webview_window(main) failed");
            if let Err(e2) = handle.emit(event_name, event) {
                tracing::error!(error = %e2, event = event_name, "emit global progress failed");
            }
        }
    }
}

/// Emits a `sync-conflict` event to the main webview, if an `AppHandle` has been set
/// on `state::APP_HANDLE` (see its doc comment). No-ops silently when no handle is set
/// yet, which keeps offline unit tests (which never call `setup()`) window/handle-free.
fn emit_sync_conflict(path: &str, conflict_path: &str, reason: &str) {
    let Some(handle) = crate::state::APP_HANDLE.get() else {
        return;
    };

    let event = SyncConflictEvent {
        path: path.to_string(),
        conflict_path: conflict_path.to_string(),
        reason: reason.to_string(),
    };

    match handle.emit_to(EventTarget::webview_window("main"), "sync-conflict", event.clone()) {
        Ok(()) => {}
        Err(e) => {
            tracing::error!(error = %e, "emit_to webview_window(main) failed");
            if let Err(e2) = handle.emit("sync-conflict", event) {
                tracing::error!(error = %e2, "emit global sync-conflict failed");
            }
        }
    }
}

/// Bundles the mutable session-cursor state (`session_id`/`offset`) that
/// `restart_session` needs to overwrite, so the function's parameter count
/// stays within clippy's `too_many_arguments` threshold.
struct SessionCursor<'a> {
    session_id: &'a mut String,
    offset: &'a mut u64,
}

/// Starts a brand-new upload session (with retry), rewinds `file` to the
/// start, and persists the fresh checkpoint (including the file-identity
/// fields — `file_identity` is `(file_len, file_mtime)` — so a later resume
/// attempt can validate against them). Used when the in-progress session is
/// found to be invalid/expired partway through the upload.
fn restart_session(
    state: &AppState,
    job_id: i64,
    client: &Client,
    token: &str,
    file: &mut File,
    cursor: &mut SessionCursor,
    file_identity: (u64, i64),
) -> AppResult<()> {
    let (file_len, file_mtime) = file_identity;
    let sid = retry_transient("upload_session/start", 4, || {
        start_upload_session(client, token)
    })?;
    file.seek(SeekFrom::Start(0))
        .map_err(|e| AppError::Io(format!("failed seeking to restart upload session: {e}")))?;
    *cursor.session_id = sid;
    *cursor.offset = 0;
    state
        .db
        .save_upload_checkpoint(job_id, cursor.session_id, 0, file_len, file_mtime)?;
    Ok(())
}

/// Uploads `file` (of `len` bytes) to `dropbox_path` via the Dropbox chunked
/// upload-session API, reading and sending at most `UPLOAD_CHUNK_SIZE` bytes
/// at a time so peak memory stays bounded regardless of file size.
///
/// The upload is a point-in-time snapshot of `len` bytes: `len` is captured
/// once by the caller. If the file is truncated concurrently, EOF is reached
/// before `offset` hits `len` and the session is finished with whatever bytes
/// were read (never a busy-loop). Concurrent growth beyond `len` is ignored.
///
/// Resumable across app restarts: the session id, byte offset, and file
/// identity (`len` + mtime) are checkpointed in the DB
/// (`sync_jobs.upload_session_id`/`upload_session_offset`/
/// `upload_session_file_len`/`upload_session_file_mtime`) after every
/// successful chunk, keyed by `job_id`. On entry, an existing checkpoint is
/// resumed from ONLY when its stored file length and mtime both match the
/// file currently being uploaded (in addition to the offset fitting within
/// `len`) — this guards against silently appending new bytes onto a stale
/// session if the local file was modified between attempts, which would
/// otherwise commit a corrupt object. Any mismatch starts a brand-new
/// session at offset 0 instead. Each `append_v2`/`finish` call is itself
/// wrapped in `retry_transient` for transient network/rate-limit/server
/// errors; if the session is found to be invalid/expired, it is abandoned
/// and replaced with a fresh one (bounded to one restart per call).
fn upload_via_session(
    state: &AppState,
    token: &str,
    mut file: File,
    len: u64,
    dropbox_path: &str,
    job_id: i64,
) -> AppResult<()> {
    let client = &state.http_client;

    let file_mtime = file
        .metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let (mut session_id, mut offset) = match state.db.get_upload_checkpoint(job_id)? {
        Some((sid, off, ck_len, ck_mtime)) if off <= len && ck_len == len && ck_mtime == file_mtime => {
            file.seek(SeekFrom::Start(off))
                .map_err(|e| AppError::Io(format!("failed seeking to resume upload: {e}")))?;
            (sid, off)
        }
        _ => {
            let sid = retry_transient("upload_session/start", 4, || {
                start_upload_session(client, token)
            })?;
            state
                .db
                .save_upload_checkpoint(job_id, &sid, 0, len, file_mtime)?;
            (sid, 0)
        }
    };

    let mut buf = vec![0u8; UPLOAD_CHUNK_SIZE];
    let mut restarted = false;

    loop {
        let mut n = 0usize;
        while n < buf.len() {
            let read = file
                .read(&mut buf[n..])
                .map_err(|e| AppError::Io(format!("failed reading local file for upload: {e}")))?;
            if read == 0 {
                break;
            }
            n += read;
        }

        // Premature EOF (file shrank since `len` was read): finishing here
        // avoids an infinite loop of empty `append_v2` calls when `offset < len`.
        // When `is_final_chunk` is already true this is a no-op distinction.
        if n == 0 && !is_final_chunk(offset, n, len) {
            let chunk_offset = offset;
            let result = retry_transient("upload_session/finish", 4, || {
                finish_upload_session(client, token, &session_id, chunk_offset, dropbox_path, &[])
            });
            match result {
                Ok(()) => {
                    state.db.clear_upload_checkpoint(job_id)?;
                    emit_upload_progress(dropbox_path, offset, len);
                    break;
                }
                Err(e) if !restarted && e.is_session_invalid() => {
                    restarted = true;
                    restart_session(
                        state,
                        job_id,
                        client,
                        token,
                        &mut file,
                        &mut SessionCursor {
                            session_id: &mut session_id,
                            offset: &mut offset,
                        },
                        (len, file_mtime),
                    )?;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }

        if is_final_chunk(offset, n, len) {
            let chunk_offset = offset;
            let result = retry_transient("upload_session/finish", 4, || {
                finish_upload_session(client, token, &session_id, chunk_offset, dropbox_path, &buf[..n])
            });
            match result {
                Ok(()) => {
                    offset += n as u64;
                    state.db.clear_upload_checkpoint(job_id)?;
                    emit_upload_progress(dropbox_path, offset, len);
                    break;
                }
                Err(e) if !restarted && e.is_session_invalid() => {
                    restarted = true;
                    restart_session(
                        state,
                        job_id,
                        client,
                        token,
                        &mut file,
                        &mut SessionCursor {
                            session_id: &mut session_id,
                            offset: &mut offset,
                        },
                        (len, file_mtime),
                    )?;
                    continue;
                }
                Err(e) => return Err(e),
            }
        } else {
            let chunk_offset = offset;
            let result = retry_transient("upload_session/append_v2", 4, || {
                append_upload_chunk(client, token, &session_id, chunk_offset, &buf[..n])
            });
            match result {
                Ok(()) => {
                    offset += n as u64;
                    state
                        .db
                        .save_upload_checkpoint(job_id, &session_id, offset, len, file_mtime)?;
                    emit_upload_progress(dropbox_path, offset, len);
                }
                Err(e) if !restarted && e.is_session_invalid() => {
                    restarted = true;
                    restart_session(
                        state,
                        job_id,
                        client,
                        token,
                        &mut file,
                        &mut SessionCursor {
                            session_id: &mut session_id,
                            offset: &mut offset,
                        },
                        (len, file_mtime),
                    )?;
                }
                Err(e) => return Err(e),
            }
        }
    }

    Ok(())
}

pub(crate) fn upload_local_file_internal(
    state: &AppState,
    relative: &str,
    job_id: i64,
) -> AppResult<()> {
    if relative.ends_with(".cloudsc") {
        return Ok(());
    }

    let folder = state
        .db
        .get_sync_folder()?
        .ok_or_else(|| AppError::Sync("sync folder not configured".to_string()))?;

    let local_path = safe_join(Path::new(&folder), relative)?;
    // Distinguish a genuine absence (NotFound) from a transient stat failure
    // (permission, Windows sharing-violation, an editor's atomic save mid-rename).
    // Only a confirmed NotFound is treated as "gone"; anything else is returned as
    // an error so the job retries — we must NEVER conclude "deleted" from a racy
    // check (DBSYNC-55; mirrors `sync_pipeline::classify_path`).
    match std::fs::symlink_metadata(&local_path) {
        Ok(_) => {} // present — fall through to the upload
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // The source vanished before we could upload it — an editor temp file
            // (`~$doc.docx`) deleted on close, or a file removed right after it was
            // created. Failing the job surfaces as a stuck "Error" the user can't
            // clear, so no-op instead. Forget the row only when there is nothing on
            // Dropbox to reconcile (a temp/ignored path, or a file never uploaded).
            // If it HAD a remote copy, leave the index row and DO NOT enqueue a
            // delete here: the guarded deletion-detection path (classify_path +
            // re-stat) decides whether this was a real deletion to propagate — an
            // unguarded delete from here could wrongly wipe a live remote file
            // during an atomic save.
            tracing::info!(file_path = %relative, "upload cancelled: local file no longer exists");
            let had_remote = state.db.get_remote_file(relative)?.is_some();
            if !had_remote || is_ignored_local_path(relative) {
                state.db.remove_local_file(relative)?;
            }
            return Ok(());
        }
        Err(e) => {
            return Err(AppError::Io(format!(
                "failed to stat local file for upload: {e}"
            )));
        }
    }

    let token = get_access_token(state)?;

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
        .map_err(|e| AppError::Io(format!("failed opening local file for upload: {e}")))?;
    let len = file
        .metadata()
        .map_err(|e| AppError::Io(format!("failed reading local file metadata: {e}")))?
        .len();

    let dropbox_path = normalize_dropbox_path(relative)?;

    match choose_upload_strategy(len) {
        UploadStrategy::SingleShot => {
            let client = &state.http_client;
            let resp = client
                .post("https://content.dropboxapi.com/2/files/upload")
                .bearer_auth(token.as_str())
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
                // Whole-file single-shot upload (up to UPLOAD_SESSION_THRESHOLD_BYTES = 150 MiB in
                // one request): use 300s like the whole-file download, not the 120s used for bounded
                // upload-session chunks — 120s would abort a legitimate large single-shot upload on a
                // slow link. Files above the threshold take the chunked session path (safe at 120s/chunk).
                .timeout(Duration::from_secs(300))
                .send()
                .map_err(|e| {
                    AppError::Network(format!(
                        "upload request failed: {}",
                        describe_reqwest_error(&e)
                    ))
                })?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().unwrap_or_else(|_| "<unreadable body>".to_string());
                return Err(AppError::Dropbox {
                    status: status.as_u16(),
                    message: format!("upload for {dropbox_path}: {body}"),
                });
            }
        }
        UploadStrategy::Session => {
            upload_via_session(state, &token, file, len, &dropbox_path, job_id)?;
        }
    }

    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DeleteOutcome {
    Deleted,
    AlreadyGoneOrConflict,
    Error,
}

/// Classifies a `files/delete_v2` response. `AlreadyGoneOrConflict` means
/// treat as a no-op and drop the job: the target is already gone
/// (`not_found`) OR a `parent_rev` precondition failed because the file was
/// restored/changed on the server (`path_write/conflict`). Anything else on a
/// failure status is a real `Error`.
pub(crate) fn classify_delete_response(status_success: bool, body: &str) -> DeleteOutcome {
    if status_success {
        return DeleteOutcome::Deleted;
    }
    if body.contains("path_lookup/not_found") || body.contains("path/not_found") {
        return DeleteOutcome::AlreadyGoneOrConflict;
    }
    // parent_rev mismatch: Dropbox has no dedicated DeleteError tag for it
    // (verified against files.stone). Best-available signal is the contiguous
    // `path_write/conflict` error_summary. Match defensively; confirm the exact
    // live body via manual QA before closing.
    if body.contains("path_write/conflict") {
        return DeleteOutcome::AlreadyGoneOrConflict;
    }
    DeleteOutcome::Error
}

/// Performs a live `files/delete_v2` call against the Dropbox API; manual-QA-only
/// for the network path (the response-classification logic is covered by the
/// pure `classify_delete_response` tests below).
pub(crate) fn delete_remote_file_internal(
    state: &AppState,
    relative: &str,
    parent_rev: Option<&str>,
) -> AppResult<()> {
    if relative.ends_with(".cloudsc") {
        return Ok(());
    }

    let token = get_access_token(state)?;
    let dropbox_path = normalize_dropbox_path(relative)?;
    let client = &state.http_client;
    let mut body = serde_json::json!({ "path": dropbox_path });
    if let Some(rev) = parent_rev {
        body["parent_rev"] = serde_json::json!(rev);
    }
    let resp = client
        .post("https://api.dropboxapi.com/2/files/delete_v2")
        .bearer_auth(token.as_str())
        .json(&body)
        .send()
        .map_err(|e| {
            AppError::Network(format!(
                "delete request failed: {}",
                describe_reqwest_error(&e)
            ))
        })?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_else(|_| "<unreadable body>".to_string());
        if matches!(
            classify_delete_response(false, &body),
            DeleteOutcome::AlreadyGoneOrConflict
        ) {
            tracing::info!(rel = %relative, "remote delete skipped: already gone or rev-conflict (restored on server) — dropping job");
            return Ok(());
        }
        return Err(AppError::Dropbox {
            status: status.as_u16(),
            message: format!("delete for {dropbox_path}: {body}"),
        });
    }

    // DBSYNC-66: the delete genuinely succeeded on Dropbox (a 200 — NOT the
    // skipped/rev-conflict no-op above, which returns early and leaves the row
    // intact because the file is back on the server). Clear the remote-index row
    // for this path AND its whole subtree now so the materialization sweep does
    // not re-create placeholders for the just-deleted content before longpoll
    // catches up — the race that forced users to delete a folder twice. The
    // subtree clear matters for a coalesced folder delete: `delete_v2` on a
    // folder is recursive, so its descendants' remote rows must go too.
    state.db.remove_remote_subtree(relative)?;
    Ok(())
}

/// Removes a local file that was deleted on the remote (remote-wins deletion).
/// Cleans both index tables so the file is not re-detected on the next tick.
pub(crate) fn delete_local_file_internal(state: &AppState, relative: &str) -> AppResult<()> {
    let folder = state
        .db
        .get_sync_folder()?
        .ok_or_else(|| AppError::Sync("sync folder not configured".to_string()))?;

    let local_path = safe_join(Path::new(&folder), relative)?;
    if local_path.exists() {
        fs::remove_file(&local_path)
            .map_err(|e| AppError::Io(format!("failed to delete local file {relative}: {e}")))?;
    }

    state.db.remove_local_file(relative)?;
    state.db.remove_remote_file(relative)?;
    Ok(())
}

pub(crate) fn download_remote_file_internal(state: &AppState, path_display: &str) -> AppResult<()> {
    let token = get_access_token(state)?;
    let folder = state
        .db
        .get_sync_folder()?
        .ok_or_else(|| AppError::Sync("sync folder not configured".to_string()))?;

    let relative = path_display.trim_start_matches('/').to_string();
    let include_prefixes = parse_prefix_csv(state.db.get_include_prefixes_csv()?);
    let exclude_prefixes = parse_prefix_csv(state.db.get_exclude_prefixes_csv()?);
    if !is_path_allowed(&relative, &include_prefixes, &exclude_prefixes) {
        return Ok(());
    }

    // Refuse a remote path that would escape the sync root before writing.
    let target = safe_join(Path::new(&folder), &relative)?;

    // Remote-wins conflict guard: preserve unsynced local edits before the atomic
    // overwrite below. (There is an inherent, benign TOCTOU window — an edit made
    // during the subsequent network fetch is not captured here, but the next scan
    // tick re-detects it as a local change. A `None` baseline, only possible for
    // the manual/hydration call sites, is intentionally NOT treated as a conflict:
    // with no last-synced hash there is nothing to prove a local modification.)
    if target.exists() {
        let (on_disk_hash, _size, _mtime) = hash_file(&target)?;
        let baseline_hash = state.db.get_local_file(&relative)?.map(|row| row.hash);

        if download_would_conflict(true, &on_disk_hash, baseline_hash.as_deref())
            && !conflict_copy_with_content_exists(&target, &on_disk_hash)?
        {
            let conflicted_path = create_conflicted_copy(&target)?;
            let conflicted_rel = relpath_under(Path::new(&folder), &conflicted_path)?;
            let reason = format!(
                "remote download would overwrite unsynced local changes; local copy preserved as {conflicted_rel}"
            );
            state
                .db
                .add_conflict(&relative, &relative, &reason, Some(&conflicted_rel), false)?;
            if let Ok(mut engine) = state.sync_engine.lock() {
                engine.record_conflict();
            }
            tracing::warn!(
                path = %relative,
                conflicted_copy = %conflicted_rel,
                "sync conflict: local copy preserved before remote overwrite"
            );
            emit_sync_conflict(&relative, &conflicted_rel, &reason);
            crate::sharing::notify_conflict(&relative);
        }
    }

    fetch_and_write_file(&state.http_client, &token, path_display, &target)?;

    let (hash, size_bytes, modified_ts) = hash_file(&target)?;
    state
        .db
        .upsert_local_file(&relative, &hash, size_bytes, modified_ts)?;

    // DBSYNC-45: record remote provenance for the (re-)hydrated file so that a
    // later remote deletion propagates as a local delete. Without this row the
    // deletion branch in `refresh_remote_index` sees `prev_remote = None` and
    // can't tell "remotely deleted" from "never uploaded", so it does nothing.
    // The downloaded content hash IS the Dropbox content_hash (DBSYNC-9), which
    // is exactly what the deletion / should-download checks compare, so the row
    // is correct even if the best-effort metadata fetch (for rev/mtime) fails.
    let (rev, remote_mtime) = match crate::remote_index::fetch_remote_file_metadata(state, &relative)
    {
        Ok(Some(meta)) => (meta.rev, meta.modified_ts),
        _ => (String::new(), modified_ts),
    };
    if let Err(e) = state.db.upsert_remote_file(&relative, &hash, &rev, remote_mtime) {
        tracing::warn!(file_path = %relative, error = %e, "failed recording remote provenance after hydrate");
    }
    Ok(())
}

pub(crate) fn hydrate_remote_folder_internal(
    state: &AppState,
    folder_path_display: &str,
) -> AppResult<usize> {
    let token = get_access_token(state)?;
    let folder = state
        .db
        .get_sync_folder()?
        .ok_or_else(|| AppError::Sync("sync folder not configured".to_string()))?;

    let include_prefixes = parse_prefix_csv(state.db.get_include_prefixes_csv()?);
    let exclude_prefixes = parse_prefix_csv(state.db.get_exclude_prefixes_csv()?);

    fs::create_dir_all(&folder).map_err(|e| AppError::Io(format!("failed to create sync folder: {e}")))?;

    let client = &state.http_client;
    let mut downloaded = 0usize;

    let response = client
        .post("https://api.dropboxapi.com/2/files/list_folder")
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({
            "path": folder_path_display,
            "recursive": true,
            "include_deleted": false
        }))
        .send()
        .map_err(|e| {
            AppError::Network(format!(
                "list_folder request failed: {}",
                describe_reqwest_error(&e)
            ))
        })?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_else(|_| "<unreadable body>".to_string());
        return Err(AppError::Dropbox {
            status: status.as_u16(),
            message: format!("list_folder for {folder_path_display}: {body}"),
        });
    }

    let mut entries_resp: DropboxListFolderResponse = response
        .json()
        .map_err(|e| AppError::Other(format!("list_folder parse failed: {e}")))?;

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

            // Skip (don't abort the whole sweep on) a single traversing entry.
            let target = match safe_join(Path::new(&folder), &relative) {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(path = %relative, error = %e, "skipping unsafe remote path");
                    continue;
                }
            };
            fetch_and_write_file(client, &token, path_display, &target)?;

            let (hash, size_bytes, modified_ts) = hash_file(&target)?;
            state.db.upsert_local_file(&relative, &hash, size_bytes, modified_ts)?;
            downloaded += 1;
        }

        if !entries_resp.has_more {
            break;
        }

        entries_resp = client
            .post("https://api.dropboxapi.com/2/files/list_folder/continue")
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({ "cursor": entries_resp.cursor.clone() }))
            .send()
            .map_err(|e| {
                AppError::Network(format!(
                    "list_folder/continue request failed: {}",
                    describe_reqwest_error(&e)
                ))
            })?
            .error_for_status()
            .map_err(|e| AppError::Dropbox {
                status: e.status().map(|s| s.as_u16()).unwrap_or(0),
                message: format!("list_folder/continue: {e}"),
            })?
            .json()
            .map_err(|e| AppError::Other(format!("list_folder/continue parse failed: {e}")))?;
    }

    Ok(downloaded)
}

#[allow(dead_code)]
pub(crate) fn pull_remote_snapshot_internal(state: &AppState) -> AppResult<usize> {
    let token = match get_access_token(state) {
        Ok(token) => token,
        Err(_) => return Ok(0),
    };
    let folder = state
        .db
        .get_sync_folder()?
        .ok_or_else(|| AppError::Sync("sync folder not configured".to_string()))?;
    fs::create_dir_all(&folder).map_err(|e| AppError::Io(format!("failed to create sync folder: {e}")))?;

    let client = &state.http_client;
    let mut downloaded = 0usize;
    let known_map: HashMap<String, FileIndexRow> = state
        .db
        .list_local_files()?
        .into_iter()
        .map(|f| (f.relative_path.clone(), f))
        .collect();

    let list_folder_resp = client
        .post("https://api.dropboxapi.com/2/files/list_folder")
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({
            "path": "",
            "recursive": true,
            "include_deleted": false
        }))
        .send()
        .map_err(|e| {
            AppError::Network(format!(
                "list_folder request failed: {}",
                describe_reqwest_error(&e)
            ))
        })?;
    if !list_folder_resp.status().is_success() {
        let status = list_folder_resp.status();
        let body = list_folder_resp.text().unwrap_or_else(|_| "<unreadable body>".to_string());
        return Err(AppError::Dropbox {
            status: status.as_u16(),
            message: format!("list_folder: {body}"),
        });
    }
    let mut response: DropboxListFolderResponse = list_folder_resp
        .json()
        .map_err(|e| AppError::Other(format!("list_folder parse failed: {e}")))?;

    loop {
        for entry in &response.entries {
            if entry.tag != "file" {
                continue;
            }
            let Some(path_display) = &entry.path_display else {
                continue;
            };

            let relative = path_display.trim_start_matches('/').to_string();
            let target = match safe_join(Path::new(&folder), &relative) {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(path = %relative, error = %e, "skipping unsafe remote path");
                    continue;
                }
            };

            if known_map.contains_key(&relative) && target.exists() {
                continue;
            }

            fetch_and_write_file(client, &token, path_display, &target)?;
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
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({ "cursor": response.cursor.clone() }))
            .send()
            .map_err(|e| {
                AppError::Network(format!(
                    "list_folder/continue request failed: {}",
                    describe_reqwest_error(&e)
                ))
            })?;
        if !continue_resp.status().is_success() {
            let status = continue_resp.status();
            let body = continue_resp.text().unwrap_or_else(|_| "<unreadable body>".to_string());
            return Err(AppError::Dropbox {
                status: status.as_u16(),
                message: format!("list_folder/continue: {body}"),
            });
        }
        response = continue_resp
            .json()
            .map_err(|e| AppError::Other(format!("list_folder/continue parse failed: {e}")))?;
    }

    Ok(downloaded)
}

#[cfg(test)]
mod tests {
    use super::{
        atomic_replace, choose_upload_strategy, classify_delete_response,
        conflict_copy_with_content_exists, download_would_conflict, emit_sync_conflict,
        emit_upload_progress, is_final_chunk, retry_transient, DeleteOutcome, UploadStrategy,
        UPLOAD_CHUNK_SIZE, UPLOAD_SESSION_THRESHOLD_BYTES,
    };
    use crate::error::{AppError, AppResult};
    use crate::path_util::hash_file;
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    // `delete_remote_file_internal` performs a live HTTP call against the
    // Dropbox API and is manual-QA-only; this module covers the pure
    // `classify_delete_response` response-classification helper instead.

    #[test]
    fn classify_delete_response_success_is_deleted() {
        assert_eq!(
            classify_delete_response(true, ""),
            DeleteOutcome::Deleted
        );
    }

    #[test]
    fn classify_delete_response_not_found_variants_are_noop() {
        assert_eq!(
            classify_delete_response(
                false,
                r#"{"error_summary":"path_lookup/not_found/..","error":{".tag":"path_lookup"}}"#
            ),
            DeleteOutcome::AlreadyGoneOrConflict
        );
        assert_eq!(
            classify_delete_response(
                false,
                r#"{"error_summary":"path/not_found/..","error":{".tag":"path"}}"#
            ),
            DeleteOutcome::AlreadyGoneOrConflict
        );
    }

    #[test]
    fn classify_delete_response_path_write_conflict_is_noop() {
        // This body shape is reconstructed from the `files.stone` API spec
        // (WriteError::conflict / WriteConflictError::file), not an observed
        // live response — the exact `parent_rev`-mismatch body must be
        // confirmed via manual QA (see PR description) before this match is
        // considered final.
        let body = r#"{"error_summary":"path_write/conflict/..","error":{".tag":"path_write","path_write":{".tag":"conflict","conflict":{".tag":"file"}}}}"#;
        assert_eq!(
            classify_delete_response(false, body),
            DeleteOutcome::AlreadyGoneOrConflict
        );
    }

    #[test]
    fn classify_delete_response_unrelated_error_is_error() {
        let no_write_permission = r#"{"error_summary":"path_write/no_write_permission/..","error":{".tag":"path_write","path_write":{".tag":"no_write_permission"}}}"#;
        assert_eq!(
            classify_delete_response(false, no_write_permission),
            DeleteOutcome::Error
        );

        let insufficient_space = r#"{"error_summary":"path_write/insufficient_space/..","error":{".tag":"path_write","path_write":{".tag":"insufficient_space"}}}"#;
        assert_eq!(
            classify_delete_response(false, insufficient_space),
            DeleteOutcome::Error
        );
    }

    #[test]
    fn classify_delete_response_malformed_or_empty_body_is_error() {
        assert_eq!(classify_delete_response(false, ""), DeleteOutcome::Error);
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

    #[test]
    fn emit_sync_conflict_is_a_no_op_without_app_handle() {
        // `state::APP_HANDLE` is unset in this offline test binary (`setup()` never
        // runs), so this must not panic and must not attempt to emit anything.
        emit_sync_conflict("a", "b", "c");
    }

    #[test]
    fn retry_transient_retries_transport_errors_then_succeeds() {
        // Only fails once (single ~2s backoff sleep) to keep the test fast while
        // still exercising the retry-then-succeed path.
        let calls = AtomicUsize::new(0);
        let result: AppResult<&str> = retry_transient("test", 4, || {
            let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
            if n < 2 {
                Err(AppError::Network(format!(
                    "upload_session/append_v2 request failed: simulated transport error {n}"
                )))
            } else {
                Ok("done")
            }
        });

        assert!(matches!(result, Ok("done")));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn retry_transient_does_not_retry_status_errors() {
        let calls = AtomicUsize::new(0);
        let result: AppResult<()> = retry_transient("test", 4, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(AppError::Dropbox {
                status: 400,
                message: "upload_session/append_v2: Bad Request; body: {}".to_string(),
            })
        });

        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn retry_transient_gives_up_after_max_attempts() {
        // max_attempts=2 keeps this to a single ~2s backoff sleep before giving up.
        let calls = AtomicUsize::new(0);
        let result: AppResult<()> = retry_transient("test", 2, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(AppError::Network(
                "upload_session/append_v2 request failed: still broken".to_string(),
            ))
        });

        assert!(matches!(result, Err(AppError::Network(_))));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn retry_transient_retries_rate_limit_status_errors() {
        // A 429 (rate limit) must be retried with backoff, unlike a plain 4xx.
        let calls = AtomicUsize::new(0);
        let result: AppResult<&str> = retry_transient("test", 2, || {
            let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
            if n < 2 {
                Err(AppError::Dropbox {
                    status: 429,
                    message: "upload_session/append_v2: Too Many Requests; body: {}".to_string(),
                })
            } else {
                Ok("done")
            }
        });

        assert!(matches!(result, Ok("done")));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn retry_transient_does_not_retry_plain_400() {
        let calls = AtomicUsize::new(0);
        let result: AppResult<()> = retry_transient("test", 4, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(AppError::Dropbox {
                status: 400,
                message: "upload_session/append_v2: Bad Request; body: {}".to_string(),
            })
        });

        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn download_would_conflict_false_when_target_absent() {
        assert!(!download_would_conflict(false, "on-disk-hash", None));
        assert!(!download_would_conflict(false, "on-disk-hash", Some("baseline-hash")));
    }

    #[test]
    fn download_would_conflict_false_when_baseline_none() {
        assert!(!download_would_conflict(true, "on-disk-hash", None));
    }

    #[test]
    fn download_would_conflict_false_when_hash_matches_baseline() {
        assert!(!download_would_conflict(true, "same-hash", Some("same-hash")));
    }

    #[test]
    fn download_would_conflict_true_when_hash_diverges() {
        assert!(download_would_conflict(true, "local-hash", Some("baseline-hash")));
    }

    #[test]
    fn conflict_copy_dedup_matches_by_content_not_existence() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("report.txt");
        std::fs::write(&target, b"local edit").expect("write target");
        let (on_disk_hash, _s, _m) = hash_file(&target).expect("hash target");

        // No conflicted-copy sibling yet.
        assert!(!conflict_copy_with_content_exists(&target, &on_disk_hash).unwrap());

        // A sibling with DIFFERENT content does not count as a dedup match.
        std::fs::write(
            dir.path().join("report (conflicted copy 20260101000000).txt"),
            b"some other content",
        )
        .expect("write other copy");
        assert!(!conflict_copy_with_content_exists(&target, &on_disk_hash).unwrap());

        // A sibling holding the SAME content as the target dedupes (retry case).
        std::fs::write(
            dir.path().join("report (conflicted copy 20260101000005).txt"),
            b"local edit",
        )
        .expect("write matching copy");
        assert!(conflict_copy_with_content_exists(&target, &on_disk_hash).unwrap());
    }

    #[test]
    fn is_session_invalid_error_detects_known_markers() {
        assert!(AppError::Dropbox {
            status: 409,
            message: "upload_session/append_v2: {\"error\": {\".tag\": \"lookup_failed\"}}".to_string()
        }
        .is_session_invalid());
        assert!(AppError::Dropbox {
            status: 409,
            message: "upload_session/finish: {\"error_summary\": \"incorrect_offset\"}".to_string()
        }
        .is_session_invalid());
        assert!(AppError::Dropbox {
            status: 409,
            message: "upload_session/finish: {\"error_summary\": \"not_closed\"}".to_string()
        }
        .is_session_invalid());
        assert!(!AppError::Network(
            "upload_session/append_v2 request failed: connection reset".to_string()
        )
        .is_session_invalid());
        assert!(!AppError::Dropbox {
            status: 400,
            message: "upload_session/append_v2: Bad Request; body: {}".to_string()
        }
        .is_session_invalid());
        // A bare 409 that is actually write-lock contention (not a dead session)
        // must NOT be treated as session-invalid.
        assert!(!AppError::Dropbox {
            status: 409,
            message: "upload_session/finish: {\"error_summary\": \"too_many_write_operations/...\"}".to_string()
        }
        .is_session_invalid());
    }
}
