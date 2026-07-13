use chrono::{Duration, Utc};

use crate::auth::oauth::complete_oauth;
use crate::error::{AppError, AppResult};
use crate::state::AppState;
use crate::storage;

/// Exchanges the authorization code for tokens and persists the session (keychain + cache).
pub(crate) async fn complete_oauth_internal(
    app_state: &AppState,
    code: String,
    state: String,
) -> AppResult<()> {
    let state = state.trim().to_string();
    let (expected_state, verifier) = {
        let engine = app_state
            .sync_engine
            .lock()
            .map_err(|_| AppError::Auth("sync engine lock poisoned".to_string()))?;
        (
            engine
                .pending_oauth_state()
                .unwrap_or_default()
                .trim()
                .to_string(),
            engine.pending_pkce_verifier().unwrap_or_default(),
        )
    };

    if expected_state.is_empty() {
        return Err(AppError::Auth(
            "OAuth session was reset (e.g. login restarted). Close the browser tab and click Start Dropbox login again."
                .to_string(),
        ));
    }

    if expected_state != state {
        return Err(AppError::Auth(format!(
            "invalid oauth state (length {} vs {}); try logging in again from the app without opening a second login tab",
            expected_state.len(),
            state.len()
        )));
    }

    let token = complete_oauth(code, verifier).await?;

    let expires_at = token
        .expires_in
        .map(|in_s| (Utc::now() + Duration::seconds(in_s)).to_rfc3339());
    let session = storage::secure_store::TokenSession {
        access_token: token.access_token.clone(),
        refresh_token: token.refresh_token.clone(),
        expires_at,
    };

    app_state
        .secure_store
        .store_session(&session)
        .map_err(|e| AppError::Auth(format!("failed to store token session: {e}")))?;

    {
        let mut cache = app_state
            .token_cache
            .lock()
            .map_err(|_| AppError::Auth("token cache mutex poisoned".to_string()))?;
        *cache = Some(session);
    }

    if let Ok(mut engine) = app_state.sync_engine.lock() {
        engine.clear_oauth_pending();
    }

    // Drop any stale delta cursor so the longpoll loop reseeds against this
    // (possibly new) account (DBSYNC-30).
    let _ = app_state
        .db
        .set_app_config(crate::remote_index::REMOTE_DELTA_CURSOR_KEY, "");

    Ok(())
}
