//! Tracing subscriber setup: structured logs written to a daily-rotating file
//! under the app data directory (kept even when no terminal is attached),
//! filtered by `RUST_LOG` (defaulting to `info`).

use std::sync::OnceLock;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

/// Keeps the non-blocking writer's background flush thread alive for the
/// app's lifetime. Must never be dropped, or the writer stops flushing.
static LOG_GUARD: OnceLock<WorkerGuard> = OnceLock::new();

fn env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
}

/// A message safe to hand to a C string API: interior NULs would truncate or reject it.
#[cfg(any(target_os = "macos", test))]
fn without_nul(message: &str) -> String {
    message.replace('\0', " ")
}

/// Writes a startup failure where a GUI launch can see it (DBSYNC-97). A Finder or Dock launch has
/// no terminal and the file log may not exist yet, so on macOS the text also goes to the unified log
/// via `NSLog` (Console.app, `log show --predicate 'process == "DropboxSyncDesktop"'`).
pub(crate) fn report_startup_failure(message: &str) {
    eprintln!("DropboxSync: {message}");
    #[cfg(target_os = "macos")]
    ns_log(&format!("DropboxSync: {message}"));
}

#[cfg(target_os = "macos")]
fn ns_log(message: &str) {
    use objc2::msg_send;
    use objc2::rc::autoreleasepool;
    use objc2::runtime::{AnyClass, AnyObject};
    use std::ffi::CString;

    #[link(name = "Foundation", kind = "framework")]
    extern "C" {
        fn NSLog(format: *const AnyObject, ...);
    }

    let Ok(text) = CString::new(without_nul(message)) else {
        return;
    };
    autoreleasepool(|_| {
        // SAFETY: `stringWithUTF8String:` takes NUL-terminated C strings that outlive the calls, and
        // `NSLog(@"%@", s)` is the documented way to log a string without it being parsed as a format.
        unsafe {
            let Some(ns_string) = AnyClass::get(c"NSString") else {
                return;
            };
            let format: *mut AnyObject = msg_send![ns_string, stringWithUTF8String: c"%@".as_ptr()];
            let body: *mut AnyObject = msg_send![ns_string, stringWithUTF8String: text.as_ptr()];
            if !format.is_null() && !body.is_null() {
                NSLog(format, body);
            }
        }
    });
}

/// Initializes the global `tracing` subscriber. Must be called once, as the
/// first thing in `run()`, before any other setup. Idempotent-safe: a second
/// call (e.g. from tests) is a harmless no-op rather than a panic.
pub(crate) fn init_tracing() {
    let dir = match crate::storage::db::app_data_dir() {
        Ok(dir) => dir,
        Err(e) => {
            // No app data dir available: fall back to a stderr-only subscriber rather than
            // panicking here. The reason is NOT swallowed (DBSYNC-97): on macOS it is usually a
            // missing App Group container, and `Db::new` fails with the same error right after.
            let _ = tracing_subscriber::fmt()
                .with_env_filter(env_filter())
                .try_init();
            report_startup_failure(&format!(
                "no app data directory, file logging disabled: {e}"
            ));
            tracing::error!(error = %e, "no app data directory, file logging disabled");
            return;
        }
    };

    let file_appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("dropbox-sync")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&dir)
        .expect("init rolling log appender");

    let (nb_writer, guard) = tracing_appender::non_blocking(file_appender);
    let _ = LOG_GUARD.set(guard);

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_writer(nb_writer)
        .with_ansi(false)
        .with_target(true);

    let _ = tracing_subscriber::registry()
        .with(env_filter())
        .with(fmt_layer)
        .try_init();
}

#[cfg(test)]
mod startup_failure_tests {
    use super::without_nul;

    /// `CString::new` rejects an interior NUL, which would silently drop the whole message.
    #[test]
    fn interior_nul_is_replaced_not_fatal() {
        let cleaned = without_nul("a\0b");
        assert_eq!(cleaned, "a b");
        assert!(std::ffi::CString::new(cleaned).is_ok());
    }
}
