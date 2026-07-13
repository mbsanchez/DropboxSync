use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::{distributions::Alphanumeric, Rng};
use reqwest::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use zeroize::Zeroizing;

use crate::error::{AppError, AppResult};

/// Dropbox App Console → your app → **Settings** → **App key** (OAuth2 `client_id`).
/// Prefer providing this value via `DROPBOX_APP_KEY` in `.env` for dev/build.
/// The build command embeds it at compile time via `option_env!`.
///
/// Threat model: this is the **public OAuth `client_id`**, not a secret. It is
/// meant to be sent to Dropbox and is recoverable from the binary regardless
/// (PKCE, not a client secret, is what protects the flow). `pub(crate)` here is
/// visibility hygiene — keeping the surface minimal — not a confidentiality
/// control; do not treat this value as sensitive.
pub(crate) const DROPBOX_APP_KEY: &str = match option_env!("DROPBOX_APP_KEY") {
    Some(k) => k,
    None => "",
};

/// Must match **exactly** a redirect URI allowed on the same Dropbox app.
/// Can be overridden via `DROPBOX_REDIRECT_URI` in `.env`.
pub const DROPBOX_REDIRECT_URI: &str = match option_env!("DROPBOX_REDIRECT_URI") {
    Some(u) => u,
    None => "http://localhost:53682/callback",
};

/// OAuth token payload. The bearer credentials are wrapped in `Zeroizing` so
/// they are wiped from memory on drop (DBSYNC-46). `expires_in` is not secret.
pub struct TokenResponse {
    pub access_token: Zeroizing<String>,
    pub refresh_token: Option<Zeroizing<String>>,
    pub expires_in: Option<i64>,
}

/// Plain deserialization target for the token endpoint response. `Zeroizing`
/// has no `Deserialize` impl, so we parse into this and immediately MOVE the
/// strings into `Zeroizing` (a move reuses the same allocation — the secret
/// bytes are never copied, and this value's plain lifetime is a single stmt).
#[derive(Deserialize)]
struct RawTokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
}

impl From<RawTokenResponse> for TokenResponse {
    fn from(raw: RawTokenResponse) -> Self {
        TokenResponse {
            access_token: Zeroizing::new(raw.access_token),
            refresh_token: raw.refresh_token.map(Zeroizing::new),
            expires_in: raw.expires_in,
        }
    }
}

fn resolve_app_key() -> AppResult<String> {
    let v = std::env::var("DROPBOX_APP_KEY").unwrap_or_else(|_| DROPBOX_APP_KEY.to_string());
    if v.is_empty() {
        return Err(AppError::Auth(
            "DROPBOX_APP_KEY is empty: set it in apps/desktop/.env (and run build again) or export it in the environment."
                .to_string(),
        ));
    }
    Ok(v)
}

fn resolve_redirect_uri() -> String {
    std::env::var("DROPBOX_REDIRECT_URI").unwrap_or_else(|_| DROPBOX_REDIRECT_URI.to_string())
}

/// Redirect URI used for the local listener and for the token exchange.
pub fn dropbox_redirect_uri() -> String {
    resolve_redirect_uri()
}

fn pkce_verifier() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(64)
        .map(char::from)
        .collect()
}

fn code_challenge(verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(hasher.finalize())
}

fn random_state() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect()
}

pub fn start_oauth() -> AppResult<(String, String, String)> {
    let app_key = resolve_app_key()?;
    let redirect_uri = resolve_redirect_uri();
    let verifier = pkce_verifier();

    let state = random_state();
    let challenge = code_challenge(&verifier);

    let auth_url = format!(
        "https://www.dropbox.com/oauth2/authorize?client_id={}&response_type=code&token_access_type=offline&redirect_uri={}&code_challenge={}&code_challenge_method=S256&state={}",
        urlencoding::encode(&app_key),
        urlencoding::encode(&redirect_uri),
        urlencoding::encode(&challenge),
        urlencoding::encode(&state)
    );

    Ok((auth_url, state, verifier))
}

pub async fn complete_oauth(
    code: String,
    verifier: Zeroizing<String>,
) -> AppResult<TokenResponse> {
    let app_key = resolve_app_key()?;
    let redirect_uri = resolve_redirect_uri();
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| AppError::Network(format!("failed to build oauth http client: {e}")))?;
    let response = client
        .post("https://api.dropboxapi.com/oauth2/token")
        // Pass values by `&str` so no additional owned copy of the verifier is
        // made here; `verifier` (Zeroizing) is wiped when it drops at the end of
        // this call. (serde_urlencoded's transient body buffer is owned by
        // reqwest and dropped after send — the best-effort limit here.)
        .form(&[
            ("code", code.as_str()),
            ("grant_type", "authorization_code"),
            ("client_id", app_key.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("code_verifier", verifier.as_str()),
        ])
        .send()
        .await
        .map_err(|e| AppError::Network(format!("token request failed: {e}")))?;

    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|e| AppError::Network(format!("token response body failed: {e}")))?;

    if !status.is_success() {
        let text = String::from_utf8_lossy(&body);
        return Err(AppError::Auth(format!(
            "dropbox token exchange failed ({status}): {text}"
        )));
    }

    let token: TokenResponse = serde_json::from_slice::<RawTokenResponse>(&body)
        // Do NOT echo the body: on a 2xx response it contains the access/refresh
        // token, which would leak in cleartext if this error is logged or shown
        // (DBSYNC-49). The serde error alone identifies the parse failure.
        .map_err(|e| AppError::Auth(format!("token parse failed: {e}")))?
        .into();

    Ok(token)
}

pub fn refresh_access_token_blocking(
    client: &reqwest::blocking::Client,
    refresh_token: &str,
) -> AppResult<TokenResponse> {
    let app_key = resolve_app_key()?;
    let redirect_uri = resolve_redirect_uri();

    let response = client
        .post("https://api.dropboxapi.com/oauth2/token")
        // Pass by `&str` so no owned plain-String copy of the refresh token is
        // made here (DBSYNC-46); `refresh_token` is already a borrowed &str.
        .form(&[
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
            ("client_id", app_key.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
        ])
        .send()
        .map_err(|e| AppError::Network(format!("refresh token request failed: {e}")))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response
            .text()
            .unwrap_or_else(|_| "<unreadable body>".to_string());
        return Err(AppError::Auth(format!(
            "dropbox refresh token exchange failed with status {status}; body: {body}"
        )));
    }

    response
        .json::<RawTokenResponse>()
        .map(TokenResponse::from)
        .map_err(|e| AppError::Auth(format!("refresh token parse failed: {e}")))
}
