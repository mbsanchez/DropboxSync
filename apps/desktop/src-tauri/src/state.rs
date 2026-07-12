use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

use tauri::AppHandle;

use crate::storage::db::Db;
use crate::storage::secure_store::SecureStore;
use crate::sync::engine::SyncEngine;

/// Stops the localhost OAuth redirect listener and joins its thread (releases the TCP port).
pub(crate) struct OauthListenerControl {
    pub cancel: Arc<AtomicBool>,
    pub handle: JoinHandle<()>,
}

#[derive(Clone)]
pub(crate) struct AppState {
    pub secure_store: SecureStore,
    pub db: Arc<Db>,
    pub sync_engine: Arc<Mutex<SyncEngine>>,
    pub token_cache: Arc<Mutex<Option<crate::storage::secure_store::TokenSession>>>,
    pub scheduler_started: Arc<Mutex<bool>>,
    pub oauth_listener: Arc<Mutex<Option<OauthListenerControl>>>,
    /// Lock-free single-flight gate for sync entry points (replaces the previous
    /// mutex-guarded `bool` on `SyncEngine`, which allowed a TOCTOU window between
    /// checking and setting the flag under two separate lock acquisitions).
    pub sync_running: Arc<AtomicBool>,
    /// Serializes concurrent access-token refreshes so only one caller ever hits
    /// Dropbox's token endpoint per staleness window; see `auth_session::refresh_token_guarded`.
    pub token_refresh_lock: Arc<Mutex<()>>,
}

/// Set once in `setup()` after the Tauri `AppHandle` becomes available; read by
/// `dropbox_transfer::emit_upload_progress` to push `upload-progress` events to the
/// webview during large-file uploads. Unset (`None`) in unit tests, which never call
/// `setup()`, so progress emission no-ops safely offline.
///
/// This is intentionally a free-standing static rather than an `AppState` field.
/// Empirically, storing `tauri::AppHandle` as an `AppState` field — even wrapped in
/// `Arc<Mutex<Option<AppHandle>>>` or `Arc<OnceLock<AppHandle>>`, and even though the
/// value is never populated or read in tests — makes the `cargo test` unit-test binary
/// crash on startup with `STATUS_ENTRYPOINT_NOT_FOUND` (0xC0000139) on this toolchain
/// (rustc 1.94.1, tauri 2.10.3, Windows). Confirmed by bisection: swapping the field's
/// type to `String` (or dropping it) makes the crash disappear; moving the exact same
/// `AppHandle` value to this module-level `static` also makes it disappear, while
/// preserving the same "single set-once site" semantics.
pub(crate) static APP_HANDLE: OnceLock<AppHandle> = OnceLock::new();

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Races N threads on a single `Arc<AtomicBool>` gate using the same
    /// `compare_exchange` pattern used at every sync entry point; exactly one
    /// thread must observe success.
    #[test]
    fn compare_exchange_gate_allows_exactly_one_winner() {
        const N: usize = 16;
        let flag = Arc::new(AtomicBool::new(false));
        let successes = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..N)
            .map(|_| {
                let flag = Arc::clone(&flag);
                let successes = Arc::clone(&successes);
                std::thread::spawn(move || {
                    if flag
                        .compare_exchange(
                            false,
                            true,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        successes.fetch_add(1, Ordering::AcqRel);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().expect("thread panicked");
        }

        assert_eq!(successes.load(Ordering::Acquire), 1);
    }
}
