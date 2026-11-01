//! The App Group the host app shares with its Finder Sync extension (DBSYNC-97).
//!
//! On macOS the whole data directory (`app.db`, logs, `overlay_state.json`) lives inside the group
//! container, because that is the one place a sandboxed extension can read without a temporary
//! exception. The group id is `<TEAM_ID>.com.dropboxsyncdesktop` (Team ID: see
//! `FinderSyncExtension/README.md`, App Group ADR). It is baked in at compile time from
//! `APPLE_TEAM_ID` (`build.rs` re-runs on a change) and mirrored into the entitlements by
//! `scripts/render-entitlements.mjs`.

/// Fixed part of the id. Must match `GROUP_SUFFIX` in `scripts/render-entitlements.mjs`.
const GROUP_SUFFIX: &str = "com.dropboxsyncdesktop";

/// `<team>.com.dropboxsyncdesktop`, or `None` without a team. Never a bare `.com.dropboxsyncdesktop`.
fn app_group_id(team: Option<&str>) -> Option<String> {
    let team = team?.trim();
    (!team.is_empty()).then(|| format!("{team}.{GROUP_SUFFIX}"))
}

/// Whether `group` is one of the App Groups in the process's own entitlements. Exact match only: a
/// prefix or a different team is not entitled. The only caller passes an id built by `app_group_id`,
/// which is never empty, so no empty-id case needs guarding here.
fn is_entitled(entitled_groups: &[String], group: &str) -> bool {
    entitled_groups.iter().any(|g| g == group)
}

/// This build's group id, but only when the running process is actually entitled to it.
///
/// Foundation returns a container URL on macOS even for a group the process is not entitled to, and
/// asking for it creates and touches that directory (and can raise the "access data from other apps"
/// prompt). So entitlement is decided first, from the process's own signature, and the container is
/// never queried otherwise.
fn entitled_group_id(team: Option<&str>, entitled_groups: &[String]) -> Option<String> {
    app_group_id(team).filter(|id| is_entitled(entitled_groups, id))
}

/// What the process's own signature says about this build's group, for the startup log.
#[derive(Debug, PartialEq, Eq)]
struct EntitlementReport {
    /// This build's group id, `None` when no team was compiled in.
    expected: Option<String>,
    /// Every `application-groups` id the process holds, verbatim.
    held: Vec<String>,
    /// Whether `expected` is among `held`; the same decision `container_path` acts on.
    found: bool,
}

fn entitlement_report(team: Option<&str>, entitled_groups: &[String]) -> EntitlementReport {
    EntitlementReport {
        expected: app_group_id(team),
        held: entitled_groups.to_vec(),
        found: entitled_group_id(team, entitled_groups).is_some(),
    }
}

/// Logs once at startup which App Groups the process's own signature carries and whether this build's
/// group is among them, so a signed build can be verified from its log (DBSYNC-97, #229) without
/// reading the container. Must run after `logging::init_tracing()`: the log file lives inside the
/// container, so nothing in `container_path` itself could ever reach it. Reads only the process's own
/// code-signature blob; never touches the container or the keychain. The group ids carry the team id,
/// which is fine here: the app log is local, never uploaded.
#[cfg(not(test))]
pub(crate) fn log_startup_entitlements() {
    let report = entitlement_report(
        option_env!("APPLE_TEAM_ID"),
        &own_entitlements::application_groups(),
    );
    tracing::info!(
        expected_group = report.expected.as_deref().unwrap_or("<none: built without APPLE_TEAM_ID>"),
        held_groups = ?report.held,
        expected_group_found = report.found,
        "App Group entitlement check"
    );
}

/// Minimal Security / CoreFoundation FFI: reads the `com.apple.security.application-groups`
/// entitlement of the running process. `SecTaskCreateFromSelf` and `SecTaskCopyValueForEntitlement`
/// only read this process's code-signature blob; they do not touch the keychain.
mod own_entitlements {
    use std::ffi::{c_char, c_void, CString};
    use std::ptr;

    type CFTypeRef = *const c_void;
    type CFIndex = isize;
    type CFTypeID = usize;
    const UTF8: u32 = 0x0800_0100; // kCFStringEncodingUTF8

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRelease(cf: CFTypeRef);
        fn CFGetTypeID(cf: CFTypeRef) -> CFTypeID;
        fn CFArrayGetTypeID() -> CFTypeID;
        fn CFStringGetTypeID() -> CFTypeID;
        fn CFArrayGetCount(array: CFTypeRef) -> CFIndex;
        fn CFArrayGetValueAtIndex(array: CFTypeRef, index: CFIndex) -> CFTypeRef;
        fn CFStringCreateWithCString(
            alloc: CFTypeRef,
            c_str: *const c_char,
            encoding: u32,
        ) -> CFTypeRef;
        fn CFStringGetLength(s: CFTypeRef) -> CFIndex;
        fn CFStringGetMaximumSizeForEncoding(length: CFIndex, encoding: u32) -> CFIndex;
        fn CFStringGetCString(
            s: CFTypeRef,
            buffer: *mut c_char,
            size: CFIndex,
            encoding: u32,
        ) -> u8;
    }

    #[link(name = "Security", kind = "framework")]
    extern "C" {
        fn SecTaskCreateFromSelf(allocator: CFTypeRef) -> CFTypeRef;
        fn SecTaskCopyValueForEntitlement(
            task: CFTypeRef,
            entitlement: CFTypeRef,
            error: *mut CFTypeRef,
        ) -> CFTypeRef;
    }

    /// Releases a Core Foundation object on drop, so no early return leaks it.
    struct Owned(CFTypeRef);
    impl Drop for Owned {
        fn drop(&mut self) {
            // SAFETY: only constructed from non-null objects returned by a Create/Copy function.
            unsafe { CFRelease(self.0) }
        }
    }

    /// The process's own `application-groups` entitlement; empty when unsigned, ad-hoc signed without
    /// entitlements, or when the value is not an array of strings.
    pub(super) fn application_groups() -> Vec<String> {
        // SAFETY: every call passes objects this function owns (released by `Owned`) or borrows from
        // the array it owns, and only reads them before the owner is dropped.
        unsafe {
            let Ok(key) = CString::new("com.apple.security.application-groups") else {
                return Vec::new();
            };
            let key = CFStringCreateWithCString(ptr::null(), key.as_ptr(), UTF8);
            if key.is_null() {
                return Vec::new();
            }
            let key = Owned(key);
            let task = SecTaskCreateFromSelf(ptr::null());
            if task.is_null() {
                return Vec::new();
            }
            let task = Owned(task);
            let mut error: CFTypeRef = ptr::null();
            let value = SecTaskCopyValueForEntitlement(task.0, key.0, &mut error);
            if !error.is_null() {
                drop(Owned(error));
            }
            if value.is_null() {
                return Vec::new();
            }
            let value = Owned(value);
            if CFGetTypeID(value.0) != CFArrayGetTypeID() {
                return Vec::new();
            }
            let mut groups = Vec::new();
            for i in 0..CFArrayGetCount(value.0) {
                let item = CFArrayGetValueAtIndex(value.0, i); // borrowed from `value`
                if item.is_null() || CFGetTypeID(item) != CFStringGetTypeID() {
                    continue;
                }
                let capacity = CFStringGetMaximumSizeForEncoding(CFStringGetLength(item), UTF8) + 1;
                let mut buf = vec![0u8; capacity.max(1) as usize];
                if CFStringGetCString(item, buf.as_mut_ptr().cast(), capacity, UTF8) != 0 {
                    if let Some(end) = buf.iter().position(|&b| b == 0) {
                        groups.push(String::from_utf8_lossy(&buf[..end]).into_owned());
                    }
                }
            }
            groups
        }
    }
}

/// The absolute path of this build's App Group container, or `None` when the build had no team, the
/// running process does not hold the group in its own signed entitlements, or Foundation returned nil.
/// `None` makes `data_dir_for` fail loudly, and the container is never queried, so a process without
/// the entitlement neither creates it nor can raise a TCC prompt.
///
/// Which builds get the group: only those built with `APPLE_TEAM_ID` AND a real `APPLE_SIGNING_IDENTITY`
/// (not unset, empty or `-`); `scripts/render-entitlements.mjs` drops the entitlement otherwise, and a
/// build with the group in its signature is the only one that passes the check above. An ad-hoc
/// `bundle:dev` or `dev:mac` therefore has no group and fails at startup. What a `tauri dev` run (a bare
/// binary, no bundle) does is UNVERIFIED until #229; do not read this as a guarantee for it.
///
/// Asked of `NSFileManager`, never built by hand, so that `containermanagerd` creates the container
/// together with its metadata. A hand-made `~/Library/Group Containers/<id>` is not a container.
///
/// Under `cfg(test)` this is always `None`: no unit test may reach the real container. Tests inject a
/// base into `data_dir_for` instead.
#[cfg(not(test))]
pub(crate) fn container_path() -> Option<String> {
    use objc2::msg_send;
    use objc2::rc::autoreleasepool;
    use objc2::runtime::{AnyClass, AnyObject};
    use std::ffi::{c_char, CStr, CString};

    let id = entitled_group_id(
        option_env!("APPLE_TEAM_ID"),
        &own_entitlements::application_groups(),
    )?;
    let id = CString::new(id).ok()?;
    autoreleasepool(|_| {
        // SAFETY: every selector below is documented Foundation API with the signature used here.
        // `stringWithUTF8String:` takes a NUL-terminated C string that `id` outlives; the returned
        // objects are autoreleased and are only read inside this pool, with the path copied out.
        unsafe {
            let ns_string = AnyClass::get(c"NSString")?;
            let file_manager = AnyClass::get(c"NSFileManager")?;
            let group: *mut AnyObject = msg_send![ns_string, stringWithUTF8String: id.as_ptr()];
            let manager: *mut AnyObject = msg_send![file_manager, defaultManager];
            let url: *mut AnyObject =
                msg_send![manager, containerURLForSecurityApplicationGroupIdentifier: group];
            if url.is_null() {
                return None;
            }
            let path: *mut AnyObject = msg_send![url, path];
            if path.is_null() {
                return None;
            }
            let utf8: *const c_char = msg_send![path, UTF8String];
            if utf8.is_null() {
                return None;
            }
            Some(CStr::from_ptr(utf8).to_string_lossy().into_owned())
        }
    })
}

#[cfg(test)]
pub(crate) fn container_path() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::{
        app_group_id, entitled_group_id, entitlement_report, is_entitled, own_entitlements,
        EntitlementReport,
    };

    #[test]
    fn the_id_is_the_team_prefix_plus_the_fixed_suffix() {
        assert_eq!(
            app_group_id(Some("ABCDE12345")).as_deref(),
            Some("ABCDE12345.com.dropboxsyncdesktop")
        );
    }

    /// A build without a team has no group. It must never yield a bare `.com.dropboxsyncdesktop`,
    /// which `containerURLForSecurityApplicationGroupIdentifier:` would treat as a real, wrong id.
    #[test]
    fn no_team_yields_no_group_id() {
        for team in [None, Some(""), Some("   ")] {
            assert_eq!(app_group_id(team), None, "team {team:?}");
        }
    }

    fn groups(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn entitled_only_on_an_exact_match() {
        let held = groups(&["ABCDE12345.com.dropboxsyncdesktop"]);
        assert!(is_entitled(&held, "ABCDE12345.com.dropboxsyncdesktop"));
        assert!(!is_entitled(&held, "ABCDE12345.com.dropboxsyncdesktop.x"));
        assert!(!is_entitled(&held, "ABCDE12345"));
        assert!(!is_entitled(&held, "ZZZZZ99999.com.dropboxsyncdesktop"));
        assert!(!is_entitled(&[], "ABCDE12345.com.dropboxsyncdesktop"));
    }

    /// The unsigned / unentitled build: a team is compiled in, but the process holds no such group, so
    /// there is no group id to ask Foundation about and the container is never touched.
    #[test]
    fn an_unentitled_process_yields_no_group_id() {
        let team = Some("ABCDE12345");
        assert_eq!(entitled_group_id(team, &[]), None);
        assert_eq!(
            entitled_group_id(team, &groups(&["ZZZZZ99999.com.dropboxsyncdesktop"])),
            None
        );
        assert_eq!(
            entitled_group_id(team, &groups(&["ABCDE12345.com.dropboxsyncdesktop"])).as_deref(),
            Some("ABCDE12345.com.dropboxsyncdesktop")
        );
        assert_eq!(
            entitled_group_id(None, &groups(&["ABCDE12345.com.dropboxsyncdesktop"])),
            None
        );
    }

    /// What the startup log line says (#229 verifies the signed build from it): the expected id, every
    /// group the process holds, and whether the expected one is among them.
    #[test]
    fn the_report_names_expected_held_and_found() {
        let held = groups(&["ZZZZZ99999.other", "ABCDE12345.com.dropboxsyncdesktop"]);
        assert_eq!(
            entitlement_report(Some("ABCDE12345"), &held),
            EntitlementReport {
                expected: Some("ABCDE12345.com.dropboxsyncdesktop".into()),
                held: held.clone(),
                found: true,
            }
        );
        let wrong = groups(&["ZZZZZ99999.com.dropboxsyncdesktop"]);
        let r = entitlement_report(Some("ABCDE12345"), &wrong);
        assert!(!r.found, "a different team's group is not the expected one");
        assert_eq!(r.held, wrong);
        let r = entitlement_report(Some("ABCDE12345"), &[]);
        assert!(!r.found);
        assert!(r.held.is_empty());
        let r = entitlement_report(None, &held);
        assert_eq!(r.expected, None);
        assert!(!r.found, "no team compiled in: nothing can be found");
    }

    /// The report must agree with the decision `container_path` makes, or the log would lie.
    #[test]
    fn the_report_agrees_with_the_entitlement_decision() {
        for (team, held) in [
            (Some("ABCDE12345"), groups(&[])),
            (
                Some("ABCDE12345"),
                groups(&["ABCDE12345.com.dropboxsyncdesktop"]),
            ),
            (
                Some("ABCDE12345"),
                groups(&["ABCDE12345.com.dropboxsyncdesktop.x"]),
            ),
            (None, groups(&["ABCDE12345.com.dropboxsyncdesktop"])),
        ] {
            assert_eq!(
                entitlement_report(team, &held).found,
                entitled_group_id(team, &held).is_some(),
                "{team:?} {held:?}"
            );
        }
    }

    /// Exercises the FFI on the test binary, which carries no application-groups entitlement. It reads
    /// only this process's own signature; it never reaches a container or the keychain.
    #[test]
    fn the_test_binary_holds_no_application_group() {
        assert!(own_entitlements::application_groups().is_empty());
    }

    /// The two halves of the id must agree: `scripts/render-entitlements.mjs` writes the entitlement
    /// and this file asks for the container. A drift would sign one group and request another.
    #[test]
    fn group_suffix_matches_the_entitlements_renderer() {
        let js = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("scripts")
            .join("render-entitlements.mjs");
        let src = std::fs::read_to_string(&js).expect("render-entitlements.mjs is readable");
        let js_suffix = src
            .lines()
            .find_map(|l| {
                l.strip_prefix("export const GROUP_SUFFIX = \"")
                    .and_then(|r| r.strip_suffix("\";"))
            })
            .expect("GROUP_SUFFIX declaration found in render-entitlements.mjs");
        assert_eq!(super::GROUP_SUFFIX, js_suffix);
    }
}
