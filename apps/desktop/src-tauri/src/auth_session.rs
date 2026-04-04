use chrono::{Duration, Utc};
use reqwest::blocking::Client;

use crate::auth::oauth::refresh_access_token_blocking;
use crate::state::AppState;
use crate::storage::secure_store::TokenSession;

pub(crate) fn has_stored_credentials(state: &AppState) -> bool {
    state.secure_store.get_session().is_ok() || state.secure_store.get_token().is_ok()
}

pub(crate) fn is_hard_auth_failure(err: &str) -> bool {
    err.contains("dropbox token expired and no refresh_token available")
        || err.contains("missing dropbox token session")
        || err.contains("dropbox refresh token exchange failed with status")
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

/// Clears the in-memory cache and performs a refresh using the session from the keychain.
pub(crate) fn force_refresh_session(state: &AppState) -> Result<TokenSession, String> {
    if let Ok(mut c) = state.token_cache.lock() {
        *c = None;
    }

    let mut session = state
        .secure_store
        .get_session()
        .map_err(|_| "missing dropbox token session".to_string())?;

    let Some(refresh_token) = session.refresh_token.as_deref() else {
        return Err("dropbox token expired and no refresh_token available".to_string());
    };

    let refreshed = refresh_access_token_blocking(refresh_token)?;

    let new_refresh_token = refreshed
        .refresh_token
        .or_else(|| session.refresh_token.clone());

    let new_expires_at = refreshed
        .expires_in
        .map(|in_s| (Utc::now() + Duration::seconds(in_s)).to_rfc3339());

    session = TokenSession {
        access_token: refreshed.access_token,
        refresh_token: new_refresh_token,
        expires_at: new_expires_at.or_else(|| session.expires_at.clone()),
    };

    state
        .secure_store
        .store_session(&session)
        .map_err(|e| format!("failed storing refreshed token session: {e}"))?;

    if let Ok(mut cache) = state.token_cache.lock() {
        *cache = Some(session.clone());
    }

    Ok(session)
}

pub(crate) fn get_access_token(state: &AppState) -> Result<String, String> {
    if let Ok(cache) = state.token_cache.lock() {
        if let Some(session) = cache.as_ref() {
            if !session_needs_refresh(session) {
                return Ok(session.access_token.clone());
            }
        }
    }

    let mut session = match state.secure_store.get_session() {
        Ok(s) => s,
        Err(_) => {
            let legacy_token = state
                .secure_store
                .get_token()
                .map_err(|e| format!("missing dropbox token session: {e}"))?;
            let migrated = TokenSession {
                access_token: legacy_token,
                refresh_token: None,
                expires_at: None,
            };
            let _ = state.secure_store.store_session(&migrated);
            migrated
        }
    };

    if session_needs_refresh(&session) {
        if let Some(refresh_token) = session.refresh_token.as_deref() {
            let refreshed = refresh_access_token_blocking(refresh_token)?;

            let new_refresh_token = refreshed
                .refresh_token
                .or_else(|| session.refresh_token.clone());

            let new_expires_at = refreshed
                .expires_in
                .map(|in_s| (Utc::now() + Duration::seconds(in_s)).to_rfc3339());

            session = TokenSession {
                access_token: refreshed.access_token,
                refresh_token: new_refresh_token,
                expires_at: new_expires_at.or_else(|| session.expires_at.clone()),
            };

            state
                .secure_store
                .store_session(&session)
                .map_err(|e| format!("failed storing refreshed token session: {e}"))?;
        } else {
            return Err("dropbox token expired and no refresh_token available".to_string());
        }
    }

    if let Ok(mut cache) = state.token_cache.lock() {
        *cache = Some(session.clone());
    }

    Ok(session.access_token)
}

pub(crate) fn verify_dropbox_token_internal(state: &AppState) -> Result<bool, String> {
    fn probe(token: &str) -> Result<reqwest::blocking::Response, ()> {
        Client::new()
            .post("https://api.dropboxapi.com/2/users/get_current_account")
            .bearer_auth(token)
            .json(&serde_json::json!({}))
            .send()
            .map_err(|_| ())
    }

    let token = get_access_token(state)?;
    let response = match probe(&token) {
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
                match probe(&token2) {
                    Ok(r2) if r2.status().is_success() => Ok(true),
                    Ok(r2) if r2.status().as_u16() == 401 => Ok(false),
                    Ok(_) => Ok(true),
                    Err(()) => Ok(true),
                }
            }
            Err(e) => {
                if is_hard_auth_failure(&e) {
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
    if let Ok(engine) = state.sync_engine.lock() {
        if engine.is_sync_running() {
            return "Syncing".to_string();
        }
        if engine.current_status().last_error.is_some() {
            return "Error".to_string();
        }
    }
    "Idle".to_string()
}

pub(crate) fn update_tray_tooltip(app: &tauri::AppHandle, label: &str) {
    if let Some(tray) = app.tray_by_id("main") {
        let _ = tray.set_tooltip(Some(format!("DropboxSyncDesktop - {label}")));
    }
}
