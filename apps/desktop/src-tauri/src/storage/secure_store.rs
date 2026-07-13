use keyring::Entry;
use serde::Deserialize;
use zeroize::Zeroizing;

const SERVICE: &str = "dropbox-sync-desktop";

/// Legacy: entire session as one JSON string (exceeds Windows blob limit when combined).
const LEGACY_SESSION_KEY: &str = "dropbox-token-session";

const SESSION_ACCESS: &str = "dropbox-session-access";
const SESSION_REFRESH: &str = "dropbox-session-refresh";
const SESSION_EXPIRES: &str = "dropbox-session-expires";

/// Windows `CRED_MAX_CREDENTIAL_BLOB_SIZE` is 2560 bytes; keyring stores passwords as UTF-16 LE,
/// so the check is `utf16_units * 2 <= 2560`. We stay under that per chunk.
const SAFE_UTF16_PAYLOAD_BYTES: usize = 2400;

/// Marker in the primary entry when the secret is split across `name.0`, `name.1`, ...
const CHUNK_MARKER: &str = "__KEYRING_CHUNKED_V1__:";

#[derive(Clone)]
pub struct SecureStore;

/// Full OAuth session persisted in the OS keychain, including `refresh_token`.
/// Survives app restarts. The in-memory `AppState::token_cache` is only a runtime copy.
///
/// The bearer credentials are `Zeroizing` so the cached in-memory copy is wiped
/// on drop (DBSYNC-46). Persisted fields are written to the keyring individually
/// (see `store_session`), so `TokenSession` itself is never (de)serialized —
/// hence no `Serialize`/`Deserialize` derive; the legacy JSON blob is parsed via
/// `LegacyTokenSession` below.
#[derive(Clone, Debug)]
pub struct TokenSession {
    pub access_token: Zeroizing<String>,
    pub refresh_token: Option<Zeroizing<String>>,
    pub expires_at: Option<String>, // RFC3339 — not secret
}

/// Plain parse target for the pre-`TokenSession` single-JSON-blob keyring entry.
/// Only used in the one-time migration in `get_session`; strings are moved into
/// `Zeroizing` immediately after parsing.
#[derive(Deserialize)]
struct LegacyTokenSession {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: Option<String>,
}

impl SecureStore {
    pub fn new() -> Self {
        Self
    }

    /// Legacy single-token entry used before `TokenSession` (see `auth_session` migration).
    pub fn get_token(&self) -> Result<String, keyring::Error> {
        Entry::new(SERVICE, "dropbox-access-token")?.get_password()
    }

    pub fn store_session(&self, session: &TokenSession) -> Result<(), keyring::Error> {
        store_value_chunked(SESSION_ACCESS, &session.access_token)?;

        if let Some(ref rt) = session.refresh_token {
            store_value_chunked(SESSION_REFRESH, rt)?;
        } else {
            clear_chunked_key(SESSION_REFRESH)?;
        }

        let expires_e = Entry::new(SERVICE, SESSION_EXPIRES)?;
        if let Some(ref exp) = session.expires_at {
            expires_e.set_password(exp)?;
        } else {
            let _ = expires_e.delete_credential();
        }

        let _ = Entry::new(SERVICE, LEGACY_SESSION_KEY)?.delete_credential();

        Ok(())
    }

    pub fn get_session(&self) -> Result<TokenSession, keyring::Error> {
        if let Ok(access) = load_value_chunked(SESSION_ACCESS) {
            let refresh = match load_value_chunked(SESSION_REFRESH) {
                Ok(s) if !s.is_empty() => Some(s),
                _ => None,
            };
            let expires_at = match Entry::new(SERVICE, SESSION_EXPIRES)?.get_password() {
                Ok(s) if !s.is_empty() => Some(s),
                _ => None,
            };
            return Ok(TokenSession {
                access_token: Zeroizing::new(access),
                refresh_token: refresh.map(Zeroizing::new),
                expires_at,
            });
        }

        let legacy_e = Entry::new(SERVICE, LEGACY_SESSION_KEY)?;
        let json = legacy_e.get_password()?;
        let legacy: LegacyTokenSession =
            serde_json::from_str(&json).map_err(|_| keyring::Error::NoEntry)?;
        let session = TokenSession {
            access_token: Zeroizing::new(legacy.access_token),
            refresh_token: legacy.refresh_token.map(Zeroizing::new),
            expires_at: legacy.expires_at,
        };
        self.store_session(&session)?;
        Ok(session)
    }
}

fn utf16_payload_bytes(s: &str) -> usize {
    s.encode_utf16().count() * 2
}

/// Split a string into chunks each within Windows Credential Manager UTF-16 blob limit.
fn split_utf16_chunks(s: &str) -> Vec<String> {
    if utf16_payload_bytes(s) <= SAFE_UTF16_PAYLOAD_BYTES {
        return vec![s.to_string()];
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut cur_bytes = 0usize;
    for ch in s.chars() {
        let add = ch.len_utf16() * 2;
        if cur_bytes + add > SAFE_UTF16_PAYLOAD_BYTES && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            cur_bytes = 0;
        }
        current.push(ch);
        cur_bytes += add;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn chunk_part_key(base: &str, i: usize) -> String {
    format!("{base}.{i}")
}

/// Deletes `base.start_idx`, `base.start_idx+1`, … (best-effort) to trim old chunk parts.
fn clear_overflow_parts(base: &str, start_idx: usize) {
    for i in start_idx..start_idx + 64 {
        let _ = Entry::new(SERVICE, &chunk_part_key(base, i)).and_then(|e| e.delete_credential());
    }
}

fn store_value_chunked(base: &str, value: &str) -> Result<(), keyring::Error> {
    let chunks = split_utf16_chunks(value);
    let primary = Entry::new(SERVICE, base)?;

    if chunks.len() == 1 {
        primary.set_password(&chunks[0])?;
        // Remove any previous chunked parts (.0, .1, …) now inlined in `base`.
        clear_overflow_parts(base, 0);
        return Ok(());
    }

    primary.set_password(&format!("{CHUNK_MARKER}{}", chunks.len()))?;
    for (i, part) in chunks.iter().enumerate() {
        Entry::new(SERVICE, &chunk_part_key(base, i))?.set_password(part)?;
    }
    // Drop leftover parts if the new value uses fewer chunks than before.
    clear_overflow_parts(base, chunks.len());
    Ok(())
}

fn load_value_chunked(base: &str) -> Result<String, keyring::Error> {
    let primary = Entry::new(SERVICE, base)?.get_password()?;
    if let Some(rest) = primary.strip_prefix(CHUNK_MARKER) {
        let n: usize = rest.parse().map_err(|_| keyring::Error::NoEntry)?;
        let mut out = String::new();
        for i in 0..n {
            let part = Entry::new(SERVICE, &chunk_part_key(base, i))?.get_password()?;
            out.push_str(&part);
        }
        return Ok(out);
    }
    Ok(primary)
}

fn clear_chunked_key(base: &str) -> Result<(), keyring::Error> {
    let _ = Entry::new(SERVICE, base)?.delete_credential();
    clear_overflow_parts(base, 0);
    Ok(())
}
