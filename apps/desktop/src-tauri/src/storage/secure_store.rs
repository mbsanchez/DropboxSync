use keyring::Entry;
use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct SecureStore;

/// Full OAuth session persisted in the OS keychain (JSON), including `refresh_token`.
/// Survives app restarts. The in-memory `AppState::token_cache` is only a runtime copy.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TokenSession {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<String>, // RFC3339
}

impl SecureStore {
    pub fn new() -> Self {
        Self
    }

    pub fn get_token(&self) -> Result<String, keyring::Error> {
        let entry = Entry::new("dropbox-sync-desktop", "dropbox-access-token")?;
        entry.get_password()
    }

    /// Writes the whole session (access + optional refresh + expiry) to the keychain entry `dropbox-token-session`.
    pub fn store_session(&self, session: &TokenSession) -> Result<(), keyring::Error> {
        let entry = Entry::new("dropbox-sync-desktop", "dropbox-token-session")?;
        let json = serde_json::to_string(session).map_err(|_| keyring::Error::NoEntry)?;
        entry.set_password(&json)
    }

    /// Loads the persisted session from the keychain (same entry as `store_session`).
    pub fn get_session(&self) -> Result<TokenSession, keyring::Error> {
        let entry = Entry::new("dropbox-sync-desktop", "dropbox-token-session")?;
        let json = entry.get_password()?;
        serde_json::from_str(&json).map_err(|_| keyring::Error::NoEntry)
    }
}
