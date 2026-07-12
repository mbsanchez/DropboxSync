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

/// Initializes the global `tracing` subscriber. Must be called once, as the
/// first thing in `run()`, before any other setup. Idempotent-safe: a second
/// call (e.g. from tests) is a harmless no-op rather than a panic.
pub(crate) fn init_tracing() {
    let dir = match crate::storage::db::app_data_dir() {
        Ok(dir) => dir,
        Err(_) => {
            // No app data dir available (e.g. unusual environment): fall back to a
            // stderr-only subscriber rather than panicking.
            let _ = tracing_subscriber::fmt()
                .with_env_filter(env_filter())
                .try_init();
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
