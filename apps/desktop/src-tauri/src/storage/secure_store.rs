use keyring::Entry;
use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct SecureStore;

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

    pub fn store_token(&self, token: &str) -> Result<(), keyring::Error> {
        let entry = Entry::new("dropbox-sync-desktop", "dropbox-access-token")?;
        entry.set_password(token)
    }

    pub fn get_token(&self) -> Result<String, keyring::Error> {
        let entry = Entry::new("dropbox-sync-desktop", "dropbox-access-token")?;
        entry.get_password()
    }

    pub fn store_session(&self, session: &TokenSession) -> Result<(), keyring::Error> {
        let entry = Entry::new("dropbox-sync-desktop", "dropbox-token-session")?;
        let json = serde_json::to_string(session).map_err(|_| keyring::Error::NoEntry)?;
        entry.set_password(&json)
    }

    pub fn get_session(&self) -> Result<TokenSession, keyring::Error> {
        let entry = Entry::new("dropbox-sync-desktop", "dropbox-token-session")?;
        let json = entry.get_password()?;
        serde_json::from_str(&json).map_err(|_| keyring::Error::NoEntry)
    }
}
