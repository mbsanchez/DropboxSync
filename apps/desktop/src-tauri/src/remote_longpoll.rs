//! Dropbox longpoll loop (DBSYNC-30): the remote counterpart of the DBSYNC-29
//! filesystem watcher. Blocks on `/2/files/list_folder/longpoll` and applies
//! cursor deltas the moment the remote changes, instead of a periodic full
//! re-list. One background thread; the 5-min sweep stays as reconciliation
//! fallback. Work is gated by the shared `sync_running` CAS.

use std::sync::atomic::Ordering;
use std::sync::OnceLock;
use std::time::Duration;

use crate::error::{AppError, AppResult};
use crate::models::DropboxLongpollResponse;
use crate::state::AppState;

/// Longpoll block time (seconds). Dropbox adds up to 90s of jitter on top.
const LONGPOLL_TIMEOUT_SECS: u64 = 30;
/// Per-request HTTP timeout for the longpoll: block + jitter + slack. The shared
/// client's 30s default would abort this long-blocking call, so we override it.
const LONGPOLL_HTTP_TIMEOUT_SECS: u64 = LONGPOLL_TIMEOUT_SECS + 90 + 15;
/// Idle sleep when not logged in / no sync folder configured yet.
const IDLE_SLEEP_SECS: u64 = 15;
const MIN_BACKOFF_SECS: u64 = 5;
const MAX_BACKOFF_SECS: u64 = 60;
/// When a change is pending but another sync owns the gate, wait this long
/// before re-longpolling, so we don't spin issuing back-to-back requests
/// (which would return `changes:true` instantly) until the other sync frees it.
const GATE_BUSY_SLEEP_SECS: u64 = 3;

/// Single-instance guard — the loop is started once from `lib.rs setup()`.
static LONGPOLL_STARTED: OnceLock<()> = OnceLock::new();

/// Start the background longpoll loop (idempotent; a second call is a no-op).
pub(crate) fn start_longpoll(state: &AppState) {
    if LONGPOLL_STARTED.set(()).is_err() {
        return;
    }
    let st = state.clone();
    std::thread::spawn(move || longpoll_loop(&st));
}

/// Exponential error backoff: 0 → 5s → 10s → … capped at 60s.
fn next_backoff(prev: u64) -> u64 {
    if prev == 0 {
        MIN_BACKOFF_SECS
    } else {
        (prev * 2).min(MAX_BACKOFF_SECS)
    }
}

fn longpoll_loop(state: &AppState) {
    let mut err_backoff = 0u64;
    loop {
        // 1. Ready check: a sync folder AND a usable token. Handles logout —
        // get_access_token errs → idle; a later login resumes the loop.
        let folder_ok = state
            .db
            .get_sync_folder()
            .ok()
            .flatten()
            .map(|f| !f.trim().is_empty())
            .unwrap_or(false);
        if !folder_ok || crate::auth_session::get_access_token(state).is_err() {
            std::thread::sleep(Duration::from_secs(IDLE_SLEEP_SECS));
            continue;
        }

        // 2. Ensure a cursor exists (seed a snapshot if not).
        let cursor = match ensure_cursor(state) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "longpoll: failed to obtain remote cursor");
                err_backoff = next_backoff(err_backoff);
                std::thread::sleep(Duration::from_secs(err_backoff));
                continue;
            }
        };
        let Some(cursor) = cursor else {
            // Just seeded (no changes to poll yet); loop to longpoll next round.
            continue;
        };

        // 3. Longpoll (unauthenticated, long-blocking).
        match do_longpoll(state, &cursor) {
            Ok(resp) => {
                err_backoff = 0;
                if resp.changes && !apply_and_drain(state) {
                    // Another sync owns the gate; the delta wasn't applied and the
                    // cursor didn't advance. Pause so we don't hot-loop re-longpolling
                    // the still-pending change until that sync finishes.
                    std::thread::sleep(Duration::from_secs(GATE_BUSY_SLEEP_SECS));
                }
                if let Some(secs) = resp.backoff {
                    std::thread::sleep(Duration::from_secs(secs));
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "longpoll request failed; backing off");
                err_backoff = next_backoff(err_backoff);
                std::thread::sleep(Duration::from_secs(err_backoff));
            }
        }
    }
}

/// Return the current delta cursor, seeding a fresh snapshot if none exists.
/// Returns `Ok(None)` when it had to seed this round (caller loops again).
fn ensure_cursor(state: &AppState) -> AppResult<Option<String>> {
    match state
        .db
        .get_app_config(crate::remote_index::REMOTE_DELTA_CURSOR_KEY)?
    {
        Some(c) if !c.is_empty() => Ok(Some(c)),
        _ => {
            crate::remote_index::seed_remote_delta_cursor(state)?;
            Ok(None)
        }
    }
}

fn do_longpoll(state: &AppState, cursor: &str) -> AppResult<DropboxLongpollResponse> {
    // The longpoll endpoint is on the `notify` host and takes NO Authorization
    // header (auth = noauth); only `continue` (in apply_remote_delta) is authed.
    let response = state
        .http_client
        .post("https://notify.dropboxapi.com/2/files/list_folder/longpoll")
        .timeout(Duration::from_secs(LONGPOLL_HTTP_TIMEOUT_SECS))
        .json(&serde_json::json!({ "cursor": cursor, "timeout": LONGPOLL_TIMEOUT_SECS }))
        .send()
        .map_err(|e| AppError::Network(format!("longpoll request failed: {e}")))?;

    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body = response
            .text()
            .unwrap_or_else(|_| "<unreadable body>".to_string());
        if crate::remote_index::is_reset_error(status, &body) {
            // Invalidated cursor: clear it so the next iteration reseeds.
            let _ = state
                .db
                .set_app_config(crate::remote_index::REMOTE_DELTA_CURSOR_KEY, "");
            return Ok(DropboxLongpollResponse {
                changes: false,
                backoff: None,
            });
        }
        return Err(AppError::Dropbox {
            status,
            message: format!("longpoll: {body}"),
        });
    }

    response
        .json::<DropboxLongpollResponse>()
        .map_err(|e| AppError::Other(format!("longpoll parse failed: {e}")))
}

/// Apply the remote delta + drain, under the shared single-flight gate. Returns
/// `false` if another sync owned the gate (nothing applied — caller should pause
/// before re-longpolling the still-pending change).
fn apply_and_drain(state: &AppState) -> bool {
    if state
        .sync_running
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        // A scan/tick/watcher already owns the sync; the next longpoll or the
        // 5-min sweep reconciles. No double-drain.
        return false;
    }
    crate::auth_session::refresh_tray_tooltip(state);
    match crate::remote_index::apply_remote_delta(state) {
        Ok(n) if n > 0 => tracing::info!(enqueued = n, "longpoll applied remote delta"),
        Ok(_) => {}
        Err(e) => tracing::error!(error = %e, "apply_remote_delta failed"),
    }
    crate::sync_pipeline::drain_sync_queue(state);
    state.sync_running.store(false, Ordering::Release);
    crate::auth_session::refresh_tray_tooltip(state);
    true
}

#[cfg(test)]
mod tests {
    use super::{next_backoff, MAX_BACKOFF_SECS, MIN_BACKOFF_SECS};
    use crate::models::DropboxLongpollResponse;

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(next_backoff(0), MIN_BACKOFF_SECS);
        assert_eq!(next_backoff(5), 10);
        assert_eq!(next_backoff(40), MAX_BACKOFF_SECS);
        assert_eq!(next_backoff(MAX_BACKOFF_SECS), MAX_BACKOFF_SECS);
    }

    #[test]
    fn longpoll_response_deserializes_both_shapes() {
        let with: DropboxLongpollResponse =
            serde_json::from_str(r#"{"changes":true,"backoff":10}"#).unwrap();
        assert!(with.changes);
        assert_eq!(with.backoff, Some(10));

        let without: DropboxLongpollResponse =
            serde_json::from_str(r#"{"changes":false}"#).unwrap();
        assert!(!without.changes);
        assert_eq!(without.backoff, None);
    }
}
