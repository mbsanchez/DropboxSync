use std::fs;
use std::path::{Path, PathBuf};

use reqwest::blocking::Client;
use walkdir::WalkDir;

use crate::auth_session::get_access_token;
use crate::cloudsc::{
    cloudsc_target_path, read_cloudsc_placeholder_file, write_cloudsc_placeholder_file,
};
use crate::dropbox_transfer::download_remote_file_internal;
use crate::models::{
    CloudscMeta, CloudscPlaceholderInfo, DropboxListFolderResponse,
};
use crate::path_util::{is_path_allowed, parse_prefix_csv, relpath_under};
use crate::state::AppState;

pub(crate) fn index_remote_folder_children_as_cloudsc_placeholders_internal(
    state: &AppState,
    remote_folder_path_display: &str,
    local_dir: &Path,
) -> Result<usize, String> {
    let token = get_access_token(state)?;
    let include_prefixes = parse_prefix_csv(state.db.get_include_prefixes_csv()?);
    let exclude_prefixes = parse_prefix_csv(state.db.get_exclude_prefixes_csv()?);

    fs::create_dir_all(local_dir).map_err(|e| format!("failed creating local dir: {e}"))?;

    let client = Client::new();
    let response = client
        .post("https://api.dropboxapi.com/2/files/list_folder")
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "path": remote_folder_path_display,
            "recursive": false,
            "include_deleted": false
        }))
        .send()
        .map_err(|e| format!("list_folder request failed: {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_else(|_| "<unreadable body>".to_string());
        return Err(format!(
            "list_folder status error for {remote_folder_path_display}: {status}; body: {body}"
        ));
    }

    let entries_resp: DropboxListFolderResponse = response
        .json()
        .map_err(|e| format!("list_folder parse failed: {e}"))?;

    let mut created = 0usize;
    for entry in entries_resp.entries {
        let tag = entry.tag;
        let path_display = match entry.path_display {
            Some(p) => p,
            None => continue,
        };

        let relative = path_display.trim_start_matches('/').to_string();
        if !is_path_allowed(&relative, &include_prefixes, &exclude_prefixes) {
            continue;
        }

        let child_name = path_display
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(&path_display);
        let placeholder_path = local_dir.join(format!("{child_name}.cloudsc"));
        let target_path = cloudsc_target_path(&placeholder_path);
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
    }

    Ok(created)
}

pub(crate) fn index_remote_root_children_as_cloudsc_placeholders_internal(
    state: &AppState,
) -> Result<usize, String> {
    let token = get_access_token(state)?;
    let sync_folder_str = state
        .db
        .get_sync_folder()?
        .ok_or_else(|| "sync folder not configured".to_string())?;
    let sync_folder = PathBuf::from(sync_folder_str);
    fs::create_dir_all(&sync_folder).map_err(|e| format!("failed creating sync folder: {e}"))?;

    let include_prefixes = parse_prefix_csv(state.db.get_include_prefixes_csv()?);
    let exclude_prefixes = parse_prefix_csv(state.db.get_exclude_prefixes_csv()?);

    let client = Client::new();
    let response = client
        .post("https://api.dropboxapi.com/2/files/list_folder")
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "path": "",
            "recursive": false,
            "include_deleted": false
        }))
        .send()
        .map_err(|e| format!("list_folder request failed: {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_else(|_| "<unreadable body>".to_string());
        return Err(format!("list_folder status error for root: {status}; body: {body}"));
    }

    let entries_resp: DropboxListFolderResponse = response
        .json()
        .map_err(|e| format!("list_folder parse failed: {e}"))?;

    let mut created = 0usize;
    for entry in entries_resp.entries {
        let tag = entry.tag;
        let path_display = match entry.path_display {
            Some(p) => p,
            None => continue,
        };

        let relative = path_display.trim_start_matches('/').to_string();
        if !is_path_allowed(&relative, &include_prefixes, &exclude_prefixes) {
            continue;
        }

        let child_name = path_display
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(&path_display);
        let placeholder_path = sync_folder.join(format!("{child_name}.cloudsc"));
        let target_path = cloudsc_target_path(&placeholder_path);
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
    }

    Ok(created)
}

pub(crate) fn list_cloudsc_placeholders(
    state: &AppState,
    limit: usize,
) -> Result<Vec<CloudscPlaceholderInfo>, String> {
    let sync_folder_str = state
        .db
        .get_sync_folder()?
        .ok_or_else(|| "sync folder not configured".to_string())?;
    let sync_folder = PathBuf::from(sync_folder_str);

    let mut out: Vec<CloudscPlaceholderInfo> = Vec::new();
    for entry in WalkDir::new(&sync_folder).min_depth(1) {
        let entry = entry.map_err(|e| format!("walk dir error: {e}"))?;
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
) -> Result<usize, String> {
    let sync_folder = state
        .db
        .get_sync_folder()?
        .ok_or_else(|| "sync folder not configured".to_string())?;

    let placeholder_path = PathBuf::from(&sync_folder).join(placeholder_local_rel_path);
    if !placeholder_path.exists() {
        return Err(format!("placeholder not found: {placeholder_local_rel_path}"));
    }

    let meta = read_cloudsc_placeholder_file(&placeholder_path)?;
    let target_path = cloudsc_target_path(&placeholder_path);

    if meta.tag == "file" {
        download_remote_file_internal(state, &meta.remote_path_display)?;
        fs::remove_file(&placeholder_path).map_err(|e| format!("failed removing placeholder: {e}"))?;
        Ok(1)
    } else {
        fs::create_dir_all(&target_path)
            .map_err(|e| format!("failed creating hydrated folder: {e}"))?;
        fs::remove_file(&placeholder_path)
            .map_err(|e| format!("failed removing folder placeholder: {e}"))?;

        index_remote_folder_children_as_cloudsc_placeholders_internal(
            state,
            &meta.remote_path_display,
            &target_path,
        )
    }
}
