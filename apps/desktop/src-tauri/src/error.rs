//! Typed application error, replacing ad-hoc `Result<_, String>` across the crate.
//!
//! Internal functions return [`AppResult<T>`]; the Tauri command boundary converts
//! to `String` for IPC serialization via the `From<AppError> for String` bridge.
//!
//! This is phase A of the `Result<_, String>` -> typed error migration (DBSYNC-17):
//! only a handful of leaf modules use [`AppError`] so far. The bridge `From` impls
//! between `AppError` and `String` let migrated and not-yet-migrated modules
//! interoperate through `?` during the transition.

use thiserror::Error;

/// Application-wide typed error. Internal functions return `AppResult<T>`;
/// the Tauri command boundary converts to `String` for IPC serialization.
// TODO(DBSYNC-17): remove `allow(dead_code)` once every module producing these
// variants (auth) is migrated off `Result<_, String>`. `Network`, `Dropbox`,
// `Storage`, `Sync`, `Io` and `Other` are all constructed by now-migrated
// modules (dropbox_transfer, cloudsc, path_util, storage::db); `Auth` is
// still only produced by `auth`/`auth_session`, not yet migrated.
#[derive(Debug, Error)]
pub(crate) enum AppError {
    #[error("network error: {0}")]
    Network(String),
    #[allow(
        dead_code,
        reason = "constructed once auth/auth_session (DBSYNC-17 later phase) migrates off Result<_, String>"
    )]
    #[error("auth error: {0}")]
    Auth(String),
    #[error("dropbox API error (status {status}): {message}")]
    Dropbox { status: u16, message: String },
    #[error("storage error: {0}")]
    Storage(String),
    #[error("sync error: {0}")]
    Sync(String),
    #[error("io error: {0}")]
    Io(String),
    #[error("{0}")]
    Other(String),
}

pub(crate) type AppResult<T> = std::result::Result<T, AppError>;

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        AppError::Io(e.to_string())
    }
}

impl From<rusqlite::Error> for AppError {
    fn from(e: rusqlite::Error) -> Self {
        AppError::Storage(e.to_string())
    }
}

impl From<serde_json::Error> for AppError {
    fn from(e: serde_json::Error) -> Self {
        AppError::Other(e.to_string())
    }
}

// --- Migration bridges (temporary, removed in the final phase) ---
// TODO(DBSYNC-17): remove once every module is migrated
impl From<AppError> for String {
    fn from(e: AppError) -> Self {
        e.to_string()
    }
}

// TODO(DBSYNC-17): remove once every module is migrated. Now exercised by
// not-yet-migrated modules (e.g. `auth_session::get_access_token`, still
// `Result<_, String>`) whose errors flow through `?` into `AppResult`-returning
// callers such as `dropbox_transfer`.
impl From<String> for AppError {
    fn from(s: String) -> Self {
        AppError::Other(s)
    }
}

// TODO(DBSYNC-17): remove once every module is migrated. Not yet exercised by
// any call site (no not-yet-migrated module currently propagates a bare
// `&str` error into an `AppResult`-returning function via `?`), but kept for
// symmetry with `From<String>` and to avoid breaking a future caller that
// does.
#[allow(
    dead_code,
    reason = "not yet exercised by any call site; see comment above"
)]
impl From<&str> for AppError {
    fn from(s: &str) -> Self {
        AppError::Other(s.to_string())
    }
}

impl AppError {
    /// Transient = worth retrying (network blip, rate limit, 5xx). Used by
    /// `dropbox_transfer::retry_transient`.
    pub(crate) fn is_transient(&self) -> bool {
        match self {
            AppError::Network(_) => true,
            AppError::Dropbox { status, .. } => *status == 429 || (*status >= 500 && *status < 600),
            AppError::Sync(m) | AppError::Other(m) => {
                m.contains("request failed")
                    || m.contains("timed out")
                    || m.contains("timeout")
                    || m.contains("too_many_write_operations")
                    || m.contains("too_many_requests")
                    || m.contains("status error: 429")
                    || m.contains("status error: 5")
            }
            _ => false,
        }
    }

    /// Does this error indicate a Dropbox upload session that is no longer
    /// usable (expired/closed/offset mismatch), as opposed to a transient
    /// transport failure or an unrelated 4xx/5xx? Used by
    /// `dropbox_transfer::upload_via_session` to decide whether to abandon
    /// the current session and start a fresh one.
    ///
    /// Deliberately does NOT match a bare 409 with no known marker — a 409
    /// Conflict can also mean `too_many_write_operations` (write-lock
    /// contention on the destination), which is transient and handled by
    /// `is_transient`/`retry_transient`, not a dead session; treating every
    /// 409 as session-invalid would trigger a full re-upload from scratch for
    /// what is really just lock contention.
    pub(crate) fn is_session_invalid(&self) -> bool {
        let message = match self {
            AppError::Dropbox { message, .. } => message.as_str(),
            AppError::Sync(m) | AppError::Other(m) => m.as_str(),
            _ => return false,
        };
        message.contains("lookup_failed")
            || message.contains("not_found")
            || message.contains("incorrect_offset")
            || message.contains("closed")
            || message.contains("not_closed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_formats_each_variant() {
        assert_eq!(
            AppError::Network("boom".into()).to_string(),
            "network error: boom"
        );
        assert_eq!(
            AppError::Auth("denied".into()).to_string(),
            "auth error: denied"
        );
        assert_eq!(
            AppError::Dropbox {
                status: 404,
                message: "not_found".into()
            }
            .to_string(),
            "dropbox API error (status 404): not_found"
        );
        assert_eq!(
            AppError::Storage("locked".into()).to_string(),
            "storage error: locked"
        );
        assert_eq!(
            AppError::Sync("conflict".into()).to_string(),
            "sync error: conflict"
        );
        assert_eq!(
            AppError::Io("missing".into()).to_string(),
            "io error: missing"
        );
        assert_eq!(AppError::Other("oops".into()).to_string(), "oops");
    }

    #[test]
    fn io_error_converts_to_io_variant() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let app_err: AppError = io_err.into();
        assert!(matches!(app_err, AppError::Io(_)));
    }

    #[test]
    fn rusqlite_error_converts_to_storage_variant() {
        let sqlite_err = rusqlite::Error::InvalidQuery;
        let app_err: AppError = sqlite_err.into();
        assert!(matches!(app_err, AppError::Storage(_)));
    }

    #[test]
    fn is_transient_true_for_network_and_retryable_dropbox_statuses() {
        assert!(AppError::Network("connection reset".into()).is_transient());
        assert!(AppError::Dropbox {
            status: 429,
            message: "rate limited".into()
        }
        .is_transient());
        assert!(AppError::Dropbox {
            status: 503,
            message: "unavailable".into()
        }
        .is_transient());
        assert!(AppError::Other("request failed: connection reset".into()).is_transient());
    }

    #[test]
    fn is_transient_false_for_non_retryable_errors() {
        assert!(!AppError::Auth("bad token".into()).is_transient());
        assert!(!AppError::Dropbox {
            status: 404,
            message: "not_found".into()
        }
        .is_transient());
        assert!(!AppError::Other("bad path".into()).is_transient());
    }

    #[test]
    fn is_session_invalid_true_for_known_markers_in_dropbox_message() {
        assert!(AppError::Dropbox {
            status: 409,
            message: "upload_session/append_v2: {\"error\": {\".tag\": \"lookup_failed\"}}".into()
        }
        .is_session_invalid());
        assert!(AppError::Dropbox {
            status: 409,
            message: "upload_session/finish: {\"error_summary\": \"incorrect_offset\"}".into()
        }
        .is_session_invalid());
    }

    #[test]
    fn is_session_invalid_false_for_transient_or_unrelated_errors() {
        assert!(!AppError::Network("upload_session/append_v2 request failed: connection reset".into())
            .is_session_invalid());
        assert!(!AppError::Dropbox {
            status: 400,
            message: "Bad Request".into()
        }
        .is_session_invalid());
        // A 409 that is really write-lock contention, not a dead session.
        assert!(!AppError::Dropbox {
            status: 409,
            message: "upload_session/finish: too_many_write_operations/...".into()
        }
        .is_session_invalid());
    }
}
