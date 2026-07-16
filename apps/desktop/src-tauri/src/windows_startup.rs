//! DBSYNC-36 Slice S4: "Start at login" toggle backed by the WinRT
//! `Windows.ApplicationModel.StartupTask` API. The task itself is declared in
//! the sparse package manifest (`native/windows/sparse-package/AppxManifest.xml`,
//! `uap5:StartupTask` extension) — `StartupTask::GetAsync` only resolves a task
//! that a package manifest registered, so this (like the CfAPI sync root and the
//! shell status column) needs package identity (DBSYNC-58).
//!
//! Decision (KISS, see DBSYNC-36 plan): the WinRT `StartupTaskState` is the SOLE
//! source of truth. There is no `app_config` mirror — `get_startup_enabled`
//! always queries live.
#![cfg(windows)]

use windows::ApplicationModel::{StartupTask, StartupTaskState};
use windows::core::HSTRING;

/// Must equal the `TaskId` of the `uap5:StartupTask` extension in
/// `native/windows/sparse-package/AppxManifest.xml`. Never change once shipped
/// — Windows keys the Settings > Startup Apps entry (and the task itself) by
/// this id.
const STARTUP_TASK_ID: &str = "DropboxSyncStartup";

/// True if the startup task is currently `Enabled` or `EnabledByPolicy`.
/// Errs (without a package identity, or on a WinRT failure) rather than
/// silently reporting `false`, so the frontend can distinguish "off" from
/// "unsupported/unknown".
pub(crate) fn get_startup_enabled() -> Result<bool, String> {
    require_package_identity()?;
    let state = run_on_mta(|| {
        let task = get_task()?;
        task.State().map_err(win_err)
    })?;
    Ok(is_enabled(state))
}

/// Enable or disable the startup task and return the resulting enabled state
/// (per `StartupTaskState`, not merely "request accepted" — e.g. a user- or
/// policy-disabled task may not actually turn on).
pub(crate) fn set_startup_enabled(enabled: bool) -> Result<bool, String> {
    require_package_identity()?;
    let state = run_on_mta(move || {
        let task = get_task()?;
        if enabled {
            let op = task.RequestEnableAsync().map_err(win_err)?;
            op.get().map_err(win_err)
        } else {
            task.Disable().map_err(win_err)?;
            task.State().map_err(win_err)
        }
    })?;
    Ok(is_enabled(state))
}

fn require_package_identity() -> Result<(), String> {
    if crate::windows_identity::has_package_identity() {
        Ok(())
    } else {
        Err("startup-at-login requires package identity".to_string())
    }
}

fn get_task() -> Result<StartupTask, String> {
    let op = StartupTask::GetAsync(&HSTRING::from(STARTUP_TASK_ID)).map_err(win_err)?;
    op.get().map_err(win_err)
}

fn is_enabled(state: StartupTaskState) -> bool {
    state == StartupTaskState::Enabled || state == StartupTaskState::EnabledByPolicy
}

fn win_err(err: windows::core::Error) -> String {
    tracing::warn!(error = %err, "startup-at-login: WinRT call failed");
    err.to_string()
}

/// Run a WinRT closure on a dedicated MTA thread — the async `.get()` calls
/// above must not run on the app's STA main thread (mirrors
/// `cloud_filter.rs::run_on_mta`). Bounded by a timeout so a stuck WinRT call
/// (e.g. `RequestEnableAsync` waiting on a consent prompt that never appears)
/// can't hang the IPC caller.
fn run_on_mta<F>(f: F) -> Result<StartupTaskState, String>
where
    F: FnOnce() -> Result<StartupTaskState, String> + Send + 'static,
{
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // SAFETY: a fresh thread; MTA is fine for these APIs. Ignore the
        // result (S_FALSE = already initialised is not an error here).
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let _ = tx.send(f());
    });
    match rx.recv_timeout(std::time::Duration::from_secs(20)) {
        Ok(result) => result,
        Err(_) => Err("startup-at-login: WinRT call timed out".to_string()),
    }
}
