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
use crate::path_util::{is_ignored_local_path, relpath_under, validate_relative};
use crate::state::AppState;

/// Keeps the debouncer (its OS watch + worker thread) alive for the app's
/// lifetime. A module static — not an `AppState` field — mirrors the
/// `state::APP_HANDLE` rationale and keeps it out of the test constructor.
static WATCHER: OnceLock<Mutex<Option<Debouncer<RecommendedWatcher>>>> = OnceLock::new();

/// Paths from batches that were dropped because the sync gate was held (DBSYNC-106).
///
/// Correlation of renames lives ONLY in `sync_pipeline::process_changed_paths`, which only
/// the watcher reaches. A dropped batch is therefore a lost rename: the periodic scan does
/// not correlate, and FSEvents does not redeliver. These paths are unioned into the next
/// batch that does win the gate, and drained by the periodic tick if no further filesystem
/// event ever arrives.
static PENDING: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

/// Cap on [`PENDING`]. Past this, paths are dropped and said to be dropped: a bounded miss
/// that announces itself beats a buffer that grows without limit under a contended gate.
const PENDING_MAX: usize = 4096;

const DEBOUNCE_MS: u64 = 500;

/// Serialises tests that touch [`PENDING`], directly OR indirectly.
///
/// The set is process-wide by design — it survives across watcher batches — so tests in any
/// module that drain or fill it race each other under the default parallel harness.
///
/// **Take it with [`pending_test_guard`], never by locking this directly.** Review of PR #167
/// found ten tests holding it and four more reaching `take_pending` *indirectly*, through
/// `run_sync_tick_internal` → `scan_local_changes_internal` → `drain_deferred_watcher_paths`,
/// none of which took it. The suite was green only because the real window is microseconds;
/// widening it by 800 ms turned one of them red. "It passes" was scheduling luck twice over.
///
/// # Not `#[cfg(test)]`, deliberately
///
/// This and [`pending_test_guard`] exist only for tests, but they are **not** gated on
/// `cfg(test)`, because a `#[cfg(test)]` item used from another module's tests breaks the
/// Windows test binary: it fails to load with `STATUS_ENTRYPOINT_NOT_FOUND` before a single
/// test runs, naming nothing. Bisected on CI twice (DBSYNC-106) — once for
/// `sync_pipeline::tests` being made `pub(crate)` so this module could reach its
/// `build_state`, and again for this guard, which is the same hazard in the other direction.
///
/// The cost is one `Mutex<()>` in the release binary. Do not "tidy" this by adding the
/// attribute back.
pub(crate) static PENDING_TEST_GUARD: Mutex<()> = Mutex::new(());

/// Take [`PENDING_TEST_GUARD`], surviving poisoning.
///
/// `Mutex::lock` returns a `Result`, and `let _g = GUARD.lock();` binds the `Err` on a
/// poisoned mutex — **holding no lock at all**. A `Mutex<()>` is poisoned by a panic in any
/// guarded test, i.e. by any failure of these tests, so the first honest red would silently
/// de-serialise every other one and turn a single failure into a cascade. There is no state
/// behind this mutex to be corrupted by a panic, so recovering the guard is always right.
#[allow(dead_code)] // test-only caller; see PENDING_TEST_GUARD for why it is not cfg(test)
pub(crate) fn pending_test_guard() -> std::sync::MutexGuard<'static, ()> {
    PENDING_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner())
}

fn pending_cell() -> &'static Mutex<HashSet<String>> {
    PENDING.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Remember a batch the gate refused.
///
/// **The batch is the unit, all or nothing.** An earlier version truncated mid-batch with
/// `take(room)`, which was wrong twice over (review of PR #167): it kept the OLDEST
/// deferrals and discarded the rename the user had just performed, and truncating mid-batch
/// could record a rename by halves — source kept, destination dropped. On the next drain
/// that source is a vanished folder with no candidate destination, so it is propagated as a
/// real recursive remote `delete_v2` while the destination is uploaded in full by the same
/// tick's scan. It converges, but through a destructive operation and a full re-upload, for
/// no benefit over dropping the batch.
///
/// Refusing the whole batch keeps the invariant the doc on [`PENDING`] claims: a remembered
/// batch is complete.
///
/// Best-effort on a poisoned lock: the paths are lost rather than panicking the debouncer
/// thread, and it says so.
pub(crate) fn remember_dropped(rels: &[String]) {
    let Ok(mut guard) = pending_cell().lock() else {
        tracing::error!("pending-rename lock poisoned; dropped batch is lost");
        return;
    };
    // Count what this batch would actually ADD: a path already held costs no room, so a
    // batch that is mostly re-deferrals is not refused for nothing.
    let new_paths = rels.iter().filter(|r| !guard.contains(*r)).count();
    if guard.len() + new_paths > PENDING_MAX {
        tracing::warn!(
            held = guard.len(),
            batch = rels.len(),
            cap = PENDING_MAX,
            "pending-rename set would overflow; dropping this batch WHOLE. A rename may be missed"
        );
        return;
    }
    for rel in rels {
        guard.insert(rel.clone());
    }
}

/// Take everything remembered by [`remember_dropped`], leaving the set empty.
pub(crate) fn take_pending() -> Vec<String> {
    match pending_cell().lock() {
        Ok(mut guard) => guard.drain().collect(),
        Err(_) => {
            tracing::error!("pending-rename lock poisoned; cannot drain");
            Vec::new()
        }
    }
}

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
        // If a scan/tick already owns the sync, skip. Also prevents reacting to the sync
        // engine's own writes (downloads/hydration), which hold the gate.
        //
        // **Dropping the batch is not free, and an earlier comment here claimed it was.**
        // It said "the periodic fallback or the next batch will pick these paths up", and
        // both halves are false for a RENAME (DBSYNC-106): the periodic fallback is
        // `scan_local_changes_only`, which does not correlate at all — it uploads what is
        // not indexed and propagates the old prefix as deletions, which is precisely the
        // reported symptom — and FSEvents does not redeliver, so the "next batch" never
        // carries the rename. Correlation lives only in `process_changed_paths`.
        if st
            .sync_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            tracing::warn!(
                count = rels.len(),
                paths = ?rels,
                "watcher batch deferred: the sync gate was held. Held for the next batch \
                 or the next periodic tick"
            );
            remember_dropped(&rels);
            return;
        }
        // Gated out of `cfg(test)` for exactly the reason `cloud_filter.rs` documents at
        // length: a test that calls `on_debounced_batch` makes this call chain statically
        // reachable in the `cargo test --lib` harness binary, which — unlike the shipped
        // `.exe` — has no embedded manifest requesting ComCtl32 v6. The `tray-icon`/`muda`
        // Windows backend statically imports `TaskDialogIndirect`, a ComCtl32 v6-only
        // export, so the harness fails to LOAD at all: `STATUS_ENTRYPOINT_NOT_FOUND`, before
        // a single test runs, naming nothing (DBSYNC-106).
        //
        // The tooltip is UI-only and not correctness-load-bearing here, so skipping it under
        // test costs nothing. The shipped app is unaffected — it has the real manifest.
        #[cfg(not(test))]
        crate::auth_session::refresh_tray_tooltip(&st);
        // Union in anything a previous batch lost to the gate, so a deferred rename is
        // correlated by the first batch that gets through (DBSYNC-106).
        let mut rels = rels;
        let deferred = take_pending();
        if !deferred.is_empty() {
            tracing::info!(
                count = deferred.len(),
                "re-delivering deferred watcher paths"
            );
            let already: HashSet<&str> = rels.iter().map(String::as_str).collect();
            let extra: Vec<String> = deferred
                .iter()
                .filter(|d| !already.contains(d.as_str()))
                .cloned()
                .collect();
            rels.extend(extra);
        }
        match crate::sync_pipeline::process_changed_paths(&st, &rels) {
            Ok(n) if n > 0 => tracing::info!(count = n, "watcher enqueued targeted change(s)"),
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "process_changed_paths failed"),
        }
        crate::sync_pipeline::drain_sync_queue(&st);
        st.sync_running.store(false, Ordering::Release);
        #[cfg(not(test))]
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

    let rel = relpath_under(&canon_n, &abs_n)
        .or_else(|_| relpath_under(&conf_n, &abs_n))
        .ok()?;

    let rel = rel.trim_start_matches('/').to_string();
    if rel.is_empty() || rel.ends_with(".cloudsc") || is_ignored_local_path(&rel) {
        return None;
    }
    if validate_relative(&rel).is_err() {
        return None;
    }
    Some(rel)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use super::{on_debounced_batch, remember_dropped, take_pending, PENDING_MAX};

    /// A minimal `AppState` on a temp database.
    ///
    /// **Deliberately duplicated rather than borrowed from `sync_pipeline::tests`.** Making
    /// that module `pub(crate)` so this one could reach its `build_state` was measured to
    /// break the Windows test binary outright: `rust (windows-latest)` failed with
    /// `STATUS_ENTRYPOINT_NOT_FOUND` before a single test ran, while `windows-app` — the
    /// production binary, which has no test code — passed. Bisected to exactly this change
    /// (DBSYNC-106): removing it turned Windows green with everything else identical.
    ///
    /// Twenty duplicated lines are cheaper than a cross-module `#[cfg(test)]` dependency
    /// that only one platform rejects, and rejects in a way that names no test.
    fn build_state(root: &std::path::Path) -> crate::state::AppState {
        use std::sync::atomic::AtomicBool;
        use std::sync::{Arc, Mutex};

        let sync_folder = root.join("synced");
        std::fs::create_dir_all(&sync_folder).expect("create sync folder");
        let db_path = root.join("db").join("app.db");
        std::fs::create_dir_all(db_path.parent().unwrap()).expect("create db dir");
        let db = crate::storage::db::Db::new_at(&db_path).expect("db init");
        db.set_sync_folder(&sync_folder.to_string_lossy())
            .expect("set sync folder");
        crate::state::AppState {
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

    /// **The wiring test.** The three tests below exercise the pending set directly, which
    /// leaves the one line that matters in production — `remember_dropped(&rels)` inside
    /// `on_debounced_batch` — unasserted. Deleting it restored the original defect with the
    /// whole suite still green, so it gets a test of its own.
    ///
    /// Holds the gate, hands the watcher a batch, and asserts the paths were kept rather
    /// than thrown away.
    #[test]
    fn a_batch_that_loses_the_gate_is_remembered_not_discarded() {
        let _serial = super::pending_test_guard();
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("synced");
        std::fs::create_dir_all(root.join("Docs")).expect("mkdir");
        std::fs::write(root.join("Docs/a.txt"), b"x").expect("write");

        let state = build_state(tmp.path());
        let _ = take_pending();

        // Somebody else owns the sync, exactly as a longpoll tick would.
        state.sync_running.store(true, Ordering::Release);

        on_debounced_batch(&state, &root, &root, vec![root.join("Docs/a.txt")]);

        // `on_debounced_batch` does its work on a spawned thread, so poll rather than sleep
        // a fixed amount.
        let deadline = Instant::now() + Duration::from_secs(5);
        let held = loop {
            let got = take_pending();
            if !got.is_empty() || Instant::now() > deadline {
                break got;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        state.sync_running.store(false, Ordering::Release);

        assert_eq!(
            held,
            vec!["Docs/a.txt".to_string()],
            "a batch refused by the gate must be held for re-delivery, not dropped"
        );
    }

    /// The pending set exists because a dropped batch is a lost rename: correlation lives
    /// only in `process_changed_paths`, the periodic scan does not correlate, and FSEvents
    /// does not redeliver (DBSYNC-106).
    #[test]
    fn a_deferred_batch_is_returned_once_and_then_forgotten() {
        let _serial = super::pending_test_guard();
        let _ = take_pending(); // other tests share the process-wide set

        remember_dropped(&["a/b.txt".to_string(), "a".to_string()]);
        let mut got = take_pending();
        got.sort();
        assert_eq!(got, vec!["a".to_string(), "a/b.txt".to_string()]);

        assert!(
            take_pending().is_empty(),
            "draining must leave the set empty, or paths are replayed forever"
        );
    }

    /// Two batches deferred in a row are unioned, not overwritten: a rename's old and new
    /// paths can arrive in different batches.
    #[test]
    fn consecutive_deferrals_accumulate() {
        let _serial = super::pending_test_guard();
        let _ = take_pending();

        remember_dropped(&["old".to_string()]);
        remember_dropped(&["new".to_string()]);
        let mut got = take_pending();
        got.sort();
        assert_eq!(got, vec!["new".to_string(), "old".to_string()]);
    }

    /// The same path deferred twice is held once. Without the set semantics this grows
    /// linearly under a contended gate.
    #[test]
    fn a_repeated_path_is_held_once() {
        let _serial = super::pending_test_guard();
        let _ = take_pending();

        remember_dropped(&["same".to_string()]);
        remember_dropped(&["same".to_string()]);
        assert_eq!(take_pending(), vec!["same".to_string()]);
    }

    /// The bound is what makes this safe to add: a gate held for a long time must not grow a
    /// buffer without limit. **The unit is the batch, not the path** — an earlier version
    /// truncated mid-batch, which kept the oldest deferrals and discarded the rename just
    /// performed, and could record a rename by halves: source kept, destination dropped. That
    /// half-record turns into a real recursive remote delete on the next drain.
    #[test]
    fn an_oversized_batch_is_refused_whole_rather_than_truncated() {
        let _serial = super::pending_test_guard();
        let _ = take_pending();

        let too_many: Vec<String> = (0..PENDING_MAX + 1).map(|i| format!("f{i}")).collect();
        remember_dropped(&too_many);
        assert!(
            take_pending().is_empty(),
            "a batch that cannot fit must be refused whole, leaving no half-recorded rename"
        );

        // One that fits is kept entire.
        let fits: Vec<String> = (0..10).map(|i| format!("g{i}")).collect();
        remember_dropped(&fits);
        assert_eq!(take_pending().len(), 10);
    }

    /// A batch of paths already held costs no room, so a re-deferral is not refused for
    /// nothing once the set is near its cap.
    #[test]
    fn re_deferring_paths_already_held_does_not_count_against_the_cap() {
        let _serial = super::pending_test_guard();
        let _ = take_pending();

        let full: Vec<String> = (0..PENDING_MAX).map(|i| format!("h{i}")).collect();
        remember_dropped(&full);
        // The very same batch again: nothing new, so it must be accepted, not refused.
        remember_dropped(&full);
        assert_eq!(take_pending().len(), PENDING_MAX);
    }

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
        assert_eq!(map_event_path(root, root, Path::new("/etc/passwd")), None);
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
