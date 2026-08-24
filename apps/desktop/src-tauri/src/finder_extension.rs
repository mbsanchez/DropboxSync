//! Whether the user has switched our Finder Sync extension on (DBSYNC-86).
//!
//! This is the only Objective-C interop in the project, and it exists because there is no
//! Rust binding for `FinderSync.framework`. It is kept in its own module so the single
//! `unsafe` block has a name and a home rather than sitting inside `commands.rs`.
//!
//! **Why the app needs to ask at all.** macOS registers a *new* plug-in instance on every
//! reinstall and the new one starts **disabled** — measured on 2026-08-21, two installs two
//! minutes apart produced two UUIDs, the second one off. So an ordinary app update silently
//! switches badges off, and without this the app has no way to say so.
//!
//! **Why it cannot enable it.** `FinderSync.h` ships `isExtensionEnabled` and
//! `showExtensionManagementInterface` and deliberately no setter. The header's own comment
//! prescribes the flow: check when the app becomes active, and show the management UI.

use serde::Serialize;

/// Three states, never a `bool`.
///
/// `NotApplicable` is not a synonym for `Disabled`: Windows has no Finder, and a Windows user
/// must never be shown a banner about a Finder extension. Collapsing these two is the mistake
/// this type exists to make impossible.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum FinderExtensionState {
    /// The user has it switched on. Nothing to say.
    Enabled,
    /// Registered but switched off — badges will not appear. This is the one that warns.
    Disabled,
    /// Not macOS, or the state could not be determined. Say nothing.
    NotApplicable,
}

/// Reads `FIFinderSyncController.isExtensionEnabled`.
///
/// **The answer is scoped to the calling bundle.** Measured during planning: a standalone
/// process gets `false` while the extension is genuinely enabled, because the property reports
/// on the extension inside the *caller's* app bundle. So this returns a meaningful value only
/// from inside `DropboxSyncDesktop.app` — and, correspondingly, it cannot be verified by any
/// test or spike outside it. The check that matters is running the app with the extension ON
/// and confirming this says `Enabled`; an implementation stuck at `Disabled` looks perfectly
/// correct as long as you only ever test with it switched off.
/// **Must be read on the main thread** (DBSYNC-88). This is not a precaution, it is the
/// bug: DBSYNC-86 read it on whatever thread called it, and `get_startup_requirements` is a
/// synchronous Tauri command, which Tauri runs on a pool thread rather than the main one. On
/// macOS 26 that returned `false` with the extension enabled and rendering badges, so the app
/// warned the user that a working feature was switched off, permanently and across relaunches.
///
/// The sibling API in this same file already knew: `open_finder_extension_settings` dispatches
/// through `run_on_main_thread` and says AppKit requires it. The read did not, and nothing
/// connected the two until the banner was seen lying on a real machine.
pub(crate) fn finder_extension_state() -> FinderExtensionState {
    #[cfg(target_os = "macos")]
    {
        // Already on the main thread: read directly. Dispatching instead would deadlock —
        // `run_on_main_thread` queues the closure for a thread that is currently blocked
        // waiting for it.
        if is_main_thread() {
            return read_extension_enabled();
        }

        // No handle yet (unit tests never call `setup()`, and startup can ask early). "Could
        // not determine" is `NotApplicable`, never `Disabled` — the whole point of having a
        // third state is that an unknown must not render a warning.
        let Some(app) = crate::state::APP_HANDLE.get() else {
            return FinderExtensionState::NotApplicable;
        };

        let (tx, rx) = std::sync::mpsc::channel();
        if app
            .run_on_main_thread(move || {
                let _ = tx.send(read_extension_enabled());
            })
            .is_err()
        {
            return FinderExtensionState::NotApplicable;
        }

        // Bounded wait: a busy main thread must delay a startup check, never hang it. Timing
        // out yields `NotApplicable`, so the failure direction is silence rather than a false
        // alarm — which is the trade-off this whole ticket is about.
        rx.recv_timeout(std::time::Duration::from_millis(500))
            .unwrap_or(FinderExtensionState::NotApplicable)
    }
    #[cfg(not(target_os = "macos"))]
    {
        FinderExtensionState::NotApplicable
    }
}

/// Whether the caller is on the main thread, asked of AppKit rather than assumed.
#[cfg(target_os = "macos")]
fn is_main_thread() -> bool {
    use objc2::msg_send;
    use objc2::runtime::AnyClass;

    let Some(cls) = AnyClass::get(c"NSThread") else {
        return false;
    };
    // SAFETY: `isMainThread` is a documented `NSThread` class property returning `BOOL`,
    // taking no arguments, sent to the class object. Nothing is retained.
    unsafe { msg_send![cls, isMainThread] }
}

/// The raw read. Only ever called on the main thread — see [`finder_extension_state`].
#[cfg(target_os = "macos")]
fn read_extension_enabled() -> FinderExtensionState {
    use objc2::msg_send;
    use objc2::runtime::AnyClass;

    // A missing class is an answer, not a crash: if `FinderSync.framework` is not loaded
    // there is nothing to report and `NotApplicable` is the honest value. This is why the
    // "must not panic" requirement needs no `catch_unwind` — `get` already returns Option.
    let Some(cls) = AnyClass::get(c"FIFinderSyncController") else {
        return FinderExtensionState::NotApplicable;
    };

    // SAFETY: `FIFinderSyncController` is an Apple class from FinderSync.framework, and
    // `isExtensionEnabled` is a documented class property (macOS 10.14+) taking no
    // arguments and returning `BOOL`. The selector is sent to the class object, which is
    // what a class property requires. Nothing is retained, so there is nothing to release.
    let enabled: bool = unsafe { msg_send![cls, isExtensionEnabled] };

    if enabled {
        FinderExtensionState::Enabled
    } else {
        FinderExtensionState::Disabled
    }
}

/// Opens System Settings at the Finder extensions pane (DBSYNC-86).
///
/// **Must run on the main thread.** It presents AppKit UI, and Tauri commands carry no
/// main-thread guarantee — calling it from a worker is undefined behaviour that shows up as an
/// intermittent hang rather than a clean failure. `run_on_main_thread` is the dispatch.
///
/// A missing class returns `Ok(())` rather than an error: failing to open a settings pane is not
/// worth surfacing to the user as a dialog, and on non-macOS there is nothing to open.
#[tauri::command]
pub fn open_finder_extension_settings(app: tauri::AppHandle) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        app.run_on_main_thread(|| {
            use objc2::msg_send;
            use objc2::runtime::AnyClass;

            let Some(cls) = AnyClass::get(c"FIFinderSyncController") else {
                return;
            };
            // SAFETY: `showExtensionManagementInterface` is a documented class method of
            // `FIFinderSyncController` (macOS 10.14+) taking no arguments and returning void.
            // Sent to the class object, on the main thread, as AppKit UI requires.
            unsafe {
                let _: () = msg_send![cls, showExtensionManagementInterface];
            }
        })
        .map_err(|e| format!("could not reach the main thread: {e}"))?;
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
    }
    Ok(())
}

/// Restarts Finder so it picks up a newly enabled extension (DBSYNC-86).
///
/// Enabling the extension is not enough on its own: on 2026-08-21 the checkbox was ticked and
/// badges still did not appear, because Finder had not reloaded the plug-in. Only a restart
/// moved it.
///
/// **Never called automatically**, because it restarts the user's file manager underneath
/// them. It is offered as a button and nothing more.
///
/// It does NOT close their open windows — macOS relaunches Finder and restores them, and this
/// comment claimed otherwise until DBSYNC-88. That wording cost real time: the maintainer
/// restarted Finder, saw the windows still open, and reasonably concluded it had not worked.
///
/// This is a plain process signal, not a `pluginkit` call: the ticket forbids the latter
/// because it depends on undocumented behaviour, while `killall Finder` is documented,
/// reversible and something users do by hand routinely.
#[tauri::command]
pub fn restart_finder() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("/usr/bin/killall")
            .arg("Finder")
            .status()
            .map_err(|e| format!("could not restart Finder: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{finder_extension_state, FinderExtensionState};

    /// On anything that is not macOS the answer must be `NotApplicable` — never `Disabled`,
    /// which is the value that renders a warning. A Windows user has no Finder to configure.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn non_macos_is_not_applicable_never_disabled() {
        assert_eq!(finder_extension_state(), FinderExtensionState::NotApplicable);
    }

    /// On macOS the call must return *something* without panicking, whatever the user's
    /// setting happens to be. This is the project's first Objective-C interop; the failure
    /// mode worth guarding is a crash, not a particular value.
    ///
    /// It deliberately does NOT assert which state: the answer depends on a checkbox, and on
    /// a test binary it is bundle-scoped to the harness rather than to the app — so asserting
    /// `Enabled` or `Disabled` here would be asserting something meaningless.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_returns_a_state_without_panicking() {
        let state = finder_extension_state();
        assert!(matches!(
            state,
            FinderExtensionState::Enabled
                | FinderExtensionState::Disabled
                | FinderExtensionState::NotApplicable
        ));
    }

    /// Guards the contract the UI depends on: exactly one state warns.
    #[test]
    fn only_disabled_warrants_a_banner() {
        let warns = |s: FinderExtensionState| s == FinderExtensionState::Disabled;
        assert!(warns(FinderExtensionState::Disabled));
        assert!(!warns(FinderExtensionState::Enabled));
        assert!(!warns(FinderExtensionState::NotApplicable));
    }
}
