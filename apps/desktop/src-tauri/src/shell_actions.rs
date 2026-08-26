//! OS-agnostic shell action dispatcher (DBSYNC-51 foundation). Native shell
//! surfaces (Windows context-menu verb, macOS Finder Sync menu, Linux) invoke
//! the running app with `--action=<name> --path=<abs>`; this parses those args
//! and routes them to the matching internal op. Fire-and-forget — the app
//! performs the whole action. The transport is the tauri single-instance arg
//! channel (the same one the `.cloudsc`-open flow uses), so no new IPC socket
//! is needed.
//!
//! Status (the other half of the shell contract) is published separately as the
//! versioned `overlay_state.json` file (see `overlay_state.rs`), read by the
//! overlays (macOS/Linux) and the status column.

use std::path::{Path, PathBuf};

use tauri::{AppHandle, Manager};

use crate::error::{AppError, AppResult};
use crate::open_handlers::{resolve_cloudsc_rel_path, spawn_drain_sync_queue_if_idle};
use crate::path_util::relpath_under;
use crate::state::AppState;
use crate::sync_pipeline::refresh_queue_depth_internal;

/// Parse `--action=<name> --path=<abs>` (also accepts the space-separated
/// `--action <name> --path <abs>`) out of a process argv. Returns `None` unless
/// both are present and non-empty. Pure — unit-testable.
pub(crate) fn parse_action_args(args: &[String]) -> Option<(String, PathBuf)> {
    let mut action: Option<String> = None;
    let mut path: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(v) = a.strip_prefix("--action=") {
            action = Some(v.to_string());
        } else if a == "--action" {
            action = args.get(i + 1).cloned();
            i += 1;
        } else if let Some(v) = a.strip_prefix("--path=") {
            path = Some(v.to_string());
        } else if a == "--path" {
            path = args.get(i + 1).cloned();
            i += 1;
        }
        i += 1;
    }
    match (action, path) {
        (Some(a), Some(p)) if !a.is_empty() && !p.is_empty() => Some((a, PathBuf::from(p))),
        _ => None,
    }
}

/// Route a shell action to its internal op. Returns `Ok(true)` when it enqueued
/// work the caller should drain. Validates the path is under the sync root;
/// unknown actions and out-of-root paths are rejected.
pub(crate) fn dispatch_action(state: &AppState, action: &str, abs_path: &Path) -> AppResult<bool> {
    match action {
        // Foundation smoke verb: reuse the proven `.cloudsc`-open path (which
        // validates the path is under the root and is a placeholder).
        "hydrate" => {
            // DBSYNC-59 Slice 2: native CfAPI content is a real file/folder (not a
            // `.cloudsc` sidecar). It hydrates in place via the platform + our
            // FETCH_DATA handler — no queue job — so handle it before the `.cloudsc`
            // resolver (which would reject a non-`.cloudsc` path). A folder hydrates
            // all of its dehydrated children.
            #[cfg(windows)]
            if abs_path.is_dir() {
                let rel = validate_under_root(state, abs_path)?;
                let n = crate::cloud_filter::hydrate_folder(abs_path);
                tracing::info!(action = "hydrate", path = %rel, count = n, "cfapi folder hydrated in place");
                return Ok(false);
            }
            #[cfg(windows)]
            if crate::path_util::is_dehydrated_placeholder(abs_path) {
                let rel = validate_under_root(state, abs_path)?;
                let ok = crate::cloud_filter::hydrate_placeholder(abs_path);
                tracing::info!(action = "hydrate", path = %rel, ok, "cfapi placeholder hydrated in place");
                return Ok(false);
            }
            let rel = resolve_cloudsc_rel_path(state, abs_path)?;
            state
                .db
                .enqueue_job("hydrate_cloudsc", Some(rel.as_str()), None)?;
            refresh_queue_depth_internal(state)?;
            tracing::info!(action = "hydrate", file_path = %rel, "shell action queued");
            Ok(true)
        }
        // DBSYNC-33: replace the local copy with a `.cloudsc` placeholder,
        // freeing space. The op removes the index row + writes the placeholder
        // before deleting, so the local delete never propagates to Dropbox.
        "free_up_space" | "dehydrate" => {
            let rel = validate_under_root(state, abs_path)?;
            let n = crate::cloudsc_ops::dehydrate_path_internal(state, &rel)?;
            tracing::info!(action = "free_up_space", path = %rel, count = n, "shell action done");
            Ok(false) // immediate; nothing to drain
        }
        // DBSYNC-52: generate a Dropbox shared link for the item and copy it to the
        // clipboard. A `.cloudsc` placeholder shares its REMOTE file (`name`), not
        // `name.cloudsc`. Errors are surfaced non-fatally (logged + a notification).
        "copy_link" => {
            let rel = validate_under_root(state, abs_path)?;
            let remote_rel = crate::sharing::remote_rel_from_item(&rel).to_string();
            // Generate the link AND copy it as one fallible step, so a clipboard
            // failure (another app holding the clipboard open) surfaces the same
            // error notification as a link-generation failure — never silent.
            let result = crate::sharing::create_or_get_shared_link(state, &remote_rel)
                .and_then(|url| crate::sharing::copy_to_clipboard(&url));
            match result {
                Ok(()) => {
                    tracing::info!(action = "copy_link", path = %remote_rel, "share link copied to clipboard");
                    crate::sharing::notify("DropboxSync", "Link copied to clipboard");
                    Ok(false)
                }
                Err(e) => {
                    tracing::warn!(action = "copy_link", path = %remote_rel, error = %e, "copy link failed");
                    crate::sharing::notify("DropboxSync", "Couldn't copy the Dropbox link");
                    Err(e)
                }
            }
        }
        // DEV / validation (DBSYNC-59 Slice 1, Windows-only): dehydrate a hydrated
        // CfAPI placeholder so on-demand hydration can be exercised end-to-end.
        // Not a user-facing verb; the real "free up space" → CfAPI mapping is Slice 2.
        #[cfg(windows)]
        "cfapi_dehydrate" => {
            let rel = validate_under_root(state, abs_path)?;
            let ok = crate::cloud_filter::dehydrate_for_test(abs_path);
            tracing::info!(action = "cfapi_dehydrate", path = %rel, ok, "shell action done");
            Ok(false)
        }
        other => Err(AppError::Sync(format!("unknown shell action: {other}"))),
    }
}

/// Resolve `abs` to its `/`-relative path under the sync root, rejecting any path
/// outside it (or a missing sync folder / non-existent path).
fn validate_under_root(state: &AppState, abs: &Path) -> AppResult<String> {
    let folder = state
        .db
        .get_sync_folder()?
        .ok_or_else(|| AppError::Sync("sync folder not configured".to_string()))?;
    let sync_canon = PathBuf::from(&folder)
        .canonicalize()
        .map_err(|e| AppError::Io(format!("invalid sync folder: {e}")))?;
    let abs_canon = abs
        .canonicalize()
        .map_err(|e| AppError::Io(format!("invalid path: {e}")))?;
    Ok(relpath_under(&sync_canon, &abs_canon)?.replace('\\', "/"))
}

/// Handle a parsed action delivered by the OS (single-instance / cold start).
pub(crate) fn handle_action_from_os(app_handle: &AppHandle, action: &str, abs_path: &Path) {
    let Some(state) = app_handle.try_state::<AppState>() else {
        tracing::warn!("handle_action_from_os: AppState not available");
        return;
    };
    let app_state = state.inner().clone();
    match dispatch_action(&app_state, action, abs_path) {
        Ok(true) => spawn_drain_sync_queue_if_idle(app_state),
        Ok(false) => {}
        Err(e) => tracing::warn!(
            action = %action,
            path = %abs_path.display(),
            error = %e,
            "shell action rejected"
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::parse_action_args;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_equals_form() {
        let got = parse_action_args(&v(&[
            "exe.exe",
            "--action=hydrate",
            "--path=C:/sync/a.cloudsc",
        ]));
        assert_eq!(
            got,
            Some(("hydrate".to_string(), PathBuf::from("C:/sync/a.cloudsc")))
        );
    }

    #[test]
    fn parses_space_form() {
        let got = parse_action_args(&v(&[
            "exe.exe",
            "--action",
            "free_up_space",
            "--path",
            "/sync/dir/file.txt",
        ]));
        assert_eq!(
            got,
            Some((
                "free_up_space".to_string(),
                PathBuf::from("/sync/dir/file.txt")
            ))
        );
    }

    #[test]
    fn missing_action_or_path_is_none() {
        assert_eq!(parse_action_args(&v(&["exe", "--path=/x/y"])), None);
        assert_eq!(parse_action_args(&v(&["exe", "--action=hydrate"])), None);
        assert_eq!(
            parse_action_args(&v(&["exe", "--action=", "--path=/x"])),
            None
        );
        assert_eq!(parse_action_args(&v(&["exe"])), None);
    }

    #[test]
    fn ignores_unrelated_args_like_cloudsc_open() {
        // A `.cloudsc` double-click passes just the file path, no --action.
        assert_eq!(parse_action_args(&v(&["exe", "C:/sync/a.cloudsc"])), None);
    }

    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    use super::{dispatch_action, validate_under_root};
    use crate::state::AppState;

    fn build_state_with_sync_folder(folder: &std::path::Path) -> AppState {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("app.db");
        std::mem::forget(dir); // keep the DB file alive for the test body
        let db = crate::storage::db::Db::new_at(&db_path).expect("db");
        db.set_sync_folder(&folder.to_string_lossy())
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
    fn dispatch_rejects_unknown_action() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = build_state_with_sync_folder(tmp.path());
        let err = dispatch_action(&state, "bogus_verb", tmp.path()).unwrap_err();
        assert!(
            format!("{err}").contains("unknown shell action"),
            "got: {err}"
        );
    }

    #[test]
    fn validate_under_root_accepts_inside_rejects_outside() {
        let root = tempfile::tempdir().expect("root");
        let outside = tempfile::tempdir().expect("outside");
        let inside = root.path().join("in.txt");
        std::fs::write(&inside, b"y").unwrap();
        let out_file = outside.path().join("x.txt");
        std::fs::write(&out_file, b"x").unwrap();

        let state = build_state_with_sync_folder(root.path());
        assert_eq!(validate_under_root(&state, &inside).unwrap(), "in.txt");
        assert!(
            validate_under_root(&state, &out_file).is_err(),
            "a path outside the sync root must be rejected"
        );
    }
}
