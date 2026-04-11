use chrono::{Duration, Utc};

use crate::auth::oauth::complete_oauth;
use crate::state::AppState;
use crate::storage;

/// Exchanges the authorization code for tokens and persists the session (keychain + cache).
pub(crate) async fn complete_oauth_internal(
    app_state: &AppState,
    code: String,
    state: String,
) -> Result<(), String> {
    let state = state.trim().to_string();
    let (expected_state, verifier) = {
        let engine = app_state
            .sync_engine
            .lock()
            .map_err(|_| "sync engine lock poisoned".to_string())?;
        (
            engine.pending_oauth_state().unwrap_or_default().trim().to_string(),
            engine.pending_pkce_verifier().unwrap_or_default(),
        )
    };

    if expected_state.is_empty() {
        return Err(
            "OAuth session was reset (e.g. login restarted). Close the browser tab and click Start Dropbox login again."
                .to_string(),
        );
    }

    if expected_state != state {
        return Err(format!(
            "invalid oauth state (length {} vs {}); try logging in again from the app without opening a second login tab",
            expected_state.len(),
            state.len()
        ));
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
        .map_err(|e| format!("failed to store token session: {e}"))?;

    {
        let mut cache = app_state
            .token_cache
            .lock()
            .map_err(|_| "token cache mutex poisoned".to_string())?;
        *cache = Some(session);
    }

    if let Ok(mut engine) = app_state.sync_engine.lock() {
        engine.clear_oauth_pending();
    }

    Ok(())
}
