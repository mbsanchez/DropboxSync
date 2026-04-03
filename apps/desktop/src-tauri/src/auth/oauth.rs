use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::{distributions::Alphanumeric, Rng};
use reqwest::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Dropbox App Console → your app → **Settings** → **App key** (OAuth2 `client_id`).
/// Not a secret in public clients: the flow uses PKCE; still, do not commit someone else's key to public repos.
pub const DROPBOX_APP_KEY: &str = "aauuh6hib5i8crx";

/// Must match **exactly** a redirect URI allowed on the same Dropbox app.
pub const DROPBOX_REDIRECT_URI: &str = "http://localhost:53682/callback";

#[derive(Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: Option<i64>,
}

fn resolve_app_key() -> Result<String, String> {
    let v = std::env::var("DROPBOX_APP_KEY").unwrap_or_else(|_| DROPBOX_APP_KEY.to_string());
    if v.is_empty() {
        return Err(
            "DROPBOX_APP_KEY is empty: set it in src-tauri/src/auth/oauth.rs or via the environment variable."
                .to_string(),
        );
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

pub fn start_oauth() -> Result<(String, String, String), String> {
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

pub async fn complete_oauth(code: String, verifier: String) -> Result<TokenResponse, String> {
    let app_key = resolve_app_key()?;
    let redirect_uri = resolve_redirect_uri();
    let response = Client::new()
        .post("https://api.dropboxapi.com/oauth2/token")
        .form(&[
            ("code", code),
            ("grant_type", "authorization_code".to_string()),
            ("client_id", app_key),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .map_err(|e| format!("token request failed: {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        return Err(format!("dropbox token exchange failed with status {status}"));
    }

    let token = response
        .json::<TokenResponse>()
        .await
        .map_err(|e| format!("token parse failed: {e}"))?;

    Ok(token)
}

pub fn refresh_access_token_blocking(refresh_token: &str) -> Result<TokenResponse, String> {
    let app_key = resolve_app_key()?;
    let redirect_uri = resolve_redirect_uri();

    let client = reqwest::blocking::Client::new();
    let response = client
        .post("https://api.dropboxapi.com/oauth2/token")
        .form(&[
            ("refresh_token", refresh_token.to_string()),
            ("grant_type", "refresh_token".to_string()),
            ("client_id", app_key),
            ("redirect_uri", redirect_uri),
        ])
        .send()
        .map_err(|e| format!("refresh token request failed: {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_else(|_| "<unreadable body>".to_string());
        return Err(format!(
            "dropbox refresh token exchange failed with status {status}; body: {body}"
        ));
    }

    response
        .json::<TokenResponse>()
        .map_err(|e| format!("refresh token parse failed: {e}"))
}
