//! Filesystem watcher (DBSYNC-29): near-instant local change detection via the
//! `notify` crate, replacing the 60s full-scan poll as the *primary* trigger.
//! A debounced batch of changed paths is handed to the targeted, network-free
//! `sync_pipeline::process_changed_paths` (no full walk), gated by the same
//! `sync_running` single-flight CAS as every other sync entry point. The
//! periodic full scan remains as a 5-minute safety net.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use notify_debouncer_mini::notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_mini::{new_debouncer, DebounceEventResult, Debouncer};

use crate::error::{AppError, AppResult};
use crate::path_util::{is_editor_temp_path, should_ignore_local_path, validate_relative};
use crate::state::AppState;

/// Keeps the debouncer (its OS watch + worker thread) alive for the app's
/// lifetime. A module static — not an `AppState` field — mirrors the
/// `state::APP_HANDLE` rationale and keeps it out of the test constructor.
static WATCHER: OnceLock<Mutex<Option<Debouncer<RecommendedWatcher>>>> = OnceLock::new();

const DEBOUNCE_MS: u64 = 500;

/// (Re)create the watcher for the currently-configured sync folder. Idempotent:
/// the previous debouncer is dropped (stopping the old watch) before the new one
/// is stored, so calling this on startup and again on every sync-folder change
/// never leaks or double-watches. Best-effort: returns `Ok(())` when there is no
/// folder to watch, and any watcher failure is surfaced for the caller to log.
pub(crate) fn arm_watcher(state: &AppState) -> AppResult<()> {
    let Some(folder) = state.db.get_sync_folder()? else {
        return Ok(());
    };
    if folder.trim().is_empty() {
        return Ok(());
    }
    let root = PathBuf::from(&folder);
    if !root.is_dir() {
        return Ok(());
    }
    // canonicalize() yields the on-disk form (Windows: a `\\?\` verbatim prefix
    // and the real case). We strip events against BOTH this and the configured
    // root because a *deleted* path can't be re-canonicalized (see map_event_path).
    let canon_root = root.canonicalize().unwrap_or_else(|_| root.clone());

    let cb_state = state.clone();
    let cb_canon = canon_root.clone();
    let cb_configured = root.clone();
    let mut debouncer = new_debouncer(
        Duration::from_millis(DEBOUNCE_MS),
        move |res: DebounceEventResult| match res {
            Ok(events) => {
                let paths: Vec<PathBuf> = events.into_iter().map(|e| e.path).collect();
                on_debounced_batch(&cb_state, &cb_canon, &cb_configured, paths);
            }
            Err(error) => {
                tracing::warn!(error = %error, "fs watcher error");
            }
        },
    )
    .map_err(|e| AppError::Other(format!("fs watcher init failed: {e}")))?;

    debouncer
        .watcher()
        .watch(&root, RecursiveMode::Recursive)
        .map_err(|e| AppError::Other(format!("fs watch failed: {e}")))?;

    let slot = WATCHER.get_or_init(|| Mutex::new(None));
    let mut guard = slot
        .lock()
        .map_err(|_| AppError::Other("fs watcher lock poisoned".to_string()))?;
    // Dropping any previous debouncer here stops the old OS watch + thread.
    *guard = Some(debouncer);
    tracing::info!(root = %root.display(), "filesystem watcher armed");
    Ok(())
}

/// Handle a debounced batch: map paths, then run targeted processing + drain on
/// a worker thread (keeping the debouncer thread responsive), under the sync gate.
fn on_debounced_batch(
    state: &AppState,
    canon_root: &Path,
    configured_root: &Path,
    paths: Vec<PathBuf>,
) {
    let mut rels: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for abs in &paths {
        if let Some(rel) = map_event_path(canon_root, configured_root, abs) {
            if seen.insert(rel.clone()) {
                rels.push(rel);
            }
        }
    }
    if rels.is_empty() {
        return;
    }

    let st = state.clone();
    std::thread::spawn(move || {
        // If a scan/tick already owns the sync, skip: the periodic fallback or the
        // next batch will pick these paths up. Also prevents reacting to the sync
        // engine's own writes (downloads/hydration), which hold the gate.
        if st
            .sync_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        crate::auth_session::refresh_tray_tooltip(&st);
        match crate::sync_pipeline::process_changed_paths(&st, &rels) {
            Ok(n) if n > 0 => tracing::info!(count = n, "watcher enqueued targeted change(s)"),
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "process_changed_paths failed"),
        }
        crate::sync_pipeline::drain_sync_queue(&st);
        st.sync_running.store(false, Ordering::Release);
        crate::auth_session::refresh_tray_tooltip(&st);
    });
}

/// Strip a leading Windows `\\?\` verbatim prefix so verbatim and non-verbatim
/// forms compare equal.
fn normalize_verbatim(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(unc) = s.strip_prefix(r"\\?\UNC\") {
        // `\\?\UNC\server\share` → `\\server\share` (network-share sync root).
        PathBuf::from(format!(r"\\{unc}"))
    } else if let Some(stripped) = s.strip_prefix(r"\\?\") {
        PathBuf::from(stripped)
    } else {
        p.to_path_buf()
    }
}

/// Map an absolute event path to a `/`-relative path under the sync root, or
/// `None` when it is outside the root or should be ignored (`.cloudsc`, dotfile
/// junk, editor/temp files, or a path that fails relative-path validation).
/// Strips against both the canonical and configured roots (verbatim-normalized)
/// to survive the Windows `\\?\`/case mismatch and deleted (un-canonicalizable)
/// paths.
pub(crate) fn map_event_path(
    canon_root: &Path,
    configured_root: &Path,
    abs: &Path,
) -> Option<String> {
    let abs_n = normalize_verbatim(abs);
    let canon_n = normalize_verbatim(canon_root);
    let conf_n = normalize_verbatim(configured_root);

    let rel = abs_n
        .strip_prefix(&canon_n)
        .ok()
        .or_else(|| abs_n.strip_prefix(&conf_n).ok())?;

    let rel = rel.to_string_lossy().replace('\\', "/");
    let rel = rel.trim_start_matches('/').to_string();
    if rel.is_empty()
        || rel.ends_with(".cloudsc")
        || should_ignore_local_path(&rel)
        || is_editor_temp_path(&rel)
    {
        return None;
    }
    if validate_relative(&rel).is_err() {
        return None;
    }
    Some(rel)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::map_event_path;

    #[test]
    fn maps_a_file_under_the_root_to_a_forward_slash_relative() {
        let root = Path::new("/sync/root");
        assert_eq!(
            map_event_path(root, root, Path::new("/sync/root/Cocina/Pizza.txt")),
            Some("Cocina/Pizza.txt".to_string())
        );
    }

    #[test]
    fn root_itself_maps_to_none() {
        let root = Path::new("/sync/root");
        assert_eq!(map_event_path(root, root, Path::new("/sync/root")), None);
    }

    #[test]
    fn path_outside_root_maps_to_none() {
        let root = Path::new("/sync/root");
        assert_eq!(
            map_event_path(root, root, Path::new("/etc/passwd")),
            None
        );
    }

    #[test]
    fn cloudsc_ignored_and_temp_paths_map_to_none() {
        let root = Path::new("/sync/root");
        assert_eq!(
            map_event_path(root, root, Path::new("/sync/root/a.txt.cloudsc")),
            None
        );
        assert_eq!(
            map_event_path(root, root, Path::new("/sync/root/.DS_Store")),
            None
        );
        assert_eq!(
            map_event_path(root, root, Path::new("/sync/root/doc.txt.tmp")),
            None
        );
        assert_eq!(
            map_event_path(root, root, Path::new("/sync/root/~$report.docx")),
            None
        );
    }

    #[test]
    fn falls_back_to_configured_root_when_canonical_differs() {
        // Simulate a canonical root the event path does not match (e.g. case /
        // verbatim), but the configured root does.
        let canon = Path::new("/sync/ROOT-canon");
        let configured = Path::new("/sync/root");
        assert_eq!(
            map_event_path(canon, configured, Path::new("/sync/root/a.txt")),
            Some("a.txt".to_string())
        );
    }
}
