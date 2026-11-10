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
//!
//! ## The projection (DBSYNC-95 slice 2a)
//!
//! The same switch also decides whether the host feeds the projection of the remote tree that the
//! extension reads (`storage/db/projection.rs`). `start` records it on the `Db`
//! (`Db::projection_active`), clears the projection when it is off and holds something, and, when it is
//! on and the projection is unpopulated, leaves the stored cursor alone (`usable_delta_cursor` treats it
//! as absent, so the loop seeds, and a failed seed falls back to it). After a commit the sync thread calls [`signal_working_set`], which
//! only hands a unit to a worker thread: the worker resolves the domain and asks the system to
//! re-enumerate the working set, and the sync thread never waits for it.

use std::sync::Arc;
use std::time::{Duration, Instant};

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

/// What the startup decision did to the projection, for the log and for tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SwitchEffect {
    /// Nothing was written.
    Untouched,
    /// The switch is off and the projection held rows or was populated: it was emptied.
    Cleared,
    /// The switch is on and the projection is not populated. Nothing was written: the stored delta
    /// cursor stays, and `usable_delta_cursor` treats it as absent beside an unpopulated projection,
    /// so the next loop iteration seeds, and a failed seed falls back to the stored cursor.
    SeedPending,
}

/// The projection half of the startup decision. Off: clear the projection, but only when it holds
/// something, found by a read, so a switch that was never on never opens a write transaction. On: write
/// nothing; an unpopulated projection is reported, and seeded by the loop (see [`SwitchEffect::SeedPending`]).
pub(crate) fn apply_switch(db: &Db, switch_on: bool) -> Result<SwitchEffect, String> {
    if !switch_on {
        if !db.projection_holds_anything().map_err(|e| e.to_string())? {
            return Ok(SwitchEffect::Untouched);
        }
        let commit = db.project_clear().map_err(|e| e.to_string())?;
        return Ok(if commit.rows > 0 {
            SwitchEffect::Cleared
        } else {
            SwitchEffect::Untouched
        });
    }
    let state = db.projection_state().map_err(|e| e.to_string())?;
    Ok(if state.populated {
        SwitchEffect::Untouched
    } else {
        SwitchEffect::SeedPending
    })
}

/// Startup entry point, called from `lib.rs` `setup` once the database exists. The switch is read here,
/// once, and recorded on the `Db` before this returns (the remote loop starts after it, so it never
/// sees a stale value); the File Provider calls run on their own thread so a slow `fileproviderd`
/// never delays startup.
pub(crate) fn start(db: Arc<Db>) {
    let switch_on = read_switch();
    db.set_projection_enabled(switch_on);
    match apply_switch(&db, switch_on) {
        Ok(effect) => tracing::info!(
            switch_on,
            ?effect,
            "File Provider projection startup decision"
        ),
        Err(e) => {
            tracing::error!(error = %e, "File Provider: could not apply the switch to the projection")
        }
    }
    let spawned = std::thread::Builder::new()
        .name("file-provider-domain".into())
        .spawn(move || {
            // The thread has no run loop, so Foundation objects autoreleased by the calls would otherwise
            // be held until it exits.
            objc2::rc::autoreleasepool(|_| {
                reconcile(switch_on, &*db, &system::SystemDomain);
            });
            // The projection may have been populated by an earlier run: tell the system once, so a
            // domain mounted after the last commit enumerates it.
            if switch_on && db.projection_state().is_ok_and(|s| s.populated) {
                signal_working_set();
            }
        });
    if let Err(e) = spawned {
        tracing::error!(error = %e, "File Provider: could not start the domain thread");
    }
}

/// Tells the system that `serverUnreachable` and `notAuthenticated` may have passed, which cancels its
/// throttling of every item that got one. Blocks on the framework, so the caller is not the sync thread;
/// a caller that is uses [`signal_errors_resolved_soon`].
pub(crate) fn signal_errors_resolved() {
    if let Err(e) = system::signal_errors_resolved() {
        tracing::warn!(error = %e, "File Provider: could not signal that errors resolved");
    }
}

/// [`signal_errors_resolved`] on its own thread, for the sync thread: a projection that just became
/// populated cures what `notPopulated` answered with `serverUnreachable`, and a working-set signal does
/// not lift that throttling.
pub(crate) fn signal_errors_resolved_soon() {
    #[cfg(test)]
    system::note_errors_resolved();
    #[cfg(not(test))]
    if let Err(e) = std::thread::Builder::new()
        .name("file-provider-resolve-seed".into())
        .spawn(signal_errors_resolved)
    {
        tracing::error!(error = %e, "File Provider: could not signal that errors resolved");
    }
}

/// How many times this test thread asked for that lift.
#[cfg(test)]
pub(crate) fn errors_resolved_on_this_thread() -> usize {
    system::errors_resolved()
}

/// Collapses a burst of signal requests into one pending signal. `request` says whether the caller
/// has to wake the worker (nobody else has); the worker calls `begin` BEFORE it signals, so a request
/// that arrives while it is signalling is not lost: it sets the flag again and wakes the worker once
/// more.
pub(crate) struct SignalGate {
    pending: std::sync::atomic::AtomicBool,
}

impl SignalGate {
    pub(crate) const fn new() -> Self {
        Self {
            pending: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub(crate) fn request(&self) -> bool {
        !self.pending.swap(true, std::sync::atomic::Ordering::AcqRel)
    }

    pub(crate) fn begin(&self) {
        self.pending
            .store(false, std::sync::atomic::Ordering::Release);
    }
}

/// The one gate in front of the signal worker. Only the real worker uses it; a test builds its own.
#[cfg(not(test))]
static SIGNAL_GATE: SignalGate = SignalGate::new();

/// How long the signal waits after a delta removed something. Dropbox can deliver a folder rename in
/// two invocations, every delete in the first (`has_more=false`, nothing announces the second) and every
/// re-add in the next one. Measured live: 2.9 s for a folder of 204 entries (#234), and DBSYNC-102
/// recorded 3.45 s and 3.43 s for the same split on a share and an un-share. Ten seconds is about three
/// times that; what it costs is a remote delete reaching Finder ten seconds late.
pub(crate) const SIGNAL_REMOVAL_GRACE: Duration = Duration::from_secs(10);

/// The longest a run of removals can hold the signal, so that a steady stream of deletes cannot keep
/// the system from ever hearing about them.
pub(crate) const SIGNAL_REMOVAL_MAX_HOLD: Duration = Duration::from_secs(60);

/// When the worker may signal after a removal. This narrows the window, it does not close it: the
/// system may still enumerate during the hold for a reason of its own, and it then sees the rows the
/// first half removed. Only the signal is held, never the projection.
pub(crate) struct SignalHold {
    /// The first removal of the run and the end of the hold; `None` when nothing is held.
    window: std::sync::Mutex<Option<(Instant, Instant)>>,
}

impl SignalHold {
    pub(crate) const fn new() -> Self {
        Self {
            window: std::sync::Mutex::new(None),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<(Instant, Instant)>> {
        self.window
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Starts the hold, or moves its end later, never past [`SIGNAL_REMOVAL_MAX_HOLD`] from its start.
    pub(crate) fn extend(&self, now: Instant) {
        let mut window = self.lock();
        let first = window.map_or(now, |(first, _)| first);
        let until = (now + SIGNAL_REMOVAL_GRACE).min(first + SIGNAL_REMOVAL_MAX_HOLD);
        *window = Some((first, until));
    }

    /// What is left of the hold. Zero ends it, so the next removal starts a new run.
    pub(crate) fn remaining(&self, now: Instant) -> Duration {
        let mut window = self.lock();
        match *window {
            Some((_, until)) if until > now => until - now,
            _ => {
                *window = None;
                Duration::ZERO
            }
        }
    }
}

/// The worker's wait before it signals. It re-opens the gate before each look at the hold, so a request
/// that lands during the wait wakes the worker once more instead of being lost, and a hold that begins
/// after the last look is seen by that wake-up.
pub(crate) fn wait_out_hold(
    gate: &SignalGate,
    hold: &SignalHold,
    now: impl Fn() -> Instant,
    mut sleep: impl FnMut(Duration),
) {
    loop {
        gate.begin();
        let wait = hold.remaining(now());
        if wait.is_zero() {
            return;
        }
        sleep(wait);
    }
}

/// The one hold in front of the signal worker. Only the real worker uses it; a test builds its own.
#[cfg(not(test))]
static SIGNAL_HOLD: SignalHold = SignalHold::new();

/// Whether a removal is still being held back from the system: it may be the first half of a split
/// rename, and the rows it removed may come back with their ids (ADR-0004, *A rename split in two*).
#[cfg(not(test))]
pub(crate) fn removal_held() -> bool {
    !SIGNAL_HOLD.remaining(Instant::now()).is_zero()
}

/// Tests hold nothing.
#[cfg(test)]
pub(crate) fn removal_held() -> bool {
    false
}

/// What a signal outcome means for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SignalReport {
    /// Same as last time: say nothing.
    Quiet,
    /// It started failing, or fails for a different reason: log it once.
    Failing,
    /// It works again after failing: log it once.
    Recovered,
}

/// Turns the stream of signal outcomes into log transitions. With the switch on and no domain
/// registered every delta fails the same way, and a WARN per delta would bury the log; the Swift
/// extension follows the same discipline (log on a transition, not on every repeat).
pub(crate) struct SignalOutcomeLog {
    /// `None`: nothing yet or the last one worked. `Some(reason)`: failing with that reason.
    failing: std::sync::Mutex<Option<String>>,
}

impl SignalOutcomeLog {
    pub(crate) const fn new() -> Self {
        Self {
            failing: std::sync::Mutex::new(None),
        }
    }

    pub(crate) fn note(&self, outcome: &Result<(), String>) -> SignalReport {
        let mut failing = self
            .failing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match (outcome, failing.as_ref()) {
            (Ok(()), None) => SignalReport::Quiet,
            (Ok(()), Some(_)) => {
                *failing = None;
                SignalReport::Recovered
            }
            (Err(e), Some(last)) if last == e => SignalReport::Quiet,
            (Err(e), _) => {
                *failing = Some(e.clone());
                SignalReport::Failing
            }
        }
    }
}

#[cfg(not(test))]
static SIGNAL_OUTCOMES: SignalOutcomeLog = SignalOutcomeLog::new();

/// Asks the system to re-enumerate the working set, which makes it call `enumerateChanges`. Fire and
/// forget: it returns at once, whatever `fileproviderd` is doing, and a burst of calls is one signal.
pub(crate) fn signal_working_set() {
    system::request_signal();
}

/// [`signal_working_set`] after a delta commit, holding the signal when the delta removed something,
/// because the second half of a rename may be one invocation away (see [`SIGNAL_REMOVAL_GRACE`]).
pub(crate) fn signal_after_delta(removed_something: bool) {
    if removed_something {
        system::hold_signal();
    }
    system::request_signal();
}

/// How many working-set signals this test thread has asked to hold.
#[cfg(test)]
pub(crate) fn signal_holds_on_this_thread() -> usize {
    system::holds()
}

/// How many working-set signals this test thread has asked for.
#[cfg(test)]
pub(crate) fn signals_requested_on_this_thread() -> usize {
    system::requested()
}

/// [`signal_working_set`], but only when the projection is being fed.
pub(crate) fn signal_if_active(db: &Db) {
    if db.projection_active() {
        signal_working_set();
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
    use std::time::{Duration, Instant};

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
        let domain = domain.ok_or_else(|| "NSFileProviderDomain init returned nil".to_string())?;
        // Finder's Move to Trash (Cmd+Delete) puts the file in the user's own Trash and sends the
        // extension a delete, instead of a domain trash the extension would have to keep. macOS 15+
        // (Domain.h, `supportsSyncingTrash`); before it the setter does not exist, the items do not
        // offer trashing, and Finder offers Delete Immediately only.
        let setter = objc2::sel!(setSupportsSyncingTrash:);
        // SAFETY: `respondsToSelector:` takes a selector and returns BOOL; the setter takes a BOOL and
        // is only sent when the object answers to it.
        unsafe {
            let responds: bool = msg_send![&*domain, respondsToSelector: setter];
            if responds {
                let _: () = msg_send![&*domain, setSupportsSyncingTrash: false];
            }
        }
        Ok(domain)
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

    extern "C" {
        /// `NSFileProviderWorkingSetContainerItemIdentifier` (NSFileProviderItem.h:51). Read from the
        /// framework rather than spelled out, so a changed string value cannot silently stop the signal.
        static NSFileProviderWorkingSetContainerItemIdentifier: *const AnyObject;
        /// `NSFileProviderErrorDomain` (NSFileProviderError.h).
        static NSFileProviderErrorDomain: *const AnyObject;
    }

    /// Reports the outcome of the signal chain once, from whichever handler finishes it.
    fn finish(
        tx: &std::sync::Mutex<Option<mpsc::Sender<Result<(), String>>>>,
        result: Result<(), String>,
    ) {
        if let Ok(mut guard) = tx.lock() {
            if let Some(tx) = guard.take() {
                let _ = tx.send(result);
            }
        }
    }

    /// `getDomains`, then `+managerForDomain:`, then
    /// `-signalEnumeratorForContainerItemIdentifier:completionHandler:` on the working set (the only
    /// container a replicated extension signals). Selectors checked against the SDK's
    /// `NSFileProviderManager.h` lines 67, 100 and 242. The manager is used inside the `getDomains`
    /// handler, so no Objective-C object crosses a thread; only the outcome does.
    fn signal_working_set_blocking() -> Result<(), String> {
        on_domain_manager(|manager, done| {
            // SAFETY: an instance method taking an NSString identifier and a `void (^)(NSError *)`
            // block (Manager.h:100). The identifier is a framework constant, valid for the process
            // lifetime; the block is copied by the framework until it runs.
            unsafe {
                let working_set: *const AnyObject = NSFileProviderWorkingSetContainerItemIdentifier;
                let _: () = msg_send![
                    manager,
                    signalEnumeratorForContainerItemIdentifier: working_set,
                    completionHandler: done
                ];
            }
        })
    }

    /// `-signalErrorResolved:completionHandler:` (Manager.h:258) for the two errors a request can answer
    /// with that the system throttles until signalled.
    pub(super) fn signal_errors_resolved() -> Result<(), String> {
        let error_class = AnyClass::get(c"NSError").ok_or("NSError is not available")?;
        for code in [SERVER_UNREACHABLE, NOT_AUTHENTICATED] {
            autoreleasepool(|_| {
                on_domain_manager(move |manager, done| {
                    // SAFETY: `errorWithDomain:code:userInfo:` is documented NSError API and nil is a
                    // valid `userInfo`. The domain is a framework constant, valid for the process
                    // lifetime, and the error is autoreleased into the pool around this call.
                    // `signalErrorResolved:completionHandler:` takes an NSError and a block the
                    // framework copies until it runs.
                    unsafe {
                        let domain: *const AnyObject = NSFileProviderErrorDomain;
                        let no_info: *const AnyObject = std::ptr::null();
                        let error: *mut AnyObject = msg_send![
                            error_class,
                            errorWithDomain: domain,
                            code: code,
                            userInfo: no_info
                        ];
                        let _: () =
                            msg_send![manager, signalErrorResolved: error, completionHandler: done];
                    }
                })
            })?;
        }
        Ok(())
    }

    /// `NSFileProviderError.h`.
    const NOT_AUTHENTICATED: isize = -1000;
    const SERVER_UNREACHABLE: isize = -1004;

    /// Resolves our domain's manager and runs `call` on it, then waits for the block `call` passed on.
    /// `getDomains`, then `+managerForDomain:`; the manager is used inside the `getDomains` handler, so
    /// no Objective-C object crosses a thread, only the outcome does. Selectors checked against the
    /// SDK's `NSFileProviderManager.h` lines 67 and 244.
    fn on_domain_manager(
        call: impl Fn(*mut AnyObject, &block2::Block<dyn Fn(*mut AnyObject)>) + 'static,
    ) -> Result<(), String> {
        let cls = manager_class()?;
        let (tx, rx) = mpsc::channel::<Result<(), String>>();
        let tx = std::sync::Arc::new(std::sync::Mutex::new(Some(tx)));
        let outer_tx = tx.clone();
        let block = RcBlock::new(move |domains: *mut AnyObject, err: *mut AnyObject| {
            autoreleasepool(|_| {
                if !err.is_null() {
                    return finish(&outer_tx, Err(describe_error(err)));
                }
                if domains.is_null() {
                    return finish(&outer_tx, Err("getDomains returned no list".to_string()));
                }
                let mut target: *mut AnyObject = std::ptr::null_mut();
                // SAFETY: `domains` is a live NSArray of NSFileProviderDomain for the duration of this
                // block; `count`, `objectAtIndex:` and `identifier` are documented API.
                unsafe {
                    let count: usize = msg_send![domains, count];
                    for i in 0..count {
                        let d: *mut AnyObject = msg_send![domains, objectAtIndex: i];
                        let id: *mut AnyObject = msg_send![d, identifier];
                        if to_rust_string(id).as_deref() == Some(DOMAIN_ID) {
                            target = d;
                            break;
                        }
                    }
                }
                if target.is_null() {
                    return finish(&outer_tx, Err("the domain is not registered".to_string()));
                }
                // SAFETY: `+managerForDomain:` takes an NSFileProviderDomain and returns an autoreleased
                // manager (nil only for a domain it cannot serve), live until this pool drains.
                let manager: *mut AnyObject = unsafe { msg_send![cls, managerForDomain: target] };
                if manager.is_null() {
                    return finish(&outer_tx, Err("managerForDomain returned nil".to_string()));
                }
                let inner_tx = outer_tx.clone();
                let done = RcBlock::new(move |err: *mut AnyObject| {
                    let result = if err.is_null() {
                        Ok(())
                    } else {
                        Err(describe_error(err))
                    };
                    finish(&inner_tx, result);
                });
                call(manager, &done);
            });
        });
        // SAFETY: class method with one block argument `void (^)(NSArray *, NSError *)` (Manager.h:244).
        unsafe {
            let _: () = msg_send![cls, getDomainsWithCompletionHandler: &*block];
        }
        rx.recv_timeout(CALL_TIMEOUT)
            .map_err(|_| "timed out waiting for the File Provider manager".to_string())?
    }

    /// The worker's queue: one unit per wake-up, created with the first request.
    static SIGNAL_WORKER: std::sync::OnceLock<Option<mpsc::Sender<()>>> =
        std::sync::OnceLock::new();
    static SIGNALS_SENT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// Holds the next signal for the removal grace.
    pub(super) fn hold_signal() {
        super::SIGNAL_HOLD.extend(Instant::now());
    }

    /// Hands a unit to the signal worker and returns. Never blocks and never calls the framework on
    /// the caller's thread.
    pub(super) fn request_signal() {
        if !super::SIGNAL_GATE.request() {
            return;
        }
        let worker = SIGNAL_WORKER.get_or_init(|| {
            let (tx, rx) = mpsc::channel::<()>();
            let spawned = std::thread::Builder::new()
                .name("file-provider-signal".into())
                .spawn(move || {
                    for () in rx {
                        // Opens the gate before signalling: a request that lands while the call is
                        // in flight must wake this loop again.
                        super::wait_out_hold(
                            &super::SIGNAL_GATE,
                            &super::SIGNAL_HOLD,
                            Instant::now,
                            std::thread::sleep,
                        );
                        let outcome = autoreleasepool(|_| signal_working_set_blocking());
                        if outcome.is_ok() {
                            SIGNALS_SENT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        // One line per transition: a missing domain would otherwise warn on every delta.
                        match (super::SIGNAL_OUTCOMES.note(&outcome), outcome) {
                            (super::SignalReport::Failing, Err(e)) => tracing::warn!(
                                error = %e,
                                "File Provider: signalling the working set failed (logged once until it changes)"
                            ),
                            (super::SignalReport::Recovered, _) => tracing::info!(
                                signals_sent = SIGNALS_SENT.load(std::sync::atomic::Ordering::Relaxed),
                                "File Provider: working set signalled again"
                            ),
                            _ => {}
                        }
                    }
                });
            match spawned {
                Ok(_) => Some(tx),
                Err(e) => {
                    tracing::error!(error = %e, "File Provider: could not start the signal worker");
                    None
                }
            }
        });
        let delivered = worker.as_ref().is_some_and(|tx| tx.send(()).is_ok());
        if !delivered {
            super::SIGNAL_GATE.begin();
        }
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

    thread_local! {
        /// How many signals the code under test asked for on this thread. Thread-local so parallel
        /// tests cannot see each other's requests.
        static REQUESTED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        static HOLDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        static ERRORS_RESOLVED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    /// Records the request instead of reaching the framework.
    pub(super) fn hold_signal() {
        HOLDS.with(|h| h.set(h.get() + 1));
    }

    pub(super) fn holds() -> usize {
        HOLDS.with(std::cell::Cell::get)
    }

    pub(super) fn request_signal() {
        REQUESTED.with(|c| c.set(c.get() + 1));
    }

    pub(super) fn requested() -> usize {
        REQUESTED.with(std::cell::Cell::get)
    }

    pub(super) fn signal_errors_resolved() -> Result<(), String> {
        Ok(())
    }

    pub(super) fn note_errors_resolved() {
        ERRORS_RESOLVED.with(|c| c.set(c.get() + 1));
    }

    pub(super) fn errors_resolved() -> usize {
        ERRORS_RESOLVED.with(std::cell::Cell::get)
    }

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

    // ── DBSYNC-95 slice 2a: the switch and the projection, the signal ───────────────────────────

    fn real_db() -> (tempfile::TempDir, Db) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = Db::new_at(&tmp.path().join("app.db")).expect("db");
        (tmp, db)
    }

    /// A burst of requests wakes the worker once; a request after the worker has begun wakes it again,
    /// so a change committed while a signal is in flight is never swallowed.
    ///
    /// Proof this test can fail: making `request` always return `true` fails the burst assertion;
    /// never clearing the flag in `begin` fails the last one.
    #[test]
    fn a_burst_of_signal_requests_wakes_the_worker_once() {
        let gate = SignalGate::new();
        assert!(gate.request(), "the first request wakes the worker");
        for _ in 0..100 {
            assert!(!gate.request(), "the rest are coalesced");
        }
        gate.begin();
        assert!(
            gate.request(),
            "a request after the worker began wakes it again"
        );
    }

    /// The signal outcome is logged on a transition only: the same failure repeated is quiet, a new
    /// reason or a recovery is reported once.
    ///
    /// Proof this test can fail: returning `Failing` for a repeated identical error (dropping the
    /// `last == e` arm) fails the second assertion; never clearing `failing` on success fails the
    /// recovery and the next-failure assertions.
    #[test]
    fn a_signal_failure_is_logged_once_per_transition() {
        let log = SignalOutcomeLog::new();
        let no_domain = Err("the domain is not registered".to_string());
        assert_eq!(log.note(&Ok(())), SignalReport::Quiet);
        assert_eq!(log.note(&no_domain), SignalReport::Failing, "first failure");
        for _ in 0..50 {
            assert_eq!(log.note(&no_domain), SignalReport::Quiet, "the repeat");
        }
        assert_eq!(
            log.note(&Err("timed out".to_string())),
            SignalReport::Failing,
            "a different reason is news"
        );
        assert_eq!(log.note(&Ok(())), SignalReport::Recovered);
        assert_eq!(log.note(&Ok(())), SignalReport::Quiet);
        assert_eq!(
            log.note(&no_domain),
            SignalReport::Failing,
            "failing again after a recovery is a new transition"
        );
    }

    /// Switch off with a populated projection clears it; with nothing in it, writes nothing at all.
    ///
    /// Proof this test can fail: skipping `project_clear` in the off branch of `apply_switch` leaves
    /// the rows; making `clear_with` unconditional moves `seq` on an empty projection.
    #[test]
    fn switching_off_clears_a_populated_projection_and_writes_nothing_to_an_empty_one() {
        let (_tmp, db) = real_db();
        db.set_projection_enabled(true);
        db.project_snapshot(&[]).expect("populate");
        assert!(db.projection_state().unwrap().populated);

        assert_eq!(apply_switch(&db, false), Ok(SwitchEffect::Cleared));
        assert!(!db.projection_state().unwrap().populated);

        let after_clear = db.projection_state().unwrap();
        assert_eq!(apply_switch(&db, false), Ok(SwitchEffect::Untouched));
        assert_eq!(db.projection_state().unwrap(), after_clear, "no write");
    }

    /// Switch on with an unpopulated projection writes nothing, and above all keeps the stored cursor:
    /// wiping it would defeat the failing-seed fallback on a boot with no network.
    ///
    /// Proof this test can fail: restoring the `set_app_config(.., "")` wipe empties the cursor.
    #[test]
    fn switching_on_with_an_unpopulated_projection_keeps_the_cursor() {
        use crate::remote_index::REMOTE_DELTA_CURSOR_KEY as CURSOR;
        let (_tmp, db) = real_db();
        db.set_app_config(CURSOR, "old-cursor").unwrap();
        let before = db.projection_state().unwrap();

        assert_eq!(apply_switch(&db, true), Ok(SwitchEffect::SeedPending));
        assert_eq!(
            db.get_app_config(CURSOR).unwrap().as_deref(),
            Some("old-cursor")
        );
        assert_eq!(db.projection_state().unwrap(), before, "no write");

        db.set_projection_enabled(true);
        db.project_snapshot(&[]).expect("populate");
        db.set_app_config(CURSOR, "live-cursor").unwrap();
        assert_eq!(apply_switch(&db, true), Ok(SwitchEffect::Untouched));
        assert_eq!(
            db.get_app_config(CURSOR).unwrap().as_deref(),
            Some("live-cursor")
        );
    }

    /// Proof this test can fail: removing the `first + SIGNAL_REMOVAL_MAX_HOLD` bound fails the last
    /// assertion, and returning zero from `remaining` fails the first.
    #[test]
    fn a_removal_holds_the_signal_and_a_run_of_them_cannot_hold_it_for_ever() {
        let hold = SignalHold::new();
        let start = Instant::now();
        assert_eq!(hold.remaining(start), Duration::ZERO);

        hold.extend(start);
        assert_eq!(hold.remaining(start), SIGNAL_REMOVAL_GRACE);

        let later = start + Duration::from_secs(5);
        hold.extend(later);
        assert_eq!(hold.remaining(later), SIGNAL_REMOVAL_GRACE);

        let mut now = later;
        while now < start + SIGNAL_REMOVAL_MAX_HOLD * 2 {
            hold.extend(now);
            now += Duration::from_secs(5);
        }
        assert_eq!(
            hold.remaining(start + SIGNAL_REMOVAL_MAX_HOLD),
            Duration::ZERO
        );
    }

    /// Proof this test can fail: moving the `remaining` that ends the hold off this path keeps a stale
    /// window, so the second run starts late and the last assertion fails.
    #[test]
    fn a_hold_that_ran_out_does_not_shorten_the_next_one() {
        let hold = SignalHold::new();
        let start = Instant::now();
        hold.extend(start);
        let expired = start + SIGNAL_REMOVAL_MAX_HOLD;
        assert_eq!(hold.remaining(expired), Duration::ZERO);

        hold.extend(expired);
        assert_eq!(hold.remaining(expired), SIGNAL_REMOVAL_GRACE);
    }

    /// The worker sleeps out the whole hold, including one that grows during the wait, and a request
    /// made during the wait is not swallowed by a gate left closed.
    ///
    /// Proof this test can fail: dropping the loop in `wait_out_hold` signals after the first sleep and
    /// fails the sleep count; moving `gate.begin()` after the loop fails the gate check.
    #[test]
    fn the_worker_waits_out_a_hold_that_grows_while_it_sleeps() {
        let gate = SignalGate::new();
        let hold = SignalHold::new();
        let start = Instant::now();
        let clock = std::cell::Cell::new(start);
        assert!(gate.request());
        hold.extend(start);

        let mut sleeps = 0;
        wait_out_hold(
            &gate,
            &hold,
            || clock.get(),
            |wait| {
                sleeps += 1;
                clock.set(clock.get() + wait);
                if sleeps == 1 {
                    assert!(gate.request(), "the gate is open during the wait");
                    hold.extend(clock.get());
                }
            },
        );

        assert_eq!(sleeps, 2);
        assert_eq!(clock.get(), start + SIGNAL_REMOVAL_GRACE * 2);
    }
}
