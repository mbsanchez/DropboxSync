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
        || err.contains("missing dropbox token session:")
        || err.contains("dropbox refresh token exchange failed with status")
}

pub(crate) fn get_access_token(state: &AppState) -> Result<String, String> {
    fn session_expired(session: &TokenSession) -> bool {
        let Some(expires_at) = session.expires_at.as_ref() else {
            return false;
        };

        let expires_dt = chrono::DateTime::parse_from_rfc3339(expires_at)
            .map(|dt| dt.with_timezone(&Utc))
            .ok();

        let Some(expires_dt) = expires_dt else {
            return false;
        };

        Utc::now() + Duration::seconds(60) >= expires_dt
    }

    if let Ok(cache) = state.token_cache.lock() {
        if let Some(session) = cache.as_ref() {
            if !session_expired(session) {
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

    if session_expired(&session) {
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
    let token = get_access_token(state)?;
    let response = match Client::new()
        .post("https://api.dropboxapi.com/2/users/get_current_account")
        .bearer_auth(&token)
        .json(&serde_json::json!({}))
        .send()
    {
        Ok(r) => r,
        Err(_) => return Ok(true),
    };
    if response.status().is_success() {
        return Ok(true);
    }
    if response.status().as_u16() == 401 {
        return Ok(false);
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
