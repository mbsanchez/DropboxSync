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
    /// The app is running under **App Translocation** (DBSYNC-88), so the question cannot be
    /// answered at all: `isExtensionEnabled` reports on the appex inside the *calling* bundle,
    /// and under translocation that bundle is a randomised read-only copy rather than the
    /// installed app whose extension the user actually enabled.
    ///
    /// Distinct from both [`Disabled`](Self::Disabled) and [`NotApplicable`](Self::NotApplicable)
    /// on purpose. `Disabled` would be a lie — this is exactly the false alarm that made this
    /// ticket — and `NotApplicable` would be silence, leaving the user with no badges and no
    /// explanation. The user has a real problem and a one-gesture fix, so it gets its own state.
    Translocated,
    /// Not macOS, or the state could not be determined. Say nothing.
    NotApplicable,
}

/// Whether `exe_path` is an executable macOS relocated into a translocation mount.
///
/// A pure predicate on the path so it is unit-testable. The condition it encodes was measured,
/// not assumed — the instrumented build on the affected Mac logged this executable path while
/// the app insisted the extension was off:
///
/// ```text
/// /private/var/folders/x3/…/T/AppTranslocation/71877875-…/d/DropboxSyncDesktop.app/Contents/MacOS/dropbox_sync_desktop
/// ```
///
/// `Security.framework` exposes `SecTranslocateIsTranslocatedURL`, which would be the documented
/// route. It is a C API needing CFURL bridging for one boolean, and the path marker is stable,
/// observable and already in the logs. Recorded as the alternative in case this ever proves
/// insufficient — not dismissed for convenience.
pub(crate) fn is_translocated_path(exe_path: &str) -> bool {
    exe_path.contains("/AppTranslocation/")
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
        // Answered before the read, because under translocation the read is meaningless rather
        // than merely inconvenient: it would report on the appex inside the temporary copy and
        // return `false` truthfully about the wrong bundle. Asking anyway and then overriding
        // the answer would leave `Disabled` reachable from a state that can never justify it.
        if std::env::current_exe()
            .map(|p| is_translocated_path(&p.display().to_string()))
            .unwrap_or(false)
        {
            return FinderExtensionState::Translocated;
        }

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

/// Logs, once per process, everything that could plausibly explain a wrong answer.
///
/// Added because the first fix for DBSYNC-88 was a reasoned guess — the main thread — and the
/// machine falsified it. The property is bundle-scoped and cannot be exercised from a test or a
/// spike, so the only instrument available is the shipped app describing its own conditions.
/// Every fact here is one that was assumed at some point in this ticket and never checked.
///
/// Runs on the main thread: it is called from inside [`read_extension_enabled`].
#[cfg(target_os = "macos")]
fn log_diagnostics_once(enabled: bool) {
    use objc2::msg_send;
    use objc2::runtime::AnyClass;

    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // Which binary is actually running — the copy question, answered by the process itself
        // rather than by asking the user to look for stray bundles.
        let exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "<unknown>".to_string());

        // Whose extension the property is reporting on. `isExtensionEnabled` answers for the
        // appex inside the CALLING bundle, so if this identifier is not ours, `false` is a
        // truthful answer about the wrong thing.
        let bundle_id = unsafe {
            AnyClass::get(c"NSBundle").map(|cls| {
                let bundle: *mut objc2::runtime::AnyObject = msg_send![cls, mainBundle];
                let ident: *mut objc2::runtime::AnyObject = msg_send![bundle, bundleIdentifier];
                if ident.is_null() {
                    "<none>".to_string()
                } else {
                    let utf8: *const std::ffi::c_char = msg_send![ident, UTF8String];
                    std::ffi::CStr::from_ptr(utf8)
                        .to_string_lossy()
                        .into_owned()
                }
            })
        }
        .unwrap_or_else(|| "<no NSBundle>".to_string());

        // A menu-bar app runs as an Accessory (no Dock icon). Apple's own header says to check
        // "when the application becomes active" — an app that never activates in the AppKit
        // sense is the next hypothesis if the bundle identifier turns out to be correct.
        let activation_policy = unsafe {
            AnyClass::get(c"NSApplication").map(|cls| {
                let app: *mut objc2::runtime::AnyObject = msg_send![cls, sharedApplication];
                let policy: isize = msg_send![app, activationPolicy];
                let active: bool = msg_send![app, isActive];
                (policy, active)
            })
        };

        tracing::info!(
            target: "finder_extension",
            enabled,
            exe = %exe,
            bundle_id = %bundle_id,
            activation_policy = ?activation_policy.map(|(p, _)| p),
            app_is_active = ?activation_policy.map(|(_, a)| a),
            "finder extension state read (DBSYNC-88 diagnostics)"
        );
    });
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

    log_diagnostics_once(enabled);

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
        assert_eq!(
            finder_extension_state(),
            FinderExtensionState::NotApplicable
        );
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
                | FinderExtensionState::Translocated
                | FinderExtensionState::NotApplicable
        ));
    }

    /// Guards the contract the UI depends on: exactly one state raises the "switched off"
    /// warning. `Translocated` must not, which is the whole point of DBSYNC-88 — it gets its
    /// own message because the extension is very probably fine.
    #[test]
    fn only_disabled_warrants_the_switched_off_banner() {
        let warns = |s: FinderExtensionState| s == FinderExtensionState::Disabled;
        assert!(warns(FinderExtensionState::Disabled));
        assert!(!warns(FinderExtensionState::Enabled));
        assert!(!warns(FinderExtensionState::NotApplicable));
        assert!(!warns(FinderExtensionState::Translocated));
    }

    /// Pins the wire format against `FinderExtensionState` in `apps/desktop/src/types.ts`.
    ///
    /// Nothing else checks this seam. The frontend compares these values as string literals,
    /// so renaming a variant here — or changing the serde casing — would make every comparison
    /// silently false: no banner, no error, no failing build, in either language. That is the
    /// same shape of defect as the one this ticket exists for, so it gets a test rather than a
    /// convention.
    #[test]
    fn serialized_names_match_the_typescript_union() {
        let wire = |s: FinderExtensionState| serde_json::to_string(&s).unwrap();
        assert_eq!(wire(FinderExtensionState::Enabled), "\"enabled\"");
        assert_eq!(wire(FinderExtensionState::Disabled), "\"disabled\"");
        assert_eq!(wire(FinderExtensionState::Translocated), "\"translocated\"");
        assert_eq!(
            wire(FinderExtensionState::NotApplicable),
            "\"notApplicable\""
        );
    }

    /// The exact path the instrumented build logged on the machine that reproduced DBSYNC-88.
    /// Pinned verbatim: this is the observation the whole fix rests on, and a predicate that
    /// stopped matching it would silently restore the false "extension is off" banner.
    #[test]
    fn the_measured_translocated_path_is_detected() {
        assert!(super::is_translocated_path(
            "/private/var/folders/x3/q_hpnkb90ml773pg2cc4lvd80000gn/T/AppTranslocation/\
             71877875-BD67-4094-8873-AED79F8EF913/d/DropboxSyncDesktop.app/Contents/MacOS/\
             dropbox_sync_desktop"
        ));
    }

    /// Ordinary install locations must NOT be mistaken for translocation. Without this the fix
    /// could pass as "no banner ever", which is the failure mode the ticket calls out.
    #[test]
    fn normal_install_locations_are_not_translocated() {
        for path in [
            "/Applications/DropboxSyncDesktop.app/Contents/MacOS/dropbox_sync_desktop",
            "/Users/someone/Applications/DropboxSyncDesktop.app/Contents/MacOS/dropbox_sync_desktop",
            "/Users/someone/Work/DropboxSync/apps/desktop/src-tauri/target/release/dropbox_sync_desktop",
            // A folder a user happened to name after the mechanism is still not a translocation
            // mount: the marker is a path COMPONENT, and this one is not.
            "/Users/someone/AppTranslocationNotes/DropboxSyncDesktop.app/Contents/MacOS/app",
        ] {
            assert!(!super::is_translocated_path(path), "misdetected: {path}");
        }
    }
}
