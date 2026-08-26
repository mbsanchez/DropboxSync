//! DBSYNC-52: generate a Dropbox shared link for a synced item and copy it to
//! the clipboard, triggered from the shell context menu (`copy_link` action).

use serde::Deserialize;

use crate::auth_session::get_access_token;
use crate::error::{AppError, AppResult};
use crate::path_util::normalize_dropbox_path;
use crate::state::AppState;

#[derive(Deserialize)]
struct SharedLinkMetadata {
    url: String,
}

#[derive(Deserialize)]
struct ListSharedLinksResult {
    links: Vec<SharedLinkMetadata>,
}

/// Create a Dropbox shared link for `remote_rel` (a `/`-relative path under the
/// sync root, WITHOUT any `.cloudsc` suffix), or return the existing one if a
/// link already exists. Returns the public `https://www.dropbox.com/...` URL.
pub(crate) fn create_or_get_shared_link(state: &AppState, remote_rel: &str) -> AppResult<String> {
    let token = get_access_token(state)?;
    let dropbox_path = normalize_dropbox_path(remote_rel)?;
    let client = &state.http_client;

    let resp = client
        .post("https://api.dropboxapi.com/2/sharing/create_shared_link_with_settings")
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({ "path": dropbox_path }))
        .send()
        .map_err(|e| AppError::Network(format!("create_shared_link request failed: {e}")))?;

    if resp.status().is_success() {
        let meta: SharedLinkMetadata = resp
            .json()
            .map_err(|e| AppError::Other(format!("create_shared_link parse failed: {e}")))?;
        return Ok(meta.url);
    }

    let status = resp.status();
    let body = resp
        .text()
        .unwrap_or_else(|_| "<unreadable body>".to_string());
    // A link already exists for this path → fetch and return it (the common case
    // for a file shared before).
    if body.contains("shared_link_already_exists") {
        return existing_shared_link(state, token.as_str(), &dropbox_path);
    }
    Err(AppError::Dropbox {
        status: status.as_u16(),
        message: format!("create_shared_link for {dropbox_path}: {body}"),
    })
}

fn existing_shared_link(state: &AppState, token: &str, dropbox_path: &str) -> AppResult<String> {
    let resp = state
        .http_client
        .post("https://api.dropboxapi.com/2/sharing/list_shared_links")
        .bearer_auth(token)
        .json(&serde_json::json!({ "path": dropbox_path, "direct_only": true }))
        .send()
        .map_err(|e| AppError::Network(format!("list_shared_links request failed: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp
            .text()
            .unwrap_or_else(|_| "<unreadable body>".to_string());
        return Err(AppError::Dropbox {
            status: status.as_u16(),
            message: format!("list_shared_links for {dropbox_path}: {body}"),
        });
    }
    let result: ListSharedLinksResult = resp
        .json()
        .map_err(|e| AppError::Other(format!("list_shared_links parse failed: {e}")))?;
    result
        .links
        .into_iter()
        .next()
        .map(|l| l.url)
        .ok_or_else(|| AppError::Sync(format!("no shared link found for {dropbox_path}")))
}

/// The remote path to share for a selected item: a `.cloudsc` placeholder shares
/// its underlying REMOTE file (`name`), not the placeholder (`name.cloudsc`).
pub(crate) fn remote_rel_from_item(rel: &str) -> &str {
    rel.strip_suffix(".cloudsc").unwrap_or(rel)
}

/// Put `text` on the OS clipboard.
pub(crate) fn copy_to_clipboard(text: &str) -> AppResult<()> {
    let mut clipboard = arboard::Clipboard::new()
        .map_err(|e| AppError::Other(format!("clipboard unavailable: {e}")))?;
    clipboard
        .set_text(text.to_string())
        .map_err(|e| AppError::Other(format!("clipboard write failed: {e}")))
}

/// Show a native OS notification (best-effort; no-ops if no `AppHandle` is set).
///
/// Gated out of `cfg(test)` so the `tauri-plugin-notification` WinRT imports don't
/// get linked into the lib test binary (see the note in `lib.rs::run`).
#[cfg(not(test))]
pub(crate) fn notify(title: &str, body: &str) {
    use tauri_plugin_notification::NotificationExt;
    let Some(handle) = crate::state::APP_HANDLE.get() else {
        return;
    };
    if let Err(e) = handle
        .notification()
        .builder()
        .title(title)
        .body(body)
        .show()
    {
        tracing::warn!(error = %e, "failed to show notification");
    }
}

#[cfg(test)]
pub(crate) fn notify(_title: &str, _body: &str) {}

/// OS notification for a newly detected sync conflict (DBSYNC-35). Delegates to
/// `notify`, so it is fire-and-forget and a no-op in tests / before the app handle
/// exists. Conflict creation is de-duplicated at each call site, so this never spams.
pub(crate) fn notify_conflict(rel: &str) {
    notify(
        "DropboxSync",
        &format!("Sync conflict on {rel} — open the app to resolve it"),
    );
}

#[cfg(test)]
mod tests {
    use super::remote_rel_from_item;

    #[test]
    fn cloudsc_item_maps_to_its_remote_path() {
        assert_eq!(
            remote_rel_from_item("dir/report.docx.cloudsc"),
            "dir/report.docx"
        );
        assert_eq!(remote_rel_from_item("report.docx"), "report.docx");
        assert_eq!(remote_rel_from_item("folder.cloudsc"), "folder");
        // Only a trailing suffix is stripped.
        assert_eq!(remote_rel_from_item("a.cloudsc/b.txt"), "a.cloudsc/b.txt");
    }
}
