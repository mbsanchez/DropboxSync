use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use chrono::{Duration, Utc};
use zeroize::Zeroizing;

use crate::auth::oauth::refresh_access_token_blocking;
use crate::error::{AppError, AppResult};
use crate::state::AppState;
use crate::storage::secure_store::TokenSession;

pub(crate) fn has_stored_credentials(state: &AppState) -> bool {
    state.secure_store.get_session().is_ok() || state.secure_store.get_token().is_ok()
}

/// Returns true when we should exchange the refresh token for a new access token.
fn session_needs_refresh(session: &TokenSession) -> bool {
    if let Some(expires_at) = session.expires_at.as_ref() {
        if let Ok(expires_dt) = chrono::DateTime::parse_from_rfc3339(expires_at) {
            let expires_dt = expires_dt.with_timezone(&Utc);
            if Utc::now() + Duration::seconds(60) >= expires_dt {
                return true;
            }
            return false;
        }
    }
    // Missing or unparseable expires_at: Dropbox access tokens are short-lived (~4 hours).
    // If we have a refresh token, refresh instead of reusing a possibly stale access_token.
    session.refresh_token.is_some()
}

/// Serializes concurrent token refreshes: only one caller runs `refresher` per
/// staleness window; losers re-check the cache after acquiring the lock and reuse
/// whatever the winner just stored, instead of racing Dropbox's token endpoint.
///
/// `force` skips the cache short-circuit: a forced refresh (401 recovery) must
/// exchange the refresh token even when the cache holds a token that is still
/// time-fresh but server-revoked. It still serializes on the refresh mutex, so
/// concurrent forced refreshes never race Dropbox's endpoint (they run one at a
/// time; a loser reading an already-rotated refresh token simply fails and the
/// next tick reuses the winner's valid cached token).
fn refresh_token_guarded(
    token_cache: &Arc<Mutex<Option<TokenSession>>>,
    token_refresh_lock: &Arc<Mutex<()>>,
    force: bool,
    refresher: impl FnOnce() -> AppResult<TokenSession>,
) -> AppResult<TokenSession> {
    // A `Mutex<()>` guards no invariant, so a prior panic while holding it cannot
    // corrupt anything — recover from poison rather than permanently wedging every
    // future refresh with "lock poisoned".
    let _guard = token_refresh_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    // Double-check: a peer may have refreshed while we waited for the lock. Skipped
    // for a forced refresh, which must not reuse a possibly-revoked cached token.
    if !force {
        if let Ok(cache) = token_cache.lock() {
            if let Some(session) = cache.as_ref() {
                if !session_needs_refresh(session) {
                    return Ok(session.clone());
                }
            }
        }
    }

    let session = refresher()?;

    if let Ok(mut cache) = token_cache.lock() {
        *cache = Some(session.clone());
    }

    Ok(session)
}

/// Clears the in-memory cache and performs a refresh using the session from the keychain.
pub(crate) fn force_refresh_session(state: &AppState) -> AppResult<TokenSession> {
    if let Ok(mut c) = state.token_cache.lock() {
        *c = None;
    }

    let session = state
        .secure_store
        .get_session()
        .map_err(|_| AppError::Auth("missing dropbox token session".to_string()))?;

    let Some(refresh_token) = session.refresh_token.clone() else {
        return Err(AppError::Auth(
            "dropbox token expired and no refresh_token available".to_string(),
        ));
    };

    refresh_token_guarded(&state.token_cache, &state.token_refresh_lock, true, || {
        tracing::info!(forced = true, "refreshing dropbox access token");
        let refreshed = refresh_access_token_blocking(&state.http_client, &refresh_token)?;

        let new_refresh_token = refreshed
            .refresh_token
            .clone()
            .or_else(|| session.refresh_token.clone());

        let new_expires_at = refreshed
            .expires_in
            .map(|in_s| (Utc::now() + Duration::seconds(in_s)).to_rfc3339());

        let new_session = TokenSession {
            access_token: refreshed.access_token,
            refresh_token: new_refresh_token,
            expires_at: new_expires_at.or_else(|| session.expires_at.clone()),
        };

        state
            .secure_store
            .store_session(&new_session)
            .map_err(|e| AppError::Auth(format!("failed storing refreshed token session: {e}")))?;

        Ok(new_session)
    })
}

pub(crate) fn get_access_token(state: &AppState) -> AppResult<Zeroizing<String>> {
    if let Ok(cache) = state.token_cache.lock() {
        if let Some(session) = cache.as_ref() {
            if !session_needs_refresh(session) {
                return Ok(session.access_token.clone());
            }
        }
    }

    let session = match state.secure_store.get_session() {
        Ok(s) => s,
        Err(_) => {
            let legacy_token = state
                .secure_store
                .get_token()
                .map_err(|e| AppError::Auth(format!("missing dropbox token session: {e}")))?;
            let migrated = TokenSession {
                access_token: Zeroizing::new(legacy_token),
                refresh_token: None,
                expires_at: None,
            };
            let _ = state.secure_store.store_session(&migrated);
            migrated
        }
    };

    if !session_needs_refresh(&session) {
        if let Ok(mut cache) = state.token_cache.lock() {
            *cache = Some(session.clone());
        }
        return Ok(session.access_token);
    }

    let Some(refresh_token) = session.refresh_token.clone() else {
        return Err(AppError::Auth(
            "dropbox token expired and no refresh_token available".to_string(),
        ));
    };

    let refreshed_session =
        refresh_token_guarded(&state.token_cache, &state.token_refresh_lock, false, || {
            tracing::info!(forced = false, "refreshing dropbox access token");
            let refreshed = refresh_access_token_blocking(&state.http_client, &refresh_token)?;

            let new_refresh_token = refreshed
                .refresh_token
                .clone()
                .or_else(|| session.refresh_token.clone());

            let new_expires_at = refreshed
                .expires_in
                .map(|in_s| (Utc::now() + Duration::seconds(in_s)).to_rfc3339());

            let new_session = TokenSession {
                access_token: refreshed.access_token,
                refresh_token: new_refresh_token,
                expires_at: new_expires_at.or_else(|| session.expires_at.clone()),
            };

            state
                .secure_store
                .store_session(&new_session)
                .map_err(|e| {
                    AppError::Auth(format!("failed storing refreshed token session: {e}"))
                })?;

            Ok(new_session)
        })?;

    Ok(refreshed_session.access_token)
}

pub(crate) fn verify_dropbox_token_internal(state: &AppState) -> AppResult<bool> {
    fn probe(
        client: &reqwest::blocking::Client,
        token: &str,
    ) -> Result<reqwest::blocking::Response, ()> {
        client
            .post("https://api.dropboxapi.com/2/users/get_current_account")
            .bearer_auth(token)
            .json(&serde_json::json!({}))
            .send()
            .map_err(|_| ())
    }

    let token = get_access_token(state)?;
    let response = match probe(&state.http_client, &token) {
        Ok(r) => r,
        Err(()) => return Ok(true),
    };

    if response.status().is_success() {
        return Ok(true);
    }

    if response.status().as_u16() == 401 {
        if let Ok(mut c) = state.token_cache.lock() {
            *c = None;
        }
        return match force_refresh_session(state) {
            Ok(session) => {
                let token2 = session.access_token;
                match probe(&state.http_client, &token2) {
                    Ok(r2) if r2.status().is_success() => Ok(true),
                    Ok(r2) if r2.status().as_u16() == 401 => Ok(false),
                    Ok(_) => Ok(true),
                    Err(()) => Ok(true),
                }
            }
            Err(e) => {
                if e.is_hard_auth() {
                    Ok(false)
                } else {
                    Ok(true)
                }
            }
        };
    }

    Ok(true)
}

pub(crate) fn current_tray_status_label(state: &AppState) -> String {
    if state.sync_running.load(Ordering::Acquire) {
        return "Syncing".to_string();
    }
    if let Ok(engine) = state.sync_engine.lock() {
        if engine.current_status(false).last_error.is_some() {
            return "Error".to_string();
        }
    }
    "Idle".to_string()
}

/// Reflect the current sync state in BOTH the tray tooltip and the tray ICON
/// (DBSYNC-32). Idle keeps the template-tinted brand icon; Syncing/Error swap to the
/// colored cloud status icons (`icon_as_template(false)` so macOS doesn't flatten
/// their colour to a menu-bar silhouette). Called at every `sync_running`/error
/// transition via [`refresh_tray_tooltip`], so the icon tracks state live.
pub(crate) fn update_tray_tooltip(app: &tauri::AppHandle, label: &str) {
    let Some(tray) = app.tray_by_id("main") else {
        return;
    };
    let _ = tray.set_tooltip(Some(format!("DropboxSyncDesktop - {label}")));

    // Same blue brand icon in every state — only the inner glyph changes (check /
    // sync-arrows / exclamation). `icon_as_template(false)` keeps the blue on macOS.
    let bytes: &[u8] = match label {
        "Syncing" => include_bytes!("icons/tray-sync.png"),
        "Error" => include_bytes!("icons/tray-error.png"),
        _ => include_bytes!("icons/tray-idle.png"),
    };
    if let Ok(img) = tauri::image::Image::from_bytes(bytes) {
        let _ = tray.set_icon(Some(img));
        let _ = tray.set_icon_as_template(false);
    }
}

/// Refreshes the tray tooltip to reflect the current sync status. Uses the
/// globally-stored `AppHandle` (set in `setup()`), so background sync threads —
/// which don't carry an `AppHandle` — can refresh it right at each
/// `sync_running` transition instead of waiting for the 60s scheduler poll,
/// which almost always misses the short "Syncing" window (DBSYNC-48).
pub(crate) fn refresh_tray_tooltip(state: &AppState) {
    if let Some(app) = crate::state::APP_HANDLE.get() {
        update_tray_tooltip(app, &current_tray_status_label(state));
    }
}

#[cfg(test)]
mod tests {
    use super::{refresh_token_guarded, TokenSession};
    use chrono::{Duration, Utc};
    use zeroize::Zeroizing;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    fn fresh_session() -> TokenSession {
        TokenSession {
            access_token: Zeroizing::new("canned-access-token".to_string()),
            refresh_token: Some(Zeroizing::new("canned-refresh-token".to_string())),
            // Far in the future so `session_needs_refresh` returns false and the
            // double-check reuses it instead of refreshing again.
            expires_at: Some((Utc::now() + Duration::hours(1)).to_rfc3339()),
        }
    }

    // Two threads both find the cache stale and race the guarded refresh; the
    // dedicated lock + double-checked cache re-read must collapse them into a
    // SINGLE invocation of the injected refresher (no concurrent Dropbox token
    // exchange / refresh-token rotation).
    #[test]
    fn concurrent_refreshers_collapse_to_a_single_refresh() {
        let cache: Arc<Mutex<Option<TokenSession>>> = Arc::new(Mutex::new(None));
        let lock: Arc<Mutex<()>> = Arc::new(Mutex::new(()));
        let calls = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let cache = Arc::clone(&cache);
                let lock = Arc::clone(&lock);
                let calls = Arc::clone(&calls);
                std::thread::spawn(move || {
                    refresh_token_guarded(&cache, &lock, false, || {
                        calls.fetch_add(1, Ordering::SeqCst);
                        // Widen the race window so the peers reliably arrive
                        // while the winner still holds the lock.
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        Ok(fresh_session())
                    })
                })
            })
            .collect();

        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "refresher must run exactly once across concurrent callers"
        );
        for r in results {
            let session = r.expect("guarded refresh should succeed");
            assert_eq!(session.access_token.as_str(), "canned-access-token");
        }
    }
}
