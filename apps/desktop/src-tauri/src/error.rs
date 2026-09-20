//! Typed application error, replacing ad-hoc `Result<_, String>` across the crate.
//!
//! Internal functions return [`AppResult<T>`]. The Tauri command boundary
//! (`#[tauri::command]` functions in `commands.rs`) converts to `String` for
//! IPC serialization via the `From<AppError> for String` conversion below;
//! that is the only place in the crate an `AppError` is turned into a `String`
//! (DBSYNC-17).

use thiserror::Error;

/// Application-wide typed error. Internal functions return `AppResult<T>`;
/// the Tauri command boundary converts to `String` for IPC serialization.
#[derive(Debug, Error)]
pub(crate) enum AppError {
    #[error("network error: {0}")]
    Network(String),
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

// --- IPC boundary conversion ---
// The single sanctioned `AppError` -> `String` conversion. `#[tauri::command]`
// functions in `commands.rs` return `Result<_, String>` (Tauri IPC requires a
// serializable error type); every command converts its `AppResult` internals
// via this `From` impl (through `?` or an explicit `.map_err(String::from)`).
// No other module should need to go the other way (`String` -> `AppError`):
// every internal function returns `AppResult` directly.
impl From<AppError> for String {
    fn from(e: AppError) -> Self {
        e.to_string()
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

    /// Is this a path Dropbox will never accept, however many times we try?
    ///
    /// Written against a **recorded live response** (DBSYNC-104 slice 1). Uploading a
    /// path containing a backslash returns, verbatim:
    ///
    /// ```text
    /// HTTP 409
    /// {"error":{".tag":"path","reason":{".tag":"malformed_path","malformed_path":null},
    ///  "upload_session_id":"pid_upload_session:..."},"error_summary":"path/malformed_path/"}
    /// ```
    ///
    /// That matters: DBSYNC-99 shipped a `RelocationError` classifier written from the
    /// spec, not one of whose permanent markers has ever been observed live, and one
    /// arm of which turned out to be wrong and had to be retracted.
    ///
    /// **Matches the `error_summary` key, not a bare substring.** `AppError::Dropbox`'s
    /// `message` is composed as `format!("upload for {path}: {body}")`, so the user's own
    /// path is inside the haystack: a file named `malformed_path.txt` would otherwise turn
    /// any 409 — `path/not_found` on a queued download, say — into a permanent failure.
    /// Deciding by looking at the wrong bytes is the bug class this whole ticket is about,
    /// so the marker is anchored to the JSON key that only Dropbox can write.
    ///
    /// Narrow on the status too: a bare 409 is NOT enough. Dropbox uses 409 for ordinary,
    /// recoverable path conflicts, and treating those as permanent would strand files that
    /// would have synced on the next tick.
    ///
    /// **`malformed_path` is broader than the backslash that was probed.** It is Dropbox's
    /// general "this path does not satisfy the required format": illegal characters
    /// (`< > : " | ? *` as well as `\`), a trailing space or period, an over-long component.
    /// All of them are permanent and all are fixed by renaming, so one classification is
    /// right — but the user-facing message must describe the tag, not the one cause that
    /// happened to be probed. See `unrepresentable_path_message`.
    /// **Recorded from `files/upload`; consumed by every job type.** Review M3 is right
    /// that the other endpoints' bodies were never observed. The asymmetry is safe in one
    /// direction only, and that is why it is acceptable: an endpoint that returns the same
    /// 409 + `error_summary` shape is classified correctly, and one that returns anything
    /// else (a 400, a different tag) simply falls through to the ordinary attempt budget —
    /// exactly the behaviour that existed before this classifier. So an unobserved shape
    /// costs a worse message, never a wrong permanent failure.
    ///
    /// For moves specifically, `classify_move_response` runs first and has no
    /// `malformed_path` arm, so a rejected destination reaches here only if that classifier
    /// surfaces the status and body unchanged. Unverified, and deliberately left that way
    /// rather than guessed at: the next probe that touches `move_v2` should record it.
    pub(crate) fn is_unrepresentable_path(&self) -> bool {
        match self {
            AppError::Dropbox { status, message } => {
                *status == 409 && message.contains(r#""error_summary":"path/malformed_path"#)
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

    /// Does this error indicate a session that cannot be recovered by retrying
    /// (missing session, no refresh token, or Dropbox explicitly rejecting the
    /// refresh grant), as opposed to a transient refresh failure that is worth
    /// retrying later? Used by `auth_session::verify_dropbox_token_internal`
    /// and `commands::compute_startup_requirements` to decide whether to
    /// surface the user as logged-out.
    ///
    /// Uses `Display` (`self.to_string()`) to inspect the message: the `Auth`
    /// variant's `"auth error: "` prefix does not interfere with any of the
    /// `contains` markers below.
    pub(crate) fn is_hard_auth(&self) -> bool {
        let text = self.to_string();
        let lower = text.to_ascii_lowercase();
        text.contains("dropbox token expired and no refresh_token available")
            || text.contains("missing dropbox token session")
            // Only treat refresh failures as hard when Dropbox explicitly rejects the grant.
            || (text.contains("dropbox refresh token exchange failed with status")
                && (lower.contains("invalid_grant") || lower.contains("invalid_client")))
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
        assert!(!AppError::Network(
            "upload_session/append_v2 request failed: connection reset".into()
        )
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

    #[test]
    fn is_hard_auth_true_for_missing_session_or_refresh_token() {
        assert!(AppError::Auth("missing dropbox token session".into()).is_hard_auth());
        assert!(
            AppError::Auth("dropbox token expired and no refresh_token available".into())
                .is_hard_auth()
        );
    }

    #[test]
    fn is_hard_auth_true_for_rejected_refresh_grant() {
        assert!(AppError::Auth(
            "dropbox refresh token exchange failed with status 400; body: {\"error\": \"invalid_grant\"}"
                .into()
        )
        .is_hard_auth());
        assert!(AppError::Auth(
            "dropbox refresh token exchange failed with status 401; body: {\"error\": \"INVALID_CLIENT\"}"
                .into()
        )
        .is_hard_auth());
    }

    #[test]
    fn is_hard_auth_false_for_transient_refresh_failures_and_unrelated_errors() {
        // Refresh failed, but not because Dropbox rejected the grant (e.g. a transient 503).
        assert!(!AppError::Auth(
            "dropbox refresh token exchange failed with status 503; body: server busy".into()
        )
        .is_hard_auth());
        assert!(!AppError::Network("connection reset".into()).is_hard_auth());
        assert!(!AppError::Auth("some other auth hiccup".into()).is_hard_auth());
    }

    /// DBSYNC-104 slice 6. The body below is the VERBATIM 409 recorded from a live
    /// `files/upload` of a path containing a backslash, against a real account. Writing
    /// the classifier against a recorded response rather than the API spec is the whole
    /// point of the probe that preceded this slice.
    #[test]
    fn a_malformed_path_rejection_is_permanent() {
        let recorded = AppError::Dropbox {
            status: 409,
            message: "upload for /probe-a\\b.txt: {\"error\":{\".tag\":\"path\",\
                      \"reason\":{\".tag\":\"malformed_path\",\"malformed_path\":null},\
                      \"upload_session_id\":\"pid_upload_session:ABIL\"},\
                      \"error_summary\":\"path/malformed_path/\"}"
                .to_string(),
        };

        assert!(recorded.is_unrepresentable_path());
        assert!(
            !recorded.is_transient(),
            "a permanently-unacceptable path must never be retried as a blip"
        );
    }

    /// Review H2(a). `message` is composed as `format!("upload for {path}: {body}")`, so
    /// the user's own path sits inside the haystack. A bare `contains("malformed_path")`
    /// meant a file NAMED `malformed_path.txt` turned any 409 into a permanent failure —
    /// deciding by looking at the wrong bytes, which is the bug class this ticket is about.
    /// Anchoring to the `error_summary` key fixes it: only Dropbox writes that.
    #[test]
    fn a_file_named_after_the_marker_does_not_forge_a_permanent_failure() {
        let path_only = AppError::Dropbox {
            status: 409,
            message: "download for /Docs/malformed_path.txt: \
                      {\"error_summary\":\"path/not_found/\"}"
                .to_string(),
        };
        assert!(
            !path_only.is_unrepresentable_path(),
            "the marker must come from Dropbox's response, not from the user's filename"
        );

        // Even a folder engineered to look like the whole tag must not match.
        let adversarial = AppError::Dropbox {
            status: 409,
            message: "upload for /path/malformed_path/x.txt: \
                      {\"error_summary\":\"path/conflict/file/\"}"
                .to_string(),
        };
        assert!(!adversarial.is_unrepresentable_path());
    }

    /// Narrowness is the safety property here. Dropbox uses 409 for ordinary,
    /// recoverable path conflicts; classifying those as permanent would strand files
    /// that would have synced on the next tick.
    #[test]
    fn an_ordinary_conflict_is_not_treated_as_unrepresentable() {
        for message in [
            "upload for /a.txt: {\"error_summary\":\"path/conflict/file/\"}",
            "upload for /a.txt: {\"error_summary\":\"path/insufficient_space/\"}",
            "upload for /a.txt: {\"error_summary\":\"too_many_write_operations\"}",
        ] {
            let err = AppError::Dropbox {
                status: 409,
                message: message.to_string(),
            };
            assert!(
                !err.is_unrepresentable_path(),
                "a bare 409 must not be permanent: {message}"
            );
        }

        // Nor is any non-Dropbox error, whatever it says.
        assert!(!AppError::Other("malformed_path".into()).is_unrepresentable_path());
        assert!(!AppError::Network("reset".into()).is_unrepresentable_path());
    }
}
