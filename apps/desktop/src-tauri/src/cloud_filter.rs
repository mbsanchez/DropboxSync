//! DBSYNC-41: expose per-file sync status in Explorer's built-in cloud "Statut"
//! column via the Cloud Files API (CfAPI), keeping the `.cloudsc` sidecar model +
//! DBSYNC-33 hydrate/dehydrate unchanged.
//!
//! - Register the sync root via **WinRT** `StorageProviderSyncRootManager.Register`
//!   (needs package identity from the sparse package, DBSYNC-58; runs on a dedicated
//!   **MTA** thread — the async `GetFolderFromPathAsync().get()` would deadlock on
//!   the app's STA main thread). Registration is done **once** and reused across
//!   launches (see `registered_folder`).
//! - The WinRT API always also creates a navigation-pane node (`NamespaceCLSID` +
//!   an HKCU shell-folder CLSID). That node makes the shell treat the folder as a
//!   namespace-extension root and *suppresses* the per-file status column on the
//!   physical path. The elevated `native/windows/sparse-package/enable-status-column.ps1`
//!   step (dev: elevated shell; prod: installer) strips that node, which is what
//!   actually surfaces the column. We therefore never re-register (that would
//!   re-create the node and break the column again).
//! - Convert each REAL (hydrated) file to a pinned in-sync placeholder in place
//!   (`CreateFileW` + `CfConvertToPlaceholder`, no download) and drive its state
//!   from `overlay_state.json` via `CfSetInSyncState` (Win32; no COM, no live
//!   `CfConnectSyncRoot` connection needed — the column reads persisted state).
//! - `.cloudsc` sidecars are left as plain files → blank status (by design).
//!
//! Every call fails soft: no package identity, non-NTFS, or a locked file → the
//! step no-ops and the app is otherwise unaffected.
#![cfg(windows)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use windows::core::{GUID, HSTRING, PCWSTR, PWSTR};
use windows::Security::Cryptography::CryptographicBuffer;
use windows::Storage::Provider::{
    StorageProviderHydrationPolicy, StorageProviderPopulationPolicy, StorageProviderSyncRootInfo,
    StorageProviderSyncRootManager,
};
use windows::Storage::StorageFolder;
use windows::Win32::Foundation::{CloseHandle, E_ABORT, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
use windows::Win32::Storage::CloudFilters::{
    CfConvertToPlaceholder, CfRevertPlaceholder, CfSetInSyncState, CfSetPinState,
    CF_CONVERT_FLAG_MARK_IN_SYNC, CF_IN_SYNC_STATE_IN_SYNC, CF_IN_SYNC_STATE_NOT_IN_SYNC,
    CF_PIN_STATE_PINNED, CF_REVERT_FLAG_NONE, CF_SET_IN_SYNC_FLAG_NONE, CF_SET_PIN_FLAG_NONE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// Stable provider id. NEVER change once shipped.
const PROVIDER_ID: GUID = GUID::from_u128(0xEA067F54_0C87_4DC2_9F90_3A632C2AAF9C);

/// The sync-root folder currently registered (register once per folder).
static REGISTERED: OnceLock<Mutex<Option<String>>> = OnceLock::new();
/// `rel → last-applied in-sync bool`, so we only touch a file when it changes.
static APPLIED: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Update Explorer's cloud "Statut" column from the current per-file states.
/// `items` is `(rel, is_synced)` for every tracked local file. Registers the sync
/// root on first use / folder change. Called from `overlay_state`, so the column
/// shares the single `overlay_state.json` source of truth.
pub(crate) fn sync_placeholder_states(sync_folder: Option<&str>, items: &[(&str, bool)]) {
    let Some(folder) = sync_folder else {
        return;
    };
    // The WinRT sync-root registration (and hence the column) needs package identity.
    if !crate::windows_identity::has_package_identity() {
        return;
    }
    if !ensure_registered(folder) {
        return; // registration failed (e.g. non-NTFS) → feature disabled
    }
    let root = Path::new(folder);
    let applied = APPLIED.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut applied) = applied.lock() else {
        return;
    };
    for (rel, is_synced) in items {
        if rel.ends_with(".cloudsc") {
            continue; // cloud-only placeholder → blank in the column, by design
        }
        if matches!(applied.get(*rel), Some(prev) if prev == is_synced) {
            continue; // unchanged since last applied
        }
        // First touch this session → re-associate the file with the current sync
        // root (revert any orphaned placeholder from a previous registration, then
        // convert fresh) so the shell resolves its per-file status instead of blank.
        let first = !applied.contains_key(*rel);
        let abs = root.join(rel);
        if !abs.is_file() {
            continue;
        }
        if apply_file_state(&abs, *is_synced, first) {
            applied.insert((*rel).to_string(), *is_synced);
        }
    }
}

/// Register the sync root once per folder. The WinRT call runs on a fresh MTA
/// thread. Returns whether a root is currently registered for `folder`.
fn ensure_registered(folder: &str) -> bool {
    let reg = REGISTERED.get_or_init(|| Mutex::new(None));
    let Ok(mut cur) = reg.lock() else {
        return false;
    };
    if cur.as_deref() == Some(folder) {
        return true;
    }
    if cur.is_some() {
        let _ = run_on_mta(unregister);
        if let Some(applied) = APPLIED.get() {
            if let Ok(mut applied) = applied.lock() {
                applied.clear();
            }
        }
        *cur = None;
    }
    // Already registered in a previous run? Do NOT re-register: the WinRT
    // `Register` re-creates the navigation-pane node that the elevated
    // `enable-status-column` step strips to make the column show. Re-adding it
    // every launch would break the column again. The persisted registration +
    // the folder's sync-root reparse point are enough for placeholder ops.
    if registered_folder().is_some_and(|f| same_folder(&f, folder)) {
        tracing::info!(folder, "CfAPI sync root already registered (reusing)");
        *cur = Some(folder.to_string());
        return true;
    }
    let folder_owned = folder.to_string();
    match run_on_mta(move || register_winrt(&folder_owned)) {
        Ok(()) => {
            tracing::info!(
                folder,
                "registered CfAPI sync root — run `enable-status-column` (elevated) to show the column"
            );
            *cur = Some(folder.to_string());
            true
        }
        Err(e) => {
            tracing::warn!(folder, error = %e, "WinRT sync-root registration failed; status column disabled");
            false
        }
    }
}

/// Case-insensitive, separator-normalized path equality. Windows paths are
/// case-insensitive and WinRT persists a canonicalized form (from `StorageFolder`)
/// that can differ from our configured folder in casing or trailing separator — an
/// exact compare would falsely re-register every launch, re-creating the nav-pane
/// node that `enable-status-column` strips and breaking the column again.
fn same_folder(a: &str, b: &str) -> bool {
    fn norm(s: &str) -> String {
        s.replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase()
    }
    norm(a) == norm(b)
}

/// The sync-root folder this provider is already registered for (persisted in the
/// shell's `SyncRootManager`), or `None`. Read-only HKLM access — no elevation.
fn registered_folder() -> Option<String> {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;
    let sid = current_user_sid()?;
    let path = format!(
        "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Explorer\\SyncRootManager\\DropboxSync!{sid}!default\\UserSyncRoots"
    );
    let key = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(path).ok()?;
    key.get_value::<String, _>(&sid).ok()
}

/// Run a WinRT closure on a dedicated MTA thread (async `.get()` must not run on
/// the app's STA main thread). Bounded by a timeout so a pathological folder (e.g. a
/// disconnected network/removable path where `GetFolderFromPathAsync().get()` blocks)
/// can't hang the caller — on expiry it fails soft and the column is simply disabled.
fn run_on_mta<F>(f: F) -> windows::core::Result<()>
where
    F: FnOnce() -> windows::core::Result<()> + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // SAFETY: a fresh thread; MTA is fine for these APIs. Ignore the result
        // (S_FALSE = already initialised is not an error here).
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let _ = tx.send(f());
    });
    match rx.recv_timeout(std::time::Duration::from_secs(20)) {
        Ok(result) => result,
        Err(_) => {
            tracing::warn!("cfapi WinRT call timed out (20s); status column disabled this run");
            Err(windows::core::Error::from(E_ABORT))
        }
    }
}

fn sync_root_id() -> String {
    match current_user_sid() {
        Some(sid) => format!("DropboxSync!{sid}!default"),
        None => "DropboxSync!default".to_string(),
    }
}

macro_rules! step {
    ($name:literal, $e:expr) => {{
        $e.map_err(|err: windows::core::Error| {
            tracing::warn!(step = $name, error = %err, "cfapi register step failed");
            err
        })?
    }};
}

fn register_winrt(folder: &str) -> windows::core::Result<()> {
    let id = sync_root_id();
    tracing::info!(sync_root_id = %id, folder, "cfapi register: begin");
    let info = step!("new", StorageProviderSyncRootInfo::new());
    step!("SetId", info.SetId(&HSTRING::from(&id)));
    let op = step!("GetFolderFromPathAsync", StorageFolder::GetFolderFromPathAsync(&HSTRING::from(folder)));
    let hfolder = step!("GetFolder.get", op.get());
    step!("SetPath", info.SetPath(&hfolder));
    step!("SetDisplayNameResource", info.SetDisplayNameResource(&HSTRING::from("DropboxSync")));
    if let Some(icon) = icon_spec() {
        step!("SetIconResource", info.SetIconResource(&HSTRING::from(icon)));
    }
    step!("SetHydrationPolicy", info.SetHydrationPolicy(StorageProviderHydrationPolicy::Full));
    step!("SetPopulationPolicy", info.SetPopulationPolicy(StorageProviderPopulationPolicy::AlwaysFull));
    step!("SetProviderId", info.SetProviderId(PROVIDER_ID));
    step!("SetVersion", info.SetVersion(&HSTRING::from("1.0.0.0")));
    // Context is a required provider-defined blob; a small non-empty buffer suffices.
    let ctx = step!("CreateContext", CryptographicBuffer::CreateFromByteArray(b"DropboxSync"));
    step!("SetContext", info.SetContext(&ctx));
    // Clear any stale/partial registration for this id first, so a leftover entry
    // from a crash or a previous build can't make Register fail (0x80070490).
    let _ = StorageProviderSyncRootManager::Unregister(&HSTRING::from(&id));
    step!("Register", StorageProviderSyncRootManager::Register(&info));
    tracing::info!("cfapi register: ok");
    Ok(())
}

fn unregister() -> windows::core::Result<()> {
    StorageProviderSyncRootManager::Unregister(&HSTRING::from(sync_root_id()))
}

/// `"<exe>,0"` — the app's own icon for the sync-root branding.
fn icon_spec() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(format!("{},0", exe.to_string_lossy()))
}

/// Current user's SID as a string (for the sync-root id), or None on failure.
fn current_user_sid() -> Option<String> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).ok()?;
        let mut len = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
        let mut buf = vec![0u8; len as usize];
        let ok = GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr() as *mut _),
            len,
            &mut len,
        );
        let _ = CloseHandle(token);
        ok.ok()?;
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut s = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut s).ok()?;
        let sid = s.to_string().ok();
        let _ = LocalFree(Some(HLOCAL(s.0 as *mut _)));
        sid
    }
}

/// Convert `abs` in place to a pinned in-sync placeholder and set its state.
/// Returns true if the in-sync state was applied. Fails soft.
///
/// `reconvert` (first touch this session): revert first so a placeholder left by a
/// previous/other sync-root registration is stripped and re-created under the
/// current root — otherwise the shell renders it blank (orphaned). No-ops on plain
/// files; our hydrated files revert without a download.
fn apply_file_state(abs: &Path, is_synced: bool, reconvert: bool) -> bool {
    let path = to_wide(&abs.to_string_lossy());
    unsafe {
        // Convert a normal file to a placeholder → open with CreateFileW (write
        // attributes + backup semantics), NOT CfOpenFileWithOplock.
        let handle = match CreateFileW(
            PCWSTR(path.as_ptr()),
            FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                tracing::debug!(path = %abs.display(), error = %e, "CreateFileW failed");
                return false;
            }
        };
        let revert = if reconvert {
            // Best-effort: strips a stale/orphaned placeholder identity. Errors
            // (e.g. plain file, or already ours) are harmless — we convert next.
            format!("{:?}", CfRevertPlaceholder(handle, CF_REVERT_FLAG_NONE, None))
        } else {
            "skip".into()
        };
        // Convert if not already a placeholder (ignore "already a placeholder").
        // Keeps the file's bytes — no download.
        let conv = CfConvertToPlaceholder(handle, None, 0, CF_CONVERT_FLAG_MARK_IN_SYNC, None, None);
        // Pin so the platform never dehydrates it — our `.cloudsc` menu owns freeing
        // space, and we keep NO live provider connection, so a dehydrated placeholder
        // here would be UNRECOVERABLE (no fetch handler to re-download it). If pinning
        // fails we must NOT leave an unpinned in-sync placeholder that Storage Sense
        // could later dehydrate: revert it to a plain file (safe while hydrated) and
        // report not-applied so it is retried next refresh.
        let pin = CfSetPinState(handle, CF_PIN_STATE_PINNED, CF_SET_PIN_FLAG_NONE, None);
        if pin.is_err() {
            tracing::warn!(file = %abs.display(), error = ?pin, "CfSetPinState failed; reverting to a plain file to keep bytes safe");
            let _ = CfRevertPlaceholder(handle, CF_REVERT_FLAG_NONE, None);
            let _ = CloseHandle(handle);
            return false;
        }
        let state = if is_synced {
            CF_IN_SYNC_STATE_IN_SYNC
        } else {
            CF_IN_SYNC_STATE_NOT_IN_SYNC
        };
        let set = CfSetInSyncState(handle, state, CF_SET_IN_SYNC_FLAG_NONE, None);
        tracing::debug!(
            file = %abs.display(), is_synced, revert = %revert,
            convert = ?conv, set_in_sync = ?set, "cfapi apply_file_state"
        );
        let applied = set.is_ok();
        let _ = CloseHandle(handle);
        applied
    }
}
