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
pub(crate) fn finder_extension_state() -> FinderExtensionState {
    #[cfg(target_os = "macos")]
    {
        use objc2::runtime::AnyClass;
        use objc2::msg_send;

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
    #[cfg(not(target_os = "macos"))]
    {
        FinderExtensionState::NotApplicable
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
/// **Never called automatically** — it closes the user's open Finder windows, so it is offered
/// as a button and nothing more. This is a plain process signal, not a `pluginkit` call: the
/// ticket forbids the latter because it depends on undocumented behaviour, while `killall
/// Finder` is documented, reversible and something users do by hand routinely.
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
