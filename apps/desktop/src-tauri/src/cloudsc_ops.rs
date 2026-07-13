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
use crate::path_util::{is_path_allowed, normalize_dropbox_path, parse_prefix_csv, relpath_under};
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
            .bearer_auth(&token)
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

            let placeholder_path = local_dir.join(format!("{child_name}.cloudsc"));
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
            .bearer_auth(&token)
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
        let remote_path = normalize_dropbox_path(&rel); // "" for root, "/Cocina", ...
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

    let placeholder_path = PathBuf::from(&sync_folder).join(placeholder_local_rel_path);
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
        assert_eq!(normalize_dropbox_path(&rel), "");
    }

    #[test]
    fn nested_dir_maps_to_leading_slash_forward_path() {
        let root = PathBuf::from("/sync/root");
        let dir = root.join("Cocina").join("Pizza");
        let rel = relpath_under(&root, &dir).expect("relpath");
        // `normalize_dropbox_path` itself converts OS separators to '/'.
        assert_eq!(normalize_dropbox_path(&rel), "/Cocina/Pizza");
    }
}
