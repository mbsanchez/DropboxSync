//! Opens the app at login on macOS (DBSYNC-95 slice 3, ADR-0004), so the File Provider extension rarely
//! finds the host down. macOS only.
//!
//! `SMAppService.mainApp` is the sole source of truth for the state, as `StartupTask` is on Windows:
//! nothing mirrors it. The one thing recorded is that the default was applied, so a user who turned
//! it off is not turned back on at the next launch.
//!
//! This all belongs to the File Provider switch. With it off and never on, a launch changes nothing
//! and Settings shows no toggle. Turning it off undoes what the default did, including when the app
//! was set to open at login by hand, since that toggle exists only behind the same switch.

use crate::storage::db::Db;

/// `app_config` key: the default has been applied once.
const DEFAULT_APPLIED_KEY: &str = "login_item_default_applied";

/// What a launch does.
#[derive(Debug, PartialEq, Eq)]
enum DefaultAction {
    None,
    Register,
    Unregister,
}

fn decide(switch_on: bool, applied: bool) -> DefaultAction {
    match (switch_on, applied) {
        (true, false) => DefaultAction::Register,
        (false, true) => DefaultAction::Unregister,
        _ => DefaultAction::None,
    }
}

/// Applies the default once when the switch is on, and undoes it when the switch goes off. The record
/// changes only after a call that worked, so a failure is tried again at the next launch.
pub(crate) fn apply_default(db: &Db, switch_on: bool) {
    let applied = match db.get_app_config(DEFAULT_APPLIED_KEY) {
        Ok(value) => value.is_some(),
        Err(e) => {
            tracing::warn!(error = %e, "login item: cannot read whether the default was applied");
            return;
        }
    };
    let recorded = match decide(switch_on, applied) {
        DefaultAction::None => return,
        DefaultAction::Register => system::set_enabled(true).and_then(|()| {
            db.set_app_config(DEFAULT_APPLIED_KEY, "1")
                .map_err(|e| e.to_string())
        }),
        DefaultAction::Unregister => system::set_enabled(false).and_then(|()| {
            db.delete_app_config(DEFAULT_APPLIED_KEY)
                .map_err(|e| e.to_string())
        }),
    };
    match recorded {
        Ok(()) => tracing::info!(switch_on, "login item: default applied"),
        Err(e) => tracing::warn!(error = %e, switch_on, "login item: could not apply the default"),
    }
}

/// The toggle exists only behind the switch: an error is how Settings learns to hide it.
pub(crate) fn is_enabled(switch_on: bool) -> Result<bool, String> {
    if !switch_on {
        return Err("start at login comes with File Provider".into());
    }
    system::is_enabled()
}

/// The state after the change, which may differ from the request: the user can veto a login item in
/// System Settings, which leaves it waiting for approval.
pub(crate) fn set_enabled(switch_on: bool, enabled: bool) -> Result<bool, String> {
    if !switch_on {
        return Err("start at login comes with File Provider".into());
    }
    system::set_enabled(enabled)?;
    system::is_enabled()
}

#[cfg(not(test))]
mod system {
    //! The only unsafe code: `msg_send!` calls to `SMAppService`, checked against the SDK's
    //! `SMAppService.h`.
    use objc2::msg_send;
    use objc2::rc::autoreleasepool;
    use objc2::runtime::{AnyClass, AnyObject};
    use std::ffi::{c_char, CStr};

    /// `SMAppServiceStatusEnabled`.
    const STATUS_ENABLED: isize = 1;

    fn main_app() -> Result<*mut AnyObject, String> {
        let class = AnyClass::get(c"SMAppService")
            .ok_or("SMAppService is not available: ServiceManagement is not linked")?;
        // SAFETY: a class property of `SMAppService` returning an autoreleased instance, used inside
        // the caller's pool.
        let service: *mut AnyObject = unsafe { msg_send![class, mainAppService] };
        if service.is_null() {
            return Err("mainAppService returned nil".into());
        }
        Ok(service)
    }

    fn describe(error: *mut AnyObject) -> String {
        if error.is_null() {
            return "no error given".into();
        }
        // SAFETY: `error` is a live NSError; `localizedDescription` and `UTF8String` are documented
        // API, and the bytes are copied out while the pool holds them.
        unsafe {
            let text: *mut AnyObject = msg_send![error, localizedDescription];
            let utf8: *const c_char = msg_send![text, UTF8String];
            if utf8.is_null() {
                return "no description".into();
            }
            CStr::from_ptr(utf8).to_string_lossy().into_owned()
        }
    }

    pub(super) fn is_enabled() -> Result<bool, String> {
        autoreleasepool(|_| {
            let service = main_app()?;
            // SAFETY: `status` is an NSInteger property of a live `SMAppService`.
            let status: isize = unsafe { msg_send![service, status] };
            Ok(status == STATUS_ENABLED)
        })
    }

    pub(super) fn set_enabled(enabled: bool) -> Result<(), String> {
        autoreleasepool(|_| {
            let service = main_app()?;
            let mut error: *mut AnyObject = std::ptr::null_mut();
            // SAFETY: both selectors take an `NSError **` out parameter and return BOOL. `error` is
            // live for the call, and what it receives is autoreleased into this pool.
            let done: bool = unsafe {
                if enabled {
                    msg_send![service, registerAndReturnError: &raw mut error]
                } else {
                    msg_send![service, unregisterAndReturnError: &raw mut error]
                }
            };
            if done {
                Ok(())
            } else {
                Err(describe(error))
            }
        })
    }
}

#[cfg(test)]
mod system {
    //! Inert stand-ins: no unit test may register a real login item.
    use std::cell::Cell;

    thread_local! {
        pub(super) static SET_CALLS: Cell<usize> = const { Cell::new(0) };
        pub(super) static FAIL: Cell<bool> = const { Cell::new(false) };
    }

    pub(super) fn is_enabled() -> Result<bool, String> {
        Ok(false)
    }

    pub(super) fn set_enabled(_enabled: bool) -> Result<(), String> {
        SET_CALLS.with(|c| c.set(c.get() + 1));
        if FAIL.with(Cell::get) {
            return Err("refused".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::new_at(&dir.path().join("app.db")).unwrap();
        (dir, db)
    }

    fn calls() -> usize {
        system::SET_CALLS.with(std::cell::Cell::get)
    }

    #[test]
    fn with_the_switch_off_a_launch_registers_nothing_and_records_nothing() {
        let (_dir, db) = db();

        apply_default(&db, false);

        assert_eq!(calls(), 0);
        assert_eq!(db.get_app_config(DEFAULT_APPLIED_KEY).unwrap(), None);
    }

    #[test]
    fn turning_the_switch_off_undoes_the_default_once() {
        let (_dir, db) = db();
        apply_default(&db, true);

        apply_default(&db, false);
        apply_default(&db, false);

        assert_eq!(calls(), 2);
        assert_eq!(db.get_app_config(DEFAULT_APPLIED_KEY).unwrap(), None);
    }

    #[test]
    fn with_the_switch_off_the_toggle_does_not_exist() {
        let before = calls();

        assert!(is_enabled(false).is_err());
        assert!(set_enabled(false, true).is_err());

        assert_eq!(calls(), before);
    }

    /// A user who turned it off stays off: the second launch must not register again.
    #[test]
    fn the_default_applies_once() {
        let (_dir, db) = db();

        apply_default(&db, true);
        apply_default(&db, true);

        assert_eq!(calls(), 1);
    }

    #[test]
    fn a_failed_registration_is_tried_again_at_the_next_launch() {
        let (_dir, db) = db();
        system::FAIL.with(|f| f.set(true));

        apply_default(&db, true);
        system::FAIL.with(|f| f.set(false));
        apply_default(&db, true);

        assert_eq!(calls(), 2);
        assert!(db.get_app_config(DEFAULT_APPLIED_KEY).unwrap().is_some());
    }
}
