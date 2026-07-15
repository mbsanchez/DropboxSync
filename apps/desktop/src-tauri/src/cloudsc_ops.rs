use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use tauri::{Emitter, EventTarget};
use walkdir::WalkDir;

use crate::auth_session::get_access_token;
use crate::cloudsc::{
    cloudsc_target_path, read_cloudsc_placeholder_file, write_cloudsc_placeholder_file,
};
use crate::dropbox_transfer::download_remote_file_internal;
use crate::error::{AppError, AppResult};
use crate::models::{
    CloudscMeta, CloudscPlaceholderInfo, DropboxListFolderResponse,
};
use crate::path_util::{
    hash_file, is_path_allowed, normalize_dropbox_path, parse_prefix_csv, relpath_under, safe_join,
    should_ignore_local_path,
};
use crate::state::AppState;

pub(crate) fn index_remote_folder_children_as_cloudsc_placeholders_internal(
    state: &AppState,
    remote_folder_path_display: &str,
    local_dir: &Path,
) -> AppResult<usize> {
    let token = get_access_token(state)?;
    let include_prefixes = parse_prefix_csv(state.db.get_include_prefixes_csv()?);
    let exclude_prefixes = parse_prefix_csv(state.db.get_exclude_prefixes_csv()?);

    fs::create_dir_all(local_dir)
        .map_err(|e| AppError::Io(format!("failed creating local dir: {e}")))?;

    let client = &state.http_client;
    let mut created = 0usize;
    // DBSYNC-45: collect placeholder create/prune names to emit ONE
    // `placeholder-changed` event per index call (no per-file event flood on a
    // large initial sweep); the frontend logs them into the Activité feed.
    let mut created_names: Vec<String> = Vec::new();
    let mut removed_names: Vec<String> = Vec::new();

    // Page through the folder with cursor + has_more; a folder with more than one
    // page of entries (Dropbox returns up to ~2000 per page) would otherwise be
    // silently truncated to its first page.
    let mut entries_resp: DropboxListFolderResponse = {
        let response = client
            .post("https://api.dropboxapi.com/2/files/list_folder")
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({
                "path": remote_folder_path_display,
                "recursive": false,
                "include_deleted": false
            }))
            .send()
            .map_err(|e| AppError::Network(format!("list_folder request failed: {e}")))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().unwrap_or_else(|_| "<unreadable body>".to_string());
            // A local directory with no Dropbox counterpart (e.g. a local-only
            // folder pending upload) is not an error for this indexer — it simply
            // has nothing to placeholder. Don't spam the log or fail the sweep.
            if body.contains("path/not_found") {
                return Ok(0);
            }
            return Err(AppError::Dropbox {
                status: status.as_u16(),
                message: format!("list_folder for {remote_folder_path_display}: {body}"),
            });
        }
        response
            .json()
            .map_err(|e| AppError::Other(format!("list_folder parse failed: {e}")))?
    };

    // Names of every remote child in this folder (regardless of selective-sync
    // filtering), used below to prune placeholders whose remote target is gone.
    let mut remote_child_names: HashSet<String> = HashSet::new();

    loop {
        for entry in entries_resp.entries {
            let tag = entry.tag;
            let path_display = match entry.path_display {
                Some(p) => p,
                None => continue,
            };

            let child_name = path_display
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or(&path_display)
                .to_string();
            remote_child_names.insert(child_name.clone());

            let relative = path_display.trim_start_matches('/').to_string();
            if !is_path_allowed(&relative, &include_prefixes, &exclude_prefixes) {
                continue;
            }

            // `child_name` is the last segment of a remote path_display: refuse a
            // name that carries embedded separators / traversal before writing it
            // to local disk (skip the poisoned entry, don't abort the sweep).
            let placeholder_path = match safe_join(local_dir, &format!("{child_name}.cloudsc")) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(name = %child_name, error = %e, "skipping unsafe placeholder name");
                    continue;
                }
            };
            let target_path = cloudsc_target_path(&placeholder_path);
            // Skip if already represented locally: as a placeholder, or as a real
            // file/dir (a hydrated folder is a real dir, so we never shadow it with
            // a `<name>.cloudsc`).
            if placeholder_path.exists() {
                continue;
            }
            if target_path.exists() {
                continue;
            }

            let meta = CloudscMeta {
                version: 1,
                tag,
                remote_path_display: path_display,
            };
            write_cloudsc_placeholder_file(&placeholder_path, &meta)?;
            created += 1;
            created_names.push(child_name.clone());
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
            let body = response.text().unwrap_or_else(|_| "<unreadable body>".to_string());
            return Err(AppError::Dropbox {
                status: status.as_u16(),
                message: format!("list_folder/continue for {remote_folder_path_display}: {body}"),
            });
        }
        entries_resp = response
            .json()
            .map_err(|e| AppError::Other(format!("list_folder/continue parse failed: {e}")))?;
    }

    // Remote-wins for placeholders: a `<X>.cloudsc` whose remote target `X` no
    // longer appears in this folder's listing was deleted remotely, so prune the
    // stale placeholder. This runs ONLY after a complete, successful pagination
    // (any list error returns earlier), so a failed/partial listing never deletes
    // placeholders. Hydrated content (real files/dirs) is untouched — only
    // `.cloudsc` files are considered.
    for dir_entry in
        fs::read_dir(local_dir).map_err(|e| AppError::Io(format!("failed reading local dir: {e}")))?
    {
        let dir_entry = match dir_entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name = dir_entry.file_name().to_string_lossy().to_string();
        if let Some(target) = name.strip_suffix(".cloudsc") {
            if !remote_child_names.contains(target) && fs::remove_file(dir_entry.path()).is_ok() {
                removed_names.push(target.to_string());
            }
        }
    }

    emit_placeholder_changed(&created_names, &removed_names);
    Ok(created)
}

/// Emits a single `placeholder-changed` event summarising the `.cloudsc`
/// placeholders created/removed during one index sweep, so the flyout Activité
/// feed can show them (DBSYNC-45). No-ops when nothing changed or no `AppHandle`
/// is set. Emitting a summary (not per-file) avoids flooding the event bus on a
/// large initial index.
fn emit_placeholder_changed(created: &[String], removed: &[String]) {
    if created.is_empty() && removed.is_empty() {
        return;
    }
    let Some(handle) = crate::state::APP_HANDLE.get() else {
        return;
    };
    let payload = serde_json::json!({ "created": created, "removed": removed });
    if handle
        .emit_to(
            EventTarget::webview_window("main"),
            "placeholder-changed",
            payload.clone(),
        )
        .is_err()
    {
        let _ = handle.emit("placeholder-changed", payload);
    }
}

/// Discovers new remote content in every folder that already exists locally as a
/// real directory (the sync root plus any hydrated folder) and creates a
/// `.cloudsc` placeholder for each remote child — file OR folder — that is not
/// already represented locally.
///
/// This keeps the "folder = single `.cloudsc` placeholder" model: we only index
/// inside real directories, never descending into unopened folder placeholders
/// (they are files, so `WalkDir` doesn't recurse into them, and a hydrated folder
/// is a real dir that `target_path.exists()` already protects from being shadowed).
/// A brand-new remote folder therefore surfaces as `<name>.cloudsc` under its
/// (real) parent, exactly like a new remote file.
pub(crate) fn index_materialized_folders_as_cloudsc_placeholders_internal(
    state: &AppState,
) -> AppResult<usize> {
    let sync_folder_str = state
        .db
        .get_sync_folder()?
        .ok_or_else(|| AppError::Sync("sync folder not configured".to_string()))?;
    let sync_folder = PathBuf::from(&sync_folder_str);
    fs::create_dir_all(&sync_folder)
        .map_err(|e| AppError::Io(format!("failed creating sync folder: {e}")))?;

    let mut created = 0usize;
    for entry in WalkDir::new(&sync_folder).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_dir() {
            continue;
        }
        let dir = entry.path();
        let rel = relpath_under(&sync_folder, dir)?; // "" for the sync root itself
        // "" for root, "/Cocina", ...; a malformed path skips this folder only.
        let remote_path = match normalize_dropbox_path(&rel) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(rel = %rel, error = %e, "skipping folder with unsafe path");
                continue;
            }
        };
        match index_remote_folder_children_as_cloudsc_placeholders_internal(state, &remote_path, dir)
        {
            Ok(n) => created += n,
            // One unreadable folder must not abort the whole sweep.
            Err(e) => tracing::error!(remote_path = %remote_path, error = %e, "index remote folder failed"),
        }
    }

    Ok(created)
}

pub(crate) fn list_cloudsc_placeholders(
    state: &AppState,
    limit: usize,
) -> AppResult<Vec<CloudscPlaceholderInfo>> {
    let sync_folder_str = state
        .db
        .get_sync_folder()?
        .ok_or_else(|| AppError::Sync("sync folder not configured".to_string()))?;
    let sync_folder = PathBuf::from(sync_folder_str);

    let mut out: Vec<CloudscPlaceholderInfo> = Vec::new();
    for entry in WalkDir::new(&sync_folder).min_depth(1) {
        let entry = entry.map_err(|e| AppError::Io(format!("walk dir error: {e}")))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let file_name = entry.file_name().to_string_lossy();
        if !file_name.ends_with(".cloudsc") {
            continue;
        }
        if out.len() >= limit {
            break;
        }
        let placeholder_path = entry.path();
        let meta = read_cloudsc_placeholder_file(placeholder_path)?;
        let local_rel = relpath_under(&sync_folder, placeholder_path)?;
        out.push(CloudscPlaceholderInfo {
            local_path_display: local_rel,
            tag: meta.tag,
            remote_path_display: meta.remote_path_display,
        });
    }
    Ok(out)
}

pub(crate) fn hydrate_cloudsc_placeholder_internal(
    state: &AppState,
    placeholder_local_rel_path: &str,
) -> AppResult<usize> {
    let sync_folder = state
        .db
        .get_sync_folder()?
        .ok_or_else(|| AppError::Sync("sync folder not configured".to_string()))?;

    // `placeholder_local_rel_path` is frontend/IPC-supplied — validate it can't
    // escape the sync root before touching the filesystem.
    let placeholder_path = safe_join(Path::new(&sync_folder), placeholder_local_rel_path)?;
    if !placeholder_path.exists() {
        return Err(AppError::Sync(format!(
            "placeholder not found: {placeholder_local_rel_path}"
        )));
    }

    let meta = read_cloudsc_placeholder_file(&placeholder_path)?;
    let target_path = cloudsc_target_path(&placeholder_path);

    if meta.tag == "file" {
        download_remote_file_internal(state, &meta.remote_path_display)?;
        fs::remove_file(&placeholder_path)
            .map_err(|e| AppError::Io(format!("failed removing placeholder: {e}")))?;
        Ok(1)
    } else {
        fs::create_dir_all(&target_path)
            .map_err(|e| AppError::Io(format!("failed creating hydrated folder: {e}")))?;
        fs::remove_file(&placeholder_path)
            .map_err(|e| AppError::Io(format!("failed removing folder placeholder: {e}")))?;

        index_remote_folder_children_as_cloudsc_placeholders_internal(
            state,
            &meta.remote_path_display,
            &target_path,
        )
    }
}

/// Free up space (DBSYNC-33): replace a fully-synced local file — or every synced
/// file under a folder — with a `.cloudsc` placeholder and delete the local copy.
///
/// The local delete must NEVER be propagated as a Dropbox delete. Two independent
/// guards ensure that: the `local_file_index` row is removed and the `.cloudsc`
/// is written BEFORE the file is deleted, so (a) the scan/watcher never sees a
/// *tracked-file* deletion and (b) the DBSYNC-45 `placeholder_exists` guard
/// suppresses any remote delete. A file with unsynced local changes is refused
/// (single file) or skipped (folder) so no data is lost.
pub(crate) fn dehydrate_path_internal(state: &AppState, rel: &str) -> AppResult<usize> {
    let sync_folder = state
        .db
        .get_sync_folder()?
        .ok_or_else(|| AppError::Sync("sync folder not configured".to_string()))?;
    let root = PathBuf::from(&sync_folder);
    let abs = safe_join(&root, rel)?;

    let mut created: Vec<String> = Vec::new();
    if abs.is_dir() {
        // Collapse the whole folder into a single `<folder>.cloudsc` — the exact
        // inverse of folder hydration (which lazily re-expands one level). Rejects
        // the whole folder (touching nothing) if any file is unsynced; degrades to
        // per-file placeholders if a file is locked (DBSYNC-54).
        let mut placeholders = dehydrate_folder_collapse(state, &root, rel)?;
        created.append(&mut placeholders);
    } else if abs.is_file() {
        // Already a native CfAPI dehydrated placeholder → cloud-only already; nothing
        // to free. Return before `dehydrate_one_file`, whose `is_file_fully_synced`
        // re-hash would OPEN it and trigger a recall/hydration (DBSYNC-59).
        #[cfg(windows)]
        if crate::path_util::is_dehydrated_placeholder(&abs) {
            return Ok(0);
        }
        if dehydrate_one_file(state, &root, rel)? {
            created.push(rel.to_string());
        } else {
            return Err(AppError::Sync(format!(
                "cannot free up space: '{rel}' is not fully synced"
            )));
        }
    } else {
        return Err(AppError::Sync(format!("path not found: {rel}")));
    }

    let count = created.len();
    if count > 0 {
        emit_placeholder_changed(&created, &[]);
        crate::overlay_state::refresh_overlay_state_internal(state);
        tracing::info!(count, "dehydrated (freed up space)");
    }
    Ok(count)
}

/// Is `rel` a fully-synced tracked file? i.e. present in the local index, with a
/// known remote copy, AND the ACTUAL on-disk bytes hash equal to the last-synced
/// remote hash. The on-disk re-hash is the AUTHORITATIVE freshness check (never
/// trust the possibly-stale index row): a file edited but not yet re-indexed by
/// the watcher/scan must not be treated as synced, or dehydrating it would
/// permanently lose the edit (index-freshness TOCTOU). Returns `Err` only if the
/// file can't be read.
fn is_file_fully_synced(state: &AppState, root: &Path, rel: &str) -> AppResult<bool> {
    if state.db.get_local_file(rel)?.is_none() {
        return Ok(false);
    }
    let Some(remote) = state.db.get_remote_file(rel)? else {
        return Ok(false);
    };
    let abs = safe_join(root, rel)?;
    let (on_disk_hash, _size, _mtime) = hash_file(&abs)?;
    Ok(on_disk_hash == remote.content_hash)
}

/// `<abs>.cloudsc` — the placeholder path for a file/folder at `abs`.
fn with_cloudsc_suffix(abs: &Path) -> PathBuf {
    let mut s = abs.to_path_buf().into_os_string();
    s.push(".cloudsc");
    PathBuf::from(s)
}

/// Write the `<abs>.cloudsc` placeholder for a synced file `rel`, recreating its
/// parent directory if a collapse attempt removed it (degrade path).
fn write_file_placeholder(root: &Path, rel: &str) -> AppResult<()> {
    let path = with_cloudsc_suffix(&safe_join(root, rel)?);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| AppError::Io(format!("failed creating dir for placeholder: {e}")))?;
    }
    let meta = CloudscMeta {
        version: 1,
        tag: "file".to_string(),
        remote_path_display: normalize_dropbox_path(rel)?,
    };
    write_cloudsc_placeholder_file(&path, &meta)
}

/// Dehydrate a single file iff it is fully synced. Returns `Ok(true)` if
/// dehydrated, `Ok(false)` if not synced (skipped, untouched). The ORDER of the
/// mutations is load-bearing. If the OS refuses to delete the file (locked / in
/// use), the placeholder + untrack are rolled back so the file is left fully
/// synced (never an orphan placeholder over a still-present file).
fn dehydrate_one_file(state: &AppState, root: &Path, rel: &str) -> AppResult<bool> {
    if !is_file_fully_synced(state, root, rel)? {
        return Ok(false); // unsynced (or index-stale) local edit — keep the file
    }

    // DBSYNC-59 Slice 2: when native CfAPI placeholders are active (packaged Windows
    // with a live hydration connection), free space by replacing the file with a real
    // dehydrated placeholder at the SAME path — it gets the native cloud icon and
    // hydrates on open — instead of a `.cloudsc` sidecar. Elsewhere, fall back to
    // `.cloudsc`.
    #[cfg(windows)]
    {
        if crate::cloud_filter::placeholders_active(&root.to_string_lossy()) {
            return dehydrate_one_file_cfapi(state, root, rel);
        }
    }

    let abs = safe_join(root, rel)?;
    let row = state
        .db
        .get_local_file(rel)?
        .ok_or_else(|| AppError::Sync(format!("missing index row for {rel}")))?;
    let placeholder_path = with_cloudsc_suffix(&abs);

    // 1) Write the placeholder + 2) untrack BEFORE deleting, so a concurrent
    //    scan/watcher tick can't remote-delete this path.
    write_file_placeholder(root, rel)?;
    state.db.remove_local_file(rel)?;
    // 3) Delete the local file; on failure, restore the fully-synced state.
    if let Err(e) = fs::remove_file(&abs) {
        let _ = fs::remove_file(&placeholder_path);
        state
            .db
            .upsert_local_file(rel, &row.hash, row.size_bytes, row.modified_ts)?;
        return Err(AppError::Io(format!(
            "failed removing local file for dehydrate: {e}"
        )));
    }
    Ok(true)
}

/// Free a single fully-synced file by replacing it with a native CfAPI dehydrated
/// placeholder at the SAME path (DBSYNC-59 Slice 2). The caller has already verified
/// `rel` is fully synced, so the bytes are safely on Dropbox.
///
/// Ordering is load-bearing for the DBSYNC-33/45 "a dehydration is never a Dropbox
/// delete" guarantee. The index row is UNTRACKED before the file is removed, so a
/// concurrent scan/watcher tick that catches the brief remove→create window sees no
/// *tracked-file* deletion (the deletion loop iterates the index). The placeholder
/// then lands at the same path — a real file that reaches `seen_paths` and is skipped
/// by [`is_dehydrated_placeholder`], so it is never re-hashed, re-uploaded, or
/// delete-detected. Finally the row is RE-TRACKED with the original sha256 so a later
/// native hydration-on-open (which does NOT pass through our index-updating download)
/// is a clean no-op: the hydrated bytes equal the remote, so no re-upload.
#[cfg(windows)]
fn dehydrate_one_file_cfapi(state: &AppState, root: &Path, rel: &str) -> AppResult<bool> {
    let abs = safe_join(root, rel)?;
    let row = state
        .db
        .get_local_file(rel)?
        .ok_or_else(|| AppError::Sync(format!("missing index row for {rel}")))?;
    let remote_path = normalize_dropbox_path(rel)?;
    let (Some(parent), Some(name)) = (abs.parent(), abs.file_name()) else {
        return Err(AppError::Sync(format!("invalid path for dehydrate: {rel}")));
    };
    let name = name.to_string_lossy().to_string();

    // 1) Untrack BEFORE deleting so the remove→create window can't remote-delete.
    state.db.remove_local_file(rel)?;
    // 2) Free the path (CfCreatePlaceholders requires the target not to exist).
    if let Err(e) = fs::remove_file(&abs) {
        // Couldn't free it (locked / in use) — restore the fully-synced state.
        state
            .db
            .upsert_local_file(rel, &row.hash, row.size_bytes, row.modified_ts)?;
        return Err(AppError::Io(format!(
            "failed removing local file for dehydrate: {e}"
        )));
    }
    // 3) Create the dehydrated placeholder.
    if crate::cloud_filter::create_dehydrated_placeholder(parent, &name, &remote_path, row.size_bytes)
    {
        // 4) Re-track with the ORIGINAL sha256 → native hydration is a clean no-op.
        state
            .db
            .upsert_local_file(rel, &row.hash, row.size_bytes, row.modified_ts)?;
        Ok(true)
    } else {
        // Placeholder creation failed. The bytes are safe on Dropbox (fully synced);
        // leave the path untracked so the next remote sweep re-represents it as
        // cloud-only. Never re-track a now-absent file.
        Err(AppError::Io(format!(
            "failed creating dehydrated placeholder for {rel}"
        )))
    }
}

/// Collapse a fully-synced folder into a single `<folder>.cloudsc` (`tag:
/// "folder"`) — the inverse of folder hydration (DBSYNC-54). Returns the rel(s)
/// that became placeholders.
///
/// Rejects the WHOLE folder (touching nothing) if it contains any file that isn't
/// safe to delete: `.cloudsc` placeholders and ignored/ephemeral files are fine,
/// but any unsynced/untracked user file aborts so no edit is ever lost.
///
/// Robustness (per PO): a file the OS won't delete (locked / in use) is left fully
/// synced — its untrack is rolled back — and the operation degrades to per-file
/// dehydration of the deletable siblings (folder kept, NO folder placeholder).
///
/// The local delete must NEVER propagate to Dropbox: every file is untracked
/// before it is deleted, and the folder rows are untracked before the dirs are
/// removed (a leftover subfolder row would make `process_known_folder_deletion`
/// fire a RECURSIVE remote delete).
fn dehydrate_folder_collapse(
    state: &AppState,
    root: &Path,
    folder_rel: &str,
) -> AppResult<Vec<String>> {
    // Never collapse the sync root itself — that would delete the whole sync folder.
    if folder_rel.is_empty() {
        return Err(AppError::Sync(
            "cannot free up space on the sync root itself".to_string(),
        ));
    }
    let abs = safe_join(root, folder_rel)?;

    // Phase 1 — enumerate the subtree, FAIL CLOSED. Any walk error aborts before we
    // mutate anything: proceeding on an incomplete enumeration could later delete a
    // file we never untracked (→ a remote delete) or lose an unsynced file we never
    // got to reject. Collect the verified files, the existing `.cloudsc` children,
    // and the directories (to remove empty-only). Any unsynced/untracked user file
    // rejects the WHOLE folder.
    let mut files: Vec<(String, crate::storage::db::FileIndexRow)> = Vec::new();
    let mut cloudsc_children: Vec<PathBuf> = Vec::new();
    let mut dirs: Vec<(usize, PathBuf)> = Vec::new();
    for entry in WalkDir::new(&abs) {
        let entry = entry.map_err(|e| {
            AppError::Io(format!(
                "cannot free up space: could not fully read folder '{folder_rel}' ({e}); nothing was changed"
            ))
        })?;
        let ft = entry.file_type();
        if ft.is_dir() {
            dirs.push((entry.depth(), entry.path().to_path_buf()));
            continue;
        }
        if !ft.is_file() {
            continue; // symlink/other — left in place; it blocks the collapse (→ degrade)
        }
        let file_rel = match entry.path().strip_prefix(root) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        if file_rel.ends_with(".cloudsc") {
            cloudsc_children.push(entry.path().to_path_buf());
            continue;
        }
        if should_ignore_local_path(&file_rel) {
            continue; // ignored/ephemeral — not deleted; blocks the collapse (→ degrade)
        }
        if !is_file_fully_synced(state, root, &file_rel)? {
            return Err(AppError::Sync(format!(
                "cannot free up space: folder '{folder_rel}' has unsynced changes in '{file_rel}'; sync it first"
            )));
        }
        let row = state
            .db
            .get_local_file(&file_rel)?
            .ok_or_else(|| AppError::Sync(format!("missing index row for {file_rel}")))?;
        files.push((file_rel, row));
    }

    // Phase 2 — delete each verified file (untrack-before-delete + rollback on
    // failure). A file the OS won't delete is re-tracked and left fully synced.
    let mut deleted: Vec<String> = Vec::new();
    let mut any_failed = false;
    for (file_rel, row) in &files {
        // Re-verify immediately before deleting. Phase 1's full-subtree re-hash can
        // take a while on a large folder, and a file verified early may have been
        // edited since — never delete one that no longer matches remote (that edit
        // isn't uploaded yet). Keep it synced and degrade to per-file for the rest.
        if !is_file_fully_synced(state, root, file_rel)? {
            return degrade_to_per_file(root, &deleted);
        }
        state.db.remove_local_file(file_rel)?;
        match fs::remove_file(safe_join(root, file_rel)?) {
            Ok(()) => deleted.push(file_rel.clone()),
            Err(e) => {
                state
                    .db
                    .upsert_local_file(file_rel, &row.hash, row.size_bytes, row.modified_ts)?;
                any_failed = true;
                tracing::warn!(path = %file_rel, error = %e, "dehydrate: file locked, left synced");
            }
        }
    }
    // A locked file → don't attempt the collapse; degrade to per-file for the rest.
    if any_failed {
        return degrade_to_per_file(root, &deleted);
    }

    // Remove the already-dehydrated `.cloudsc` children (subsumed by the folder
    // placeholder). If one won't delete, degrade rather than half-collapse.
    for p in &cloudsc_children {
        if fs::remove_file(p).is_err() {
            return degrade_to_per_file(root, &deleted);
        }
    }

    // Untrack the subtree's folder rows BEFORE removing the dirs — a surviving
    // subfolder row would make `process_known_folder_deletion` fire a RECURSIVE
    // remote delete.
    untrack_folders_subtree(state, folder_rel)?;

    // Remove directories deepest-first, EMPTY-ONLY (`remove_dir`, never a blanket
    // `remove_dir_all`). A dir left non-empty by a file that arrived after Phase 1,
    // or by an ignored/symlink entry, simply isn't removed — so that content is
    // preserved instead of being blown away.
    dirs.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, dir) in &dirs {
        let _ = fs::remove_dir(dir); // ignores "directory not empty"
    }

    if abs.exists() {
        // The folder couldn't be fully emptied (a concurrent/ignored/symlink entry
        // remains) → degrade: represent what we freed as per-file placeholders. The
        // folder stays and is re-tracked by the next sweep.
        return degrade_to_per_file(root, &deleted);
    }

    // Fully collapsed → a single folder placeholder.
    let meta = CloudscMeta {
        version: 1,
        tag: "folder".to_string(),
        remote_path_display: normalize_dropbox_path(folder_rel)?,
    };
    write_cloudsc_placeholder_file(&with_cloudsc_suffix(&abs), &meta)?;
    Ok(vec![folder_rel.to_string()])
}

/// Write a per-file `.cloudsc` for each already-deleted file (recreating parent
/// dirs a collapse attempt may have removed). Used when a folder can't be cleanly
/// collapsed; the freed files are represented individually and the folder is kept.
fn degrade_to_per_file(root: &Path, deleted: &[String]) -> AppResult<Vec<String>> {
    for file_rel in deleted {
        write_file_placeholder(root, file_rel)?;
    }
    Ok(deleted.to_vec())
}

/// Remove every `known_folders` row for `folder_rel` and anything beneath it
/// (`folder_rel/…`). Keys are `/`-canonical, so a lexical prefix match is exact.
fn untrack_folders_subtree(state: &AppState, folder_rel: &str) -> AppResult<()> {
    let prefix = format!("{folder_rel}/");
    for folder in state.db.list_known_folders()? {
        if folder == folder_rel || folder.starts_with(&prefix) {
            state.db.remove_known_folder(&folder)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The materialized-folder sweep maps each real local directory to its Dropbox
    // list_folder path via `normalize_dropbox_path(relpath_under(root, dir))`. The
    // root must map to "" (Dropbox root), never "/", and nested dirs must use
    // forward slashes regardless of the OS separator.
    #[test]
    fn root_dir_maps_to_empty_dropbox_path() {
        let root = PathBuf::from("/sync/root");
        let rel = relpath_under(&root, &root).expect("relpath");
        assert_eq!(normalize_dropbox_path(&rel).unwrap(), "");
    }

    #[test]
    fn nested_dir_maps_to_leading_slash_forward_path() {
        let root = PathBuf::from("/sync/root");
        let dir = root.join("Cocina").join("Pizza");
        let rel = relpath_under(&root, &dir).expect("relpath");
        // `normalize_dropbox_path` itself converts OS separators to '/'.
        assert_eq!(normalize_dropbox_path(&rel).unwrap(), "/Cocina/Pizza");
    }

    // ---------------------------------------------------------------------------
    // Dehydrate / free up space (DBSYNC-33)
    // ---------------------------------------------------------------------------

    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    fn build_state(sync_folder: &Path) -> AppState {
        std::fs::create_dir_all(sync_folder).expect("create sync folder");
        let dbdir = tempfile::tempdir().expect("db tempdir");
        let db_path = dbdir.path().join("app.db");
        std::mem::forget(dbdir);
        let db = crate::storage::db::Db::new_at(&db_path).expect("db");
        db.set_sync_folder(&sync_folder.to_string_lossy())
            .expect("set folder");
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

    #[test]
    fn dehydrate_synced_file_creates_placeholder_and_frees_local() {
        let tmp = tempfile::tempdir().unwrap();
        let sync = tmp.path().join("synced");
        let state = build_state(&sync);
        std::fs::write(sync.join("a.txt"), b"hello").unwrap();
        let (h, _, _) = hash_file(&sync.join("a.txt")).unwrap();
        state.db.upsert_local_file("a.txt", &h, 5, 0).unwrap();
        state.db.upsert_remote_file("a.txt", &h, "rev", 0).unwrap();

        let n = dehydrate_path_internal(&state, "a.txt").unwrap();
        assert_eq!(n, 1);
        assert!(!sync.join("a.txt").exists(), "local file should be freed");
        assert!(sync.join("a.txt.cloudsc").exists(), "placeholder written");
        assert!(
            state.db.get_local_file("a.txt").unwrap().is_none(),
            "index row untracked"
        );
    }

    #[test]
    fn dehydrate_refuses_unsynced_file() {
        let tmp = tempfile::tempdir().unwrap();
        let sync = tmp.path().join("synced");
        let state = build_state(&sync);
        std::fs::write(sync.join("b.txt"), b"local edit").unwrap();
        state.db.upsert_local_file("b.txt", "LOCALHASH", 10, 0).unwrap();
        // Remote hash differs → the local copy has unsynced changes.
        state.db.upsert_remote_file("b.txt", "REMOTEHASH", "rev", 0).unwrap();

        let err = dehydrate_path_internal(&state, "b.txt").unwrap_err();
        assert!(format!("{err}").contains("not fully synced"), "got: {err}");
        assert!(sync.join("b.txt").exists(), "unsynced file must be kept");
        assert!(!sync.join("b.txt.cloudsc").exists());
        assert!(state.db.get_local_file("b.txt").unwrap().is_some());
    }

    #[test]
    fn dehydrate_refuses_when_disk_differs_from_remote_despite_stale_index() {
        let tmp = tempfile::tempdir().unwrap();
        let sync = tmp.path().join("synced");
        let state = build_state(&sync);
        // The index CLAIMS synced (local hash == remote hash), but the on-disk
        // bytes were edited after the last index (watcher hasn't caught up). The
        // authoritative re-hash must catch this and refuse — never lose the edit.
        std::fs::write(sync.join("d.txt"), b"edited after the last index update").unwrap();
        state.db.upsert_local_file("d.txt", "STALE_SYNCED", 5, 0).unwrap();
        state.db.upsert_remote_file("d.txt", "STALE_SYNCED", "rev", 0).unwrap();

        let err = dehydrate_path_internal(&state, "d.txt").unwrap_err();
        assert!(format!("{err}").contains("not fully synced"), "got: {err}");
        assert!(sync.join("d.txt").exists(), "the edited file must be kept");
        assert!(!sync.join("d.txt.cloudsc").exists());
    }

    #[test]
    fn dehydrate_does_not_trigger_remote_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let sync = tmp.path().join("synced");
        let state = build_state(&sync);
        std::fs::write(sync.join("c.txt"), b"data").unwrap();
        let (h, _, _) = hash_file(&sync.join("c.txt")).unwrap();
        state.db.upsert_local_file("c.txt", &h, 4, 0).unwrap();
        state.db.upsert_remote_file("c.txt", &h, "rev", 0).unwrap();

        dehydrate_path_internal(&state, "c.txt").unwrap();

        // The watcher/scan now re-evaluates the (absent) file. It must NOT enqueue
        // a remote delete (DBSYNC-33 constraint / DBSYNC-45 guard).
        crate::sync_pipeline::process_changed_paths(&state, &["c.txt".to_string()]).unwrap();
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
            "dehydration must never enqueue a remote delete, got {deletes:?}"
        );
    }

    /// Register a real file under the sync root as a fully-synced tracked file.
    fn track_synced_file(state: &AppState, sync: &Path, rel: &str, bytes: &[u8]) {
        let abs = sync.join(rel);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(&abs, bytes).unwrap();
        let (h, _, _) = hash_file(&abs).unwrap();
        state.db.upsert_local_file(rel, &h, bytes.len() as i64, 0).unwrap();
        state.db.upsert_remote_file(rel, &h, "rev", 0).unwrap();
    }

    #[test]
    fn dehydrate_folder_collapses_to_single_placeholder_and_frees_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let sync = tmp.path().join("synced");
        let state = build_state(&sync);
        track_synced_file(&state, &sync, "dir/a.txt", b"aaa");
        track_synced_file(&state, &sync, "dir/sub/b.txt", b"bbb");
        state.db.upsert_known_folder("dir").unwrap();
        state.db.upsert_known_folder("dir/sub").unwrap();

        let n = dehydrate_path_internal(&state, "dir").unwrap();
        assert_eq!(n, 1);
        assert!(!sync.join("dir").exists(), "the whole local subtree is freed");
        assert!(
            sync.join("dir.cloudsc").exists(),
            "one folder-level placeholder written"
        );
        let meta = read_cloudsc_placeholder_file(&sync.join("dir.cloudsc")).unwrap();
        assert_eq!(meta.tag, "folder", "must be a folder placeholder");
        // Every descendant row untracked (files + subfolders).
        assert!(state.db.get_local_file("dir/a.txt").unwrap().is_none());
        assert!(state.db.get_local_file("dir/sub/b.txt").unwrap().is_none());
        assert!(
            state.db.list_known_folders().unwrap().is_empty(),
            "known folders untracked"
        );
    }

    #[test]
    fn dehydrate_folder_rejects_when_a_child_is_unsynced() {
        let tmp = tempfile::tempdir().unwrap();
        let sync = tmp.path().join("synced");
        let state = build_state(&sync);
        track_synced_file(&state, &sync, "dir/ok.txt", b"ok");
        // An unsynced sibling (on-disk bytes hash != remote hash).
        std::fs::write(sync.join("dir/edited.txt"), b"local only").unwrap();
        state.db.upsert_local_file("dir/edited.txt", "LOCAL", 10, 0).unwrap();
        state.db.upsert_remote_file("dir/edited.txt", "REMOTE", "rev", 0).unwrap();

        let err = dehydrate_path_internal(&state, "dir").unwrap_err();
        assert!(format!("{err}").contains("unsynced"), "got: {err}");
        // Nothing touched — reject happens before any mutation.
        assert!(sync.join("dir/ok.txt").exists());
        assert!(sync.join("dir/edited.txt").exists());
        assert!(!sync.join("dir.cloudsc").exists());
        assert!(state.db.get_local_file("dir/ok.txt").unwrap().is_some());
    }

    #[test]
    fn dehydrate_folder_does_not_trigger_remote_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let sync = tmp.path().join("synced");
        let state = build_state(&sync);
        track_synced_file(&state, &sync, "dir/a.txt", b"aaa");
        track_synced_file(&state, &sync, "dir/sub/b.txt", b"bbb");
        state.db.upsert_known_folder("dir").unwrap();
        state.db.upsert_known_folder("dir/sub").unwrap();

        dehydrate_path_internal(&state, "dir").unwrap();

        // Re-evaluate every now-absent path — files AND subfolders. None may enqueue
        // a remote delete (a leftover subfolder row would cause a RECURSIVE delete).
        crate::sync_pipeline::process_changed_paths(
            &state,
            &[
                "dir".to_string(),
                "dir/sub".to_string(),
                "dir/a.txt".to_string(),
                "dir/sub/b.txt".to_string(),
            ],
        )
        .unwrap();
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
            "folder dehydration must never enqueue a remote delete, got {deletes:?}"
        );
    }

    #[test]
    fn dehydrate_refuses_sync_root() {
        let tmp = tempfile::tempdir().unwrap();
        let sync = tmp.path().join("synced");
        let state = build_state(&sync);
        std::fs::write(sync.join("keep.txt"), b"x").unwrap();

        let err = dehydrate_path_internal(&state, "").unwrap_err();
        assert!(format!("{err}").contains("sync root"), "got: {err}");
        assert!(sync.join("keep.txt").exists(), "root contents untouched");
    }

    /// When a file can't be deleted (locked / in use), it must stay fully synced
    /// (real, tracked) and the folder degrades to per-file dehydration of the rest
    /// — never a partial collapse. Simulated on Windows by holding an open handle
    /// that denies delete-sharing, so `fs::remove_file` fails with a sharing
    /// violation (`remove_file` clears the read-only attribute, so that wouldn't).
    #[cfg(windows)]
    #[test]
    fn dehydrate_folder_locked_file_stays_synced_and_siblings_go_per_file() {
        use std::os::windows::fs::OpenOptionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let sync = tmp.path().join("synced");
        let state = build_state(&sync);
        track_synced_file(&state, &sync, "dir/free.txt", b"free");
        track_synced_file(&state, &sync, "dir/locked.txt", b"lock");
        state.db.upsert_known_folder("dir").unwrap();

        // FILE_SHARE_READ only (no delete-sharing) → deletion fails while held; a
        // reader (the freshness re-hash) is still allowed.
        let locked = sync.join("dir/locked.txt");
        let handle = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0x1)
            .open(&locked)
            .unwrap();

        let n = dehydrate_path_internal(&state, "dir").unwrap();

        // Degraded to per-file: no folder collapse, folder kept.
        assert!(
            !sync.join("dir.cloudsc").exists(),
            "must not collapse to a folder placeholder when a file is locked"
        );
        assert!(sync.join("dir").is_dir(), "folder is kept");
        // The deletable sibling became a placeholder.
        assert_eq!(n, 1, "one deletable sibling dehydrated");
        assert!(!sync.join("dir/free.txt").exists());
        assert!(sync.join("dir/free.txt.cloudsc").exists());
        // The locked file stays REAL and still tracked (synced) — nothing lost.
        assert!(locked.exists(), "locked file is kept");
        assert!(
            state.db.get_local_file("dir/locked.txt").unwrap().is_some(),
            "locked file stays tracked/synced after rollback"
        );

        drop(handle); // release so the tempdir can be cleaned up
    }

    #[test]
    fn dehydrate_folder_with_ignored_file_degrades_to_per_file() {
        let tmp = tempfile::tempdir().unwrap();
        let sync = tmp.path().join("synced");
        let state = build_state(&sync);
        track_synced_file(&state, &sync, "dir/a.txt", b"aaa");
        state.db.upsert_known_folder("dir").unwrap();
        // An ignored file keeps the folder non-empty → no collapse, but no data loss.
        std::fs::write(sync.join("dir/.DS_Store"), b"junk").unwrap();

        let n = dehydrate_path_internal(&state, "dir").unwrap();

        assert_eq!(n, 1);
        assert!(
            !sync.join("dir.cloudsc").exists(),
            "must not collapse while an ignored file remains"
        );
        assert!(sync.join("dir").is_dir(), "folder kept");
        assert!(sync.join("dir/.DS_Store").exists(), "ignored file preserved");
        assert!(!sync.join("dir/a.txt").exists());
        assert!(sync.join("dir/a.txt.cloudsc").exists(), "freed file → per-file placeholder");
    }

    #[test]
    fn dehydrate_folder_collapses_over_existing_cloudsc_children() {
        let tmp = tempfile::tempdir().unwrap();
        let sync = tmp.path().join("synced");
        let state = build_state(&sync);
        track_synced_file(&state, &sync, "dir/a.txt", b"aaa");
        state.db.upsert_known_folder("dir").unwrap();
        // A previously-dehydrated child placeholder must not block the collapse.
        std::fs::write(sync.join("dir/old.txt.cloudsc"), b"{}").unwrap();

        let n = dehydrate_path_internal(&state, "dir").unwrap();

        assert_eq!(n, 1);
        assert!(!sync.join("dir").exists(), "subtree freed incl. the .cloudsc child");
        assert!(sync.join("dir.cloudsc").exists(), "collapsed to one folder placeholder");
        let meta = read_cloudsc_placeholder_file(&sync.join("dir.cloudsc")).unwrap();
        assert_eq!(meta.tag, "folder");
    }

    #[test]
    fn dehydrate_folder_leaves_sibling_folders_tracked() {
        let tmp = tempfile::tempdir().unwrap();
        let sync = tmp.path().join("synced");
        let state = build_state(&sync);
        track_synced_file(&state, &sync, "dir/a.txt", b"aaa");
        state.db.upsert_known_folder("dir").unwrap();
        // Siblings whose known-folder rows share a name PREFIX with "dir" must NOT
        // be untracked (a `starts_with` bug would nuke them → recursive remote delete).
        state.db.upsert_known_folder("dir2").unwrap();
        state.db.upsert_known_folder("dirtest/sub").unwrap();

        dehydrate_path_internal(&state, "dir").unwrap();

        let mut folders = state.db.list_known_folders().unwrap();
        folders.sort();
        assert_eq!(
            folders,
            vec!["dir2".to_string(), "dirtest/sub".to_string()],
            "only 'dir' and its descendants may be untracked"
        );
    }
}
