//! Registers and removes the File Provider domain (DBSYNC-95 slice 1, ADR-0004). macOS only.
//!
//! Until the cutover slice the domain sits behind a switch, so `main` keeps today's sidecar path:
//!
//! | switch | breadcrumb | action |
//! | --- | --- | --- |
//! | off | none | nothing: **zero File Provider calls**, exactly today's behaviour |
//! | off | present | remove the domain, clear the breadcrumb |
//! | on | any | set the breadcrumb, add the domain (idempotent) |
//!
//! The switch is the per-user defaults key `FileProviderEnabled` in the app's defaults domain, read once
//! at startup (`defaults write com.mobsanchez.dropboxsyncdesktop FileProviderEnabled -bool YES`). It is
//! not an environment variable: a Finder or login-item launch never inherits one. The breadcrumb is an
//! `app_config` row that records "this install registered a domain", so turning the switch off removes
//! the domain, while an install that never opted in makes no File Provider call at all.
//!
//! The calls go through `objc2` + `block2` (no Swift shim); `build.rs` links `FileProvider.framework`,
//! without which `AnyClass::get` silently returns `None`. Nothing in this module's tests reaches the
//! framework or the real user defaults: the defaults read is `None` under `cfg(test)`, and the decision
//! and the sequencing are written against the `DomainOps` and `Breadcrumb` traits that tests replace.

use std::sync::Arc;

use crate::storage::db::Db;

/// The per-user defaults key that turns the domain on. Documented in `FileProviderExtension/README.md`.
pub(crate) const SWITCH_KEY: &str = "FileProviderEnabled";
/// `app_config` key of the breadcrumb.
pub(crate) const BREADCRUMB_KEY: &str = "file_provider_domain_registered";
/// The one value that means "present". Anything else, including an empty string, is "absent".
pub(crate) const BREADCRUMB_VALUE: &str = "registered";
/// Domain identifier; must not contain `/` or `:`.
pub(crate) const DOMAIN_ID: &str = "dropbox-sync";
/// The name shown in Finder's sidebar and used for the `~/Library/CloudStorage` folder.
pub(crate) const DOMAIN_DISPLAY_NAME: &str = "DropboxSync";

/// What a defaults lookup found. Only an explicit boolean `true` turns the switch on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DefaultsValue {
    Absent,
    Bool(bool),
    /// A string, number or anything else stored under the key: not an explicit boolean.
    Other,
}

/// The switch from a defaults lookup: on only for an explicit `true`. A stray string such as `"YES"`
/// written with `-string` is `Other` and stays off, so the switch is never on by accident.
pub(crate) fn switch_from_defaults(value: DefaultsValue) -> bool {
    value == DefaultsValue::Bool(true)
}

/// Whether an `app_config` value is the breadcrumb. Decided once, here: exactly `BREADCRUMB_VALUE`.
pub(crate) fn breadcrumb_present(value: Option<&str>) -> bool {
    value == Some(BREADCRUMB_VALUE)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DomainAction {
    /// Make no File Provider call.
    None,
    /// Add the domain.
    Add,
    /// Remove the domain.
    Remove,
}

/// The whole decision table, as a pure function.
pub(crate) fn decide_domain_action(switch_on: bool, breadcrumb: bool) -> DomainAction {
    match (switch_on, breadcrumb) {
        (true, _) => DomainAction::Add,
        (false, true) => DomainAction::Remove,
        (false, false) => DomainAction::None,
    }
}

/// What happens to the files under the domain when it is removed
/// (`NSFileProviderDomainRemovalMode`, `NSFileProviderManager.h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemovalMode {
    /// `NSFileProviderDomainRemovalModeRemoveAll` (0): deletes the files, including local changes the
    /// host never saw. Never used: kept so the value mapping is visible next to the one in use.
    #[allow(dead_code)]
    RemoveAll,
    /// `NSFileProviderDomainRemovalModePreserveDirtyUserData` (1, macOS 12+): keeps the files with
    /// local changes that were not synced and reports where they went.
    PreserveDirtyUserData,
}

impl RemovalMode {
    /// The `NSInteger` value of the framework enum.
    pub(crate) fn raw(self) -> isize {
        match self {
            RemovalMode::RemoveAll => 0,
            RemovalMode::PreserveDirtyUserData => 1,
        }
    }
}

/// The two File Provider calls, behind a trait so tests never reach the framework.
pub(crate) trait DomainOps {
    fn add(&self) -> Result<(), String>;
    /// Removes the domain in the given mode; a domain that is not registered counts as removed.
    fn remove(&self, mode: RemovalMode) -> Result<(), String>;
}

/// The breadcrumb, behind a trait so the sequencing can be tested with a real or a failing store.
pub(crate) trait Breadcrumb {
    fn is_set(&self) -> Result<bool, String>;
    fn set(&self) -> Result<(), String>;
    fn clear(&self) -> Result<(), String>;
}

impl Breadcrumb for Db {
    fn is_set(&self) -> Result<bool, String> {
        self.get_app_config(BREADCRUMB_KEY)
            .map(|v| breadcrumb_present(v.as_deref()))
            .map_err(|e| e.to_string())
    }
    fn set(&self) -> Result<(), String> {
        self.set_app_config(BREADCRUMB_KEY, BREADCRUMB_VALUE)
            .map_err(|e| e.to_string())
    }
    fn clear(&self) -> Result<(), String> {
        self.delete_app_config(BREADCRUMB_KEY)
            .map_err(|e| e.to_string())
    }
}

/// What `reconcile` did, for the startup log and for tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// No File Provider call was made.
    Untouched,
    Added,
    Removed,
    /// The decision was made but a step failed; the message is for the log. A failed removal leaves the
    /// breadcrumb in place so the next launch retries it.
    Failed(DomainAction, String),
}

/// Reads the breadcrumb, decides, logs the decision once, and carries it out.
///
/// Ordering matters. On add the breadcrumb is written BEFORE the domain is added, so a crash between
/// the two leaves a breadcrumb for a domain that may exist (and a later switch-off removes it) rather
/// than a domain nothing remembers. On remove the breadcrumb is cleared only AFTER the domain is gone,
/// so a failed removal is retried at the next launch instead of being forgotten.
pub(crate) fn reconcile(switch_on: bool, crumb: &impl Breadcrumb, ops: &impl DomainOps) -> Outcome {
    let present = match crumb.is_set() {
        Ok(p) => p,
        Err(e) => {
            // Without the breadcrumb the off path cannot tell "never opted in" from "must remove".
            tracing::error!(error = %e, switch_on, "File Provider: cannot read the breadcrumb; making no File Provider call");
            return Outcome::Untouched;
        }
    };
    let action = decide_domain_action(switch_on, present);
    tracing::info!(
        switch_on,
        breadcrumb = present,
        ?action,
        "File Provider startup decision"
    );
    match action {
        DomainAction::None => Outcome::Untouched,
        DomainAction::Add => {
            if let Err(e) = crumb.set() {
                tracing::error!(error = %e, "File Provider: cannot record the breadcrumb; not adding the domain");
                return Outcome::Failed(action, e);
            }
            match ops.add() {
                Ok(()) => {
                    tracing::info!("File Provider: domain added");
                    Outcome::Added
                }
                Err(e) => {
                    tracing::error!(error = %e, "File Provider: adding the domain failed");
                    Outcome::Failed(action, e)
                }
            }
        }
        // Switching the feature off must never discard the user's unsynced local changes.
        DomainAction::Remove => match ops.remove(RemovalMode::PreserveDirtyUserData) {
            Ok(()) => match crumb.clear() {
                Ok(()) => {
                    tracing::info!("File Provider: domain removed");
                    Outcome::Removed
                }
                Err(e) => {
                    tracing::error!(error = %e, "File Provider: domain removed but the breadcrumb could not be cleared");
                    Outcome::Failed(action, e)
                }
            },
            Err(e) => {
                tracing::error!(error = %e, "File Provider: removing the domain failed; will retry at next launch");
                Outcome::Failed(action, e)
            }
        },
    }
}

/// Startup entry point, called from `lib.rs` `setup` once the database exists. The switch is read here,
/// once; the File Provider calls run on their own thread so a slow `fileproviderd` never delays startup.
pub(crate) fn start(db: Arc<Db>) {
    let switch_on = read_switch();
    let spawned = std::thread::Builder::new()
        .name("file-provider-domain".into())
        .spawn(move || {
            // The thread has no run loop, so Foundation objects autoreleased by the calls would otherwise
            // be held until it exits.
            objc2::rc::autoreleasepool(|_| {
                reconcile(switch_on, &*db, &system::SystemDomain);
            });
        });
    if let Err(e) = spawned {
        tracing::error!(error = %e, "File Provider: could not start the domain thread");
    }
}

/// Reads `FileProviderEnabled` from the app's own defaults domain. Under `cfg(test)` this is always off:
/// no unit test may read the real user defaults.
#[cfg(not(test))]
fn read_switch() -> bool {
    switch_from_defaults(system::read_defaults_value(SWITCH_KEY))
}

#[cfg(test)]
fn read_switch() -> bool {
    false
}

#[cfg(not(test))]
mod system {
    //! The only unsafe code: `msg_send!` calls to `NSUserDefaults` and `NSFileProviderManager`.
    use std::ffi::CString;
    use std::sync::mpsc;
    use std::time::Duration;

    use block2::RcBlock;
    use objc2::msg_send;
    use objc2::rc::{autoreleasepool, Allocated, Retained};
    use objc2::runtime::{AnyClass, AnyObject};

    use super::{DefaultsValue, DomainOps, RemovalMode, DOMAIN_DISPLAY_NAME, DOMAIN_ID};

    /// How long to wait for a File Provider completion handler before giving up.
    const CALL_TIMEOUT: Duration = Duration::from_secs(30);

    pub(super) struct SystemDomain;

    fn ns_string(s: &str) -> Option<Retained<AnyObject>> {
        let c = CString::new(s).ok()?;
        let cls = AnyClass::get(c"NSString")?;
        // SAFETY: `stringWithUTF8String:` takes a NUL-terminated C string that `c` outlives and returns
        // an autoreleased NSString, which `Retained` takes ownership of by retaining it.
        unsafe { msg_send![cls, stringWithUTF8String: c.as_ptr()] }
    }

    fn to_rust_string(obj: *mut AnyObject) -> Option<String> {
        if obj.is_null() {
            return None;
        }
        // SAFETY: `obj` is a live NSString for the duration of the caller's block or pool;
        // `UTF8String` returns a NUL-terminated buffer that is copied out immediately.
        unsafe {
            let utf8: *const std::ffi::c_char = msg_send![obj, UTF8String];
            if utf8.is_null() {
                return None;
            }
            Some(
                std::ffi::CStr::from_ptr(utf8)
                    .to_string_lossy()
                    .into_owned(),
            )
        }
    }

    /// `domain=… code=… description` of an NSError, for the log.
    fn describe_error(err: *mut AnyObject) -> String {
        // SAFETY: `err` is a non-null NSError, live while the completion handler runs.
        unsafe {
            let code: isize = msg_send![err, code];
            let domain: *mut AnyObject = msg_send![err, domain];
            let desc: *mut AnyObject = msg_send![err, localizedDescription];
            format!(
                "domain={} code={} {}",
                to_rust_string(domain).unwrap_or_default(),
                code,
                to_rust_string(desc).unwrap_or_default()
            )
        }
    }

    pub(super) fn read_defaults_value(key: &str) -> DefaultsValue {
        autoreleasepool(|_| {
            let (Some(cls), Some(k)) = (AnyClass::get(c"NSUserDefaults"), ns_string(key)) else {
                return DefaultsValue::Absent;
            };
            // SAFETY: `standardUserDefaults` and `objectForKey:` are documented Foundation API with
            // the signatures used; every object is read inside this pool.
            unsafe {
                let defaults: *mut AnyObject = msg_send![cls, standardUserDefaults];
                if defaults.is_null() {
                    return DefaultsValue::Absent;
                }
                let value: *mut AnyObject = msg_send![defaults, objectForKey: &*k];
                if value.is_null() {
                    return DefaultsValue::Absent;
                }
                let Some(number) = AnyClass::get(c"NSNumber") else {
                    return DefaultsValue::Other;
                };
                let is_number: bool = msg_send![value, isKindOfClass: number];
                if !is_number {
                    return DefaultsValue::Other;
                }
                // `-bool YES` is stored as a boolean NSNumber; an integer 1 would read the same, which
                // is acceptable: both are an explicit, deliberate write.
                let b: bool = msg_send![value, boolValue];
                DefaultsValue::Bool(b)
            }
        })
    }

    fn new_domain() -> Result<Retained<AnyObject>, String> {
        let cls = AnyClass::get(c"NSFileProviderDomain")
            .ok_or("NSFileProviderDomain is not available (is FileProvider linked?)")?;
        let id = ns_string(DOMAIN_ID).ok_or("cannot build the domain identifier")?;
        let name = ns_string(DOMAIN_DISPLAY_NAME).ok_or("cannot build the domain name")?;
        // SAFETY: `alloc` + `initWithIdentifier:displayName:` is the documented initializer; both
        // arguments are live NSStrings.
        let domain: Option<Retained<AnyObject>> = unsafe {
            let alloc: Allocated<AnyObject> = msg_send![cls, alloc];
            msg_send![alloc, initWithIdentifier: &*id, displayName: &*name]
        };
        domain.ok_or_else(|| "NSFileProviderDomain init returned nil".to_string())
    }

    fn manager_class() -> Result<&'static AnyClass, String> {
        AnyClass::get(c"NSFileProviderManager").ok_or_else(|| {
            "NSFileProviderManager is not available (is FileProvider linked?)".to_string()
        })
    }

    /// Sends `+[NSFileProviderManager addDomain:completionHandler:]` and waits.
    fn add_domain(domain: &AnyObject) -> Result<(), String> {
        let cls = manager_class()?;
        let (tx, rx) = mpsc::channel::<Result<(), String>>();
        let tx = std::sync::Mutex::new(Some(tx));
        let block = RcBlock::new(move |err: *mut AnyObject| {
            let result = if err.is_null() {
                Ok(())
            } else {
                Err(describe_error(err))
            };
            if let Ok(mut guard) = tx.lock() {
                if let Some(tx) = guard.take() {
                    let _ = tx.send(result);
                }
            }
        });
        // SAFETY: `addDomain:completionHandler:` is a class method taking an NSFileProviderDomain and a
        // `void (^)(NSError *)` block (Manager.h:229-234). The block is retained by the framework until
        // it runs; `domain` is live for the call.
        unsafe {
            let _: () = msg_send![cls, addDomain: domain, completionHandler: &*block];
        }
        rx.recv_timeout(CALL_TIMEOUT)
            .map_err(|_| "timed out waiting for the File Provider completion handler".to_string())?
    }

    /// Sends `+[NSFileProviderManager removeDomain:mode:completionHandler:]` and waits. The plain
    /// `removeDomain:completionHandler:` is RemoveAll mode and would delete unsynced local changes.
    fn remove_domain(domain: &AnyObject, mode: RemovalMode) -> Result<(), String> {
        let cls = manager_class()?;
        let (tx, rx) = mpsc::channel::<Result<(), String>>();
        let tx = std::sync::Mutex::new(Some(tx));
        let block = RcBlock::new(move |preserved: *mut AnyObject, err: *mut AnyObject| {
            if !preserved.is_null() {
                // SAFETY: `preserved` is a live NSURL while the completion handler runs; `path` returns an NSString.
                let path: *mut AnyObject = unsafe { msg_send![preserved, path] };
                tracing::info!(
                    path = to_rust_string(path).unwrap_or_default(),
                    "File Provider: the removal preserved local data here"
                );
            }
            let result = if err.is_null() {
                Ok(())
            } else {
                Err(describe_error(err))
            };
            if let Ok(mut guard) = tx.lock() {
                if let Some(tx) = guard.take() {
                    let _ = tx.send(result);
                }
            }
        });
        // SAFETY: `removeDomain:mode:completionHandler:` is a class method (macOS 12+) taking an
        // NSFileProviderDomain, an NSInteger enum and a `void (^)(NSURL *, NSError *)` block
        // (NSFileProviderManager.h:239). The block is retained by the framework until it runs; `domain`
        // is live for the call.
        unsafe {
            let _: () = msg_send![
                cls,
                removeDomain: domain,
                mode: mode.raw(),
                completionHandler: &*block
            ];
        }
        rx.recv_timeout(CALL_TIMEOUT)
            .map_err(|_| "timed out waiting for the File Provider completion handler".to_string())?
    }

    /// Whether `DOMAIN_ID` is among `+[NSFileProviderManager getDomainsWithCompletionHandler:]`.
    fn domain_is_registered() -> Result<bool, String> {
        let cls = manager_class()?;
        let (tx, rx) = mpsc::channel::<Result<bool, String>>();
        let tx = std::sync::Mutex::new(Some(tx));
        let block = RcBlock::new(move |domains: *mut AnyObject, err: *mut AnyObject| {
            let result = if !err.is_null() {
                Err(describe_error(err))
            } else if domains.is_null() {
                Ok(false)
            } else {
                // SAFETY: `domains` is a live NSArray of NSFileProviderDomain for the duration of this
                // block; `count`, `objectAtIndex:` and `identifier` are documented API.
                unsafe {
                    let count: usize = msg_send![domains, count];
                    let mut found = false;
                    for i in 0..count {
                        let d: *mut AnyObject = msg_send![domains, objectAtIndex: i];
                        let id: *mut AnyObject = msg_send![d, identifier];
                        if to_rust_string(id).as_deref() == Some(DOMAIN_ID) {
                            found = true;
                            break;
                        }
                    }
                    Ok(found)
                }
            };
            if let Ok(mut guard) = tx.lock() {
                if let Some(tx) = guard.take() {
                    let _ = tx.send(result);
                }
            }
        });
        // SAFETY: class method with one block argument `void (^)(NSArray *, NSError *)` (Manager.h:244).
        unsafe {
            let _: () = msg_send![cls, getDomainsWithCompletionHandler: &*block];
        }
        rx.recv_timeout(CALL_TIMEOUT)
            .map_err(|_| "timed out waiting for getDomains".to_string())?
    }

    impl DomainOps for SystemDomain {
        fn add(&self) -> Result<(), String> {
            let domain = new_domain()?;
            add_domain(&domain)
        }

        fn remove(&self, mode: RemovalMode) -> Result<(), String> {
            // Removing a domain that is not registered is an error from the framework; for the switch-off
            // path that state is already the goal, so it counts as removed.
            if !domain_is_registered()? {
                return Ok(());
            }
            let domain = new_domain()?;
            remove_domain(&domain, mode)
        }
    }
}

#[cfg(test)]
mod system {
    //! Inert stand-ins: no unit test may reach FileProvider or the user defaults.
    use super::{DomainOps, RemovalMode};

    pub(super) struct SystemDomain;

    impl DomainOps for SystemDomain {
        fn add(&self) -> Result<(), String> {
            panic!("a unit test reached the real File Provider add");
        }
        fn remove(&self, _mode: RemovalMode) -> Result<(), String> {
            panic!("a unit test reached the real File Provider remove");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[test]
    fn the_decision_table_covers_every_state() {
        assert_eq!(decide_domain_action(false, false), DomainAction::None);
        assert_eq!(decide_domain_action(false, true), DomainAction::Remove);
        assert_eq!(decide_domain_action(true, false), DomainAction::Add);
        assert_eq!(decide_domain_action(true, true), DomainAction::Add);
    }

    #[test]
    fn only_an_explicit_boolean_true_turns_the_switch_on() {
        assert!(switch_from_defaults(DefaultsValue::Bool(true)));
        assert!(!switch_from_defaults(DefaultsValue::Bool(false)));
        assert!(!switch_from_defaults(DefaultsValue::Absent));
        assert!(!switch_from_defaults(DefaultsValue::Other));
    }

    #[test]
    fn the_breadcrumb_is_exactly_one_value() {
        assert!(breadcrumb_present(Some("registered")));
        for v in [
            None,
            Some(""),
            Some("0"),
            Some("true"),
            Some("Registered"),
            Some(" registered"),
        ] {
            assert!(!breadcrumb_present(v), "{v:?}");
        }
    }

    #[test]
    fn the_domain_identifier_has_no_forbidden_characters() {
        assert!(!DOMAIN_ID.contains('/') && !DOMAIN_ID.contains(':') && !DOMAIN_ID.is_empty());
    }

    /// The key is a user-facing contract (README, ADR-0004, `defaults write`): renaming it silently
    /// would leave everyone's switch off.
    #[test]
    fn the_switch_key_and_the_display_name_are_the_documented_ones() {
        assert_eq!(SWITCH_KEY, "FileProviderEnabled");
        assert_eq!(DOMAIN_DISPLAY_NAME, "DropboxSync");
    }

    #[derive(Default)]
    struct FakeOps {
        calls: RefCell<Vec<&'static str>>,
        remove_modes: RefCell<Vec<RemovalMode>>,
        fail_add: Cell<bool>,
        fail_remove: Cell<bool>,
    }
    impl DomainOps for FakeOps {
        fn add(&self) -> Result<(), String> {
            self.calls.borrow_mut().push("add");
            if self.fail_add.get() {
                Err("add failed".into())
            } else {
                Ok(())
            }
        }
        fn remove(&self, mode: RemovalMode) -> Result<(), String> {
            self.calls.borrow_mut().push("remove");
            self.remove_modes.borrow_mut().push(mode);
            if self.fail_remove.get() {
                Err("remove failed".into())
            } else {
                Ok(())
            }
        }
    }

    struct FakeCrumb {
        present: Cell<bool>,
        fail_read: Cell<bool>,
        fail_set: Cell<bool>,
        events: RefCell<Vec<&'static str>>,
    }
    impl FakeCrumb {
        fn new(present: bool) -> Self {
            Self {
                present: Cell::new(present),
                fail_read: Cell::new(false),
                fail_set: Cell::new(false),
                events: RefCell::new(Vec::new()),
            }
        }
    }
    impl Breadcrumb for FakeCrumb {
        fn is_set(&self) -> Result<bool, String> {
            if self.fail_read.get() {
                Err("read failed".into())
            } else {
                Ok(self.present.get())
            }
        }
        fn set(&self) -> Result<(), String> {
            self.events.borrow_mut().push("set");
            if self.fail_set.get() {
                return Err("set failed".into());
            }
            self.present.set(true);
            Ok(())
        }
        fn clear(&self) -> Result<(), String> {
            self.events.borrow_mut().push("clear");
            self.present.set(false);
            Ok(())
        }
    }

    #[test]
    fn off_without_a_breadcrumb_makes_no_file_provider_call_and_writes_nothing() {
        let (crumb, ops) = (FakeCrumb::new(false), FakeOps::default());
        assert_eq!(reconcile(false, &crumb, &ops), Outcome::Untouched);
        assert!(ops.calls.borrow().is_empty());
        assert!(crumb.events.borrow().is_empty());
    }

    #[test]
    fn off_with_a_breadcrumb_removes_the_domain_and_clears_the_breadcrumb() {
        let (crumb, ops) = (FakeCrumb::new(true), FakeOps::default());
        assert_eq!(reconcile(false, &crumb, &ops), Outcome::Removed);
        assert_eq!(*ops.calls.borrow(), ["remove"]);
        assert!(!crumb.present.get());
    }

    #[test]
    fn removal_asks_to_preserve_unsynced_local_data() {
        let crumb = FakeCrumb::new(true);
        let ops = FakeOps::default();
        reconcile(false, &crumb, &ops);
        assert_eq!(
            *ops.remove_modes.borrow(),
            [RemovalMode::PreserveDirtyUserData]
        );
    }

    #[test]
    fn the_removal_modes_carry_the_framework_enum_values() {
        // NSFileProviderDomainRemovalModeRemoveAll = 0, ...PreserveDirtyUserData = 1 (NSFileProviderManager.h).
        assert_eq!(RemovalMode::RemoveAll.raw(), 0);
        assert_eq!(RemovalMode::PreserveDirtyUserData.raw(), 1);
    }

    #[test]
    fn on_adds_the_domain_and_sets_the_breadcrumb_first() {
        let (crumb, ops) = (FakeCrumb::new(false), FakeOps::default());
        assert_eq!(reconcile(true, &crumb, &ops), Outcome::Added);
        assert_eq!(*ops.calls.borrow(), ["add"]);
        assert!(crumb.present.get());
    }

    #[test]
    fn on_again_adds_again_which_is_idempotent_for_the_framework() {
        let (crumb, ops) = (FakeCrumb::new(true), FakeOps::default());
        assert_eq!(reconcile(true, &crumb, &ops), Outcome::Added);
        assert_eq!(*ops.calls.borrow(), ["add"]);
        assert!(crumb.present.get());
    }

    #[test]
    fn a_failed_removal_keeps_the_breadcrumb_so_the_next_launch_retries() {
        let (crumb, ops) = (FakeCrumb::new(true), FakeOps::default());
        ops.fail_remove.set(true);
        assert!(matches!(
            reconcile(false, &crumb, &ops),
            Outcome::Failed(DomainAction::Remove, _)
        ));
        assert!(crumb.present.get());
        assert!(
            crumb.events.borrow().is_empty(),
            "clear must not run after a failed removal"
        );
    }

    #[test]
    fn a_failed_breadcrumb_write_means_the_domain_is_not_added() {
        let (crumb, ops) = (FakeCrumb::new(false), FakeOps::default());
        crumb.fail_set.set(true);
        assert!(matches!(
            reconcile(true, &crumb, &ops),
            Outcome::Failed(DomainAction::Add, _)
        ));
        assert!(ops.calls.borrow().is_empty());
    }

    #[test]
    fn a_failed_add_leaves_the_breadcrumb_so_a_later_switch_off_removes_the_domain() {
        let (crumb, ops) = (FakeCrumb::new(false), FakeOps::default());
        ops.fail_add.set(true);
        assert!(matches!(
            reconcile(true, &crumb, &ops),
            Outcome::Failed(DomainAction::Add, _)
        ));
        assert!(crumb.present.get());
    }

    #[test]
    fn an_unreadable_breadcrumb_makes_no_file_provider_call_whatever_the_switch() {
        for switch_on in [false, true] {
            let (crumb, ops) = (FakeCrumb::new(true), FakeOps::default());
            crumb.fail_read.set(true);
            assert_eq!(reconcile(switch_on, &crumb, &ops), Outcome::Untouched);
            assert!(ops.calls.borrow().is_empty());
        }
    }

    /// The breadcrumb through the real database, not a fake: set, read, clear, and a stale value.
    #[test]
    fn the_database_breadcrumb_round_trips() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = Db::new_at(&tmp.path().join("app.db")).expect("db");
        assert_eq!(db.is_set(), Ok(false));
        db.set().expect("set");
        assert_eq!(db.is_set(), Ok(true));
        db.clear().expect("clear");
        assert_eq!(db.is_set(), Ok(false));
        db.clear().expect("clearing an absent key is not an error");
        db.set_app_config(BREADCRUMB_KEY, "").expect("write");
        assert_eq!(db.is_set(), Ok(false), "an empty value is not a breadcrumb");
    }

    /// Whole sequence through the real database: off/none -> on -> off.
    #[test]
    fn switching_on_then_off_leaves_no_breadcrumb_and_calls_add_then_remove() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = Db::new_at(&tmp.path().join("app.db")).expect("db");
        let ops = FakeOps::default();
        assert_eq!(reconcile(false, &db, &ops), Outcome::Untouched);
        assert_eq!(reconcile(true, &db, &ops), Outcome::Added);
        assert_eq!(reconcile(false, &db, &ops), Outcome::Removed);
        assert_eq!(reconcile(false, &db, &ops), Outcome::Untouched);
        assert_eq!(*ops.calls.borrow(), ["add", "remove"]);
    }
}
