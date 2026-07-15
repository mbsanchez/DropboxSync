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

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use windows::core::{GUID, HSTRING, PCWSTR, PWSTR};
use windows::Security::Cryptography::CryptographicBuffer;
use windows::Storage::Provider::{
    StorageProviderHydrationPolicy, StorageProviderHydrationPolicyModifier,
    StorageProviderPopulationPolicy, StorageProviderSyncRootInfo, StorageProviderSyncRootManager,
};
use windows::Storage::StorageFolder;
use windows::Win32::Foundation::{CloseHandle, E_ABORT, HANDLE, HLOCAL, LocalFree, NTSTATUS};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
use windows::Win32::Storage::CloudFilters::{
    CfConnectSyncRoot, CfConvertToPlaceholder, CfCreatePlaceholders, CfExecute,
    CfHydratePlaceholder, CfRevertPlaceholder, CfSetInSyncState, CfSetPinState, CF_CALLBACK_INFO,
    CF_CALLBACK_PARAMETERS, CF_CALLBACK_REGISTRATION, CF_CALLBACK_TYPE,
    CF_CALLBACK_TYPE_CANCEL_FETCH_DATA, CF_CALLBACK_TYPE_FETCH_DATA, CF_CONNECTION_KEY,
    CF_CONNECT_FLAGS, CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH, CF_CONNECT_FLAG_REQUIRE_PROCESS_INFO,
    CF_CONVERT_FLAG_MARK_IN_SYNC, CF_CREATE_FLAG_NONE, CF_FS_METADATA, CF_HYDRATE_FLAG_NONE,
    CF_IN_SYNC_STATE_IN_SYNC, CF_IN_SYNC_STATE_NOT_IN_SYNC, CF_OPERATION_INFO,
    CF_OPERATION_PARAMETERS, CF_OPERATION_PARAMETERS_0, CF_OPERATION_PARAMETERS_0_0,
    CF_OPERATION_TRANSFER_DATA_FLAG_NONE, CF_OPERATION_TYPE_TRANSFER_DATA,
    CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC, CF_PLACEHOLDER_CREATE_INFO, CF_PIN_STATE_PINNED,
    CF_REVERT_FLAG_NONE, CF_SET_IN_SYNC_FLAG_NONE, CF_SET_PIN_FLAG_NONE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_ARCHIVE, FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::error::{AppError, AppResult};

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
        // A native CfAPI dehydrated placeholder (cloud-only, e.g. after "free up
        // space") is a real file. NEVER run apply_file_state on it: pinning it would
        // make the platform hydrate (re-download) it, defeating the freed space, and
        // its cloud-only status is owned by its native dehydrated state, not the
        // column driver (DBSYNC-59 Slice 2).
        if crate::path_util::is_dehydrated_placeholder(&abs) {
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
    // Already registered in a previous run WITH the current policy version? Do NOT
    // re-register: the WinRT `Register` re-creates the navigation-pane node that the
    // elevated `enable-status-column` step strips to make the column show. Re-adding
    // it every launch would break the column again. The persisted registration + the
    // folder's sync-root reparse point are enough for placeholder ops. A bumped
    // `REG_POLICY_VERSION` (a policy change like enabling dehydration) forces one
    // re-register — after which the elevated strip must run again.
    if registered_folder().is_some_and(|f| same_folder(&f, folder))
        && stored_policy_version() >= REG_POLICY_VERSION
    {
        tracing::info!(folder, "CfAPI sync root already registered (reusing)");
        *cur = Some(folder.to_string());
        connect_provider(folder);
        return true;
    }
    let folder_owned = folder.to_string();
    match run_on_mta(move || register_winrt(&folder_owned)) {
        Ok(()) => {
            set_stored_policy_version(REG_POLICY_VERSION);
            tracing::info!(
                folder,
                "registered CfAPI sync root (policy v{REG_POLICY_VERSION}) — run `enable-status-column` (elevated) to show the column"
            );
            *cur = Some(folder.to_string());
            connect_provider(folder);
            true
        }
        Err(e) => {
            tracing::warn!(folder, error = %e, "WinRT sync-root registration failed; status column disabled");
            false
        }
    }
}

/// Live `CfConnectSyncRoot` connection key (`CF_CONNECTION_KEY.0`) for the process
/// lifetime, so the platform can call our `FETCH_DATA` handler to hydrate on open.
/// The connection does NOT persist across process restarts — reconnect each run.
static CONNECTION: OnceLock<Mutex<Option<i64>>> = OnceLock::new();
/// `TransferKey`s the platform asked us to cancel; checked between transfer chunks.
static CANCELLED: OnceLock<Mutex<HashSet<i64>>> = OnceLock::new();

/// Connect the (registered) sync root once per process so on-demand hydration
/// works. Fails soft — a registered-but-disconnected root just can't hydrate
/// dehydrated files on open (the status column is unaffected).
fn connect_provider(folder: &str) {
    let slot = CONNECTION.get_or_init(|| Mutex::new(None));
    let Ok(mut guard) = slot.lock() else {
        return;
    };
    if guard.is_some() {
        return; // already connected this session
    }
    // Terminated callback table: FETCH_DATA (hydrate on open) + CANCEL + END.
    let table = [
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_FETCH_DATA,
            Callback: Some(on_fetch_data),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_CANCEL_FETCH_DATA,
            Callback: Some(on_cancel_fetch_data),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE(-1),
            Callback: None,
        },
    ];
    let path = HSTRING::from(folder);
    let flags = CF_CONNECT_FLAGS(
        CF_CONNECT_FLAG_REQUIRE_PROCESS_INFO.0 | CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH.0,
    );
    match unsafe { CfConnectSyncRoot(PCWSTR(path.as_ptr()), table.as_ptr(), None, flags) } {
        Ok(key) => {
            *guard = Some(key.0);
            tracing::info!(folder, "cfapi provider connected (on-demand hydration)");
        }
        Err(e) => {
            tracing::warn!(folder, error = %e, "CfConnectSyncRoot failed; on-demand hydration disabled");
        }
    }
}

const STATUS_SUCCESS: NTSTATUS = NTSTATUS(0);
const STATUS_UNSUCCESSFUL: NTSTATUS = NTSTATUS(0xC000_0001u32 as i32);
/// Transfer chunk size — a multiple of the sector size (4 KiB) as CfAPI requires
/// for every non-final `TRANSFER_DATA`.
const FETCH_CHUNK: usize = 4 * 1024 * 1024;

fn mark_cancelled(transfer_key: i64) {
    if let Ok(mut s) = CANCELLED.get_or_init(|| Mutex::new(HashSet::new())).lock() {
        s.insert(transfer_key);
    }
}
fn is_cancelled(transfer_key: i64) -> bool {
    CANCELLED
        .get()
        .and_then(|m| m.lock().ok().map(|s| s.contains(&transfer_key)))
        .unwrap_or(false)
}
fn clear_cancel(transfer_key: i64) {
    if let Some(m) = CANCELLED.get() {
        if let Ok(mut s) = m.lock() {
            s.remove(&transfer_key);
        }
    }
}

/// Reach the owned `AppState` from a platform callback thread (no `AppHandle` arg).
fn app_state() -> Option<crate::state::AppState> {
    use tauri::Manager;
    let handle = crate::state::APP_HANDLE.get()?;
    Some(handle.try_state::<crate::state::AppState>()?.inner().clone())
}

/// Reconstruct the placeholder's absolute path from the callback info
/// (`VolumeDosName` = `C:` + `NormalizedPath` = `\Users\…\file`).
unsafe fn full_local_path(info: &CF_CALLBACK_INFO) -> Option<PathBuf> {
    let vol = info.VolumeDosName.to_string().ok()?;
    let norm = info.NormalizedPath.to_string().ok()?;
    if norm.is_empty() {
        return None;
    }
    Some(PathBuf::from(format!("{vol}{norm}")))
}

/// `FETCH_DATA`: the platform needs `[offset, offset+length)` of a dehydrated
/// placeholder. Download the file from Dropbox and stream the range back via
/// `CfExecute`. On any failure, report failure so the open unblocks (errors)
/// rather than hanging. Panic-firewalled — a panic across this ABI aborts.
unsafe extern "system" fn on_fetch_data(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    if info.is_null() || params.is_null() {
        return;
    }
    let info = &*info;
    let params = &*params;
    let conn = info.ConnectionKey;
    let transfer = info.TransferKey;
    let fetch = &params.Anonymous.FetchData;
    let offset = fetch.RequiredFileOffset;
    let length = fetch.RequiredLength;
    let abs = full_local_path(info);

    let served = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match &abs {
        Some(p) => serve_fetch(conn, transfer, p, offset, length),
        None => Err(AppError::Other("could not resolve placeholder path".into())),
    }))
    .unwrap_or_else(|_| Err(AppError::Other("fetch handler panicked".into())));

    clear_cancel(transfer);
    match served {
        Ok(()) => {
            tracing::info!(offset, length, "cfapi fetch-data served");
            // Native (double-click) hydration: settle the status column off its
            // "syncing" glyph once hydration finishes. Deferred to a background thread
            // — never re-enter CfAPI (open + CfSetInSyncState) from inside the
            // FETCH_DATA callback — with a small delay so the platform finalizes the
            // hydration first.
            if let Some(p) = abs.clone() {
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    reconcile_after_hydration(&p);
                });
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "cfapi fetch-data failed; failing the open");
            let _ = cf_transfer(conn, transfer, std::ptr::null(), offset, length, STATUS_UNSUCCESSFUL);
        }
    }
}

/// `CANCEL_FETCH_DATA`: flag the transfer so an in-flight `serve_fetch` stops.
unsafe extern "system" fn on_cancel_fetch_data(
    info: *const CF_CALLBACK_INFO,
    _params: *const CF_CALLBACK_PARAMETERS,
) {
    if info.is_null() {
        return;
    }
    mark_cancelled((*info).TransferKey);
    tracing::debug!("cfapi fetch-data cancelled by platform");
}

/// Download the whole file to a temp, then stream the requested range to the
/// platform. (HydrationPolicy=Full → the platform asks for the whole file on
/// first access, so whole-file download is the normal path; ranged download is a
/// future optimisation.)
fn serve_fetch(
    conn: CF_CONNECTION_KEY,
    transfer: i64,
    abs: &Path,
    offset: i64,
    length: i64,
) -> AppResult<()> {
    let state = app_state().ok_or_else(|| AppError::Other("app state unavailable".into()))?;
    let folder = state
        .db
        .get_sync_folder()?
        .ok_or_else(|| AppError::Other("no sync folder configured".into()))?;
    let rel = crate::path_util::relpath_under(Path::new(&folder), abs)?;
    let remote = crate::path_util::normalize_dropbox_path(&rel)?;

    let tmp = std::env::temp_dir().join(format!(
        "dbsync-hydrate-{}-{}.tmp",
        std::process::id(),
        rand::random::<u32>()
    ));
    let result = (|| -> AppResult<()> {
        crate::dropbox_transfer::download_to_path(&state, &remote, &tmp)?;
        let mut f = std::fs::File::open(&tmp)?;
        stream_range(conn, transfer, &mut f, offset, length)
    })();
    let _ = std::fs::remove_file(&tmp);
    result
}

/// Stream `[offset, offset+length)` from `f` to the platform in aligned chunks.
fn stream_range(
    conn: CF_CONNECTION_KEY,
    transfer: i64,
    f: &mut std::fs::File,
    offset: i64,
    length: i64,
) -> AppResult<()> {
    f.seek(SeekFrom::Start(offset as u64))?;
    let mut buf = vec![0u8; FETCH_CHUNK];
    let mut pos = offset;
    let mut remaining = length;
    while remaining > 0 {
        if is_cancelled(transfer) {
            return Err(AppError::Other("fetch cancelled".into()));
        }
        let want = remaining.min(FETCH_CHUNK as i64) as usize;
        let mut filled = 0;
        while filled < want {
            let n = f.read(&mut buf[filled..want])?;
            if n == 0 {
                break; // EOF
            }
            filled += n;
        }
        if filled == 0 {
            break;
        }
        unsafe {
            cf_transfer(conn, transfer, buf.as_ptr() as *const _, pos, filled as i64, STATUS_SUCCESS)
                .map_err(|e| AppError::Other(format!("CfExecute(TRANSFER_DATA): {e}")))?;
        }
        pos += filled as i64;
        remaining -= filled as i64;
        if filled < want {
            break; // reached EOF (final, possibly unaligned, chunk — allowed)
        }
    }
    Ok(())
}

/// One `CfExecute(CF_OPERATION_TYPE_TRANSFER_DATA)`. `status = STATUS_UNSUCCESSFUL`
/// + null buffer reports a failed fetch for the range.
unsafe fn cf_transfer(
    conn: CF_CONNECTION_KEY,
    transfer: i64,
    buffer: *const core::ffi::c_void,
    offset: i64,
    length: i64,
    status: NTSTATUS,
) -> windows::core::Result<()> {
    let op_info = CF_OPERATION_INFO {
        StructSize: std::mem::size_of::<CF_OPERATION_INFO>() as u32,
        Type: CF_OPERATION_TYPE_TRANSFER_DATA,
        ConnectionKey: conn,
        TransferKey: transfer,
        CorrelationVector: std::ptr::null(),
        SyncStatus: std::ptr::null(),
        RequestKey: 0,
    };
    let mut params = CF_OPERATION_PARAMETERS {
        ParamSize: (core::mem::offset_of!(CF_OPERATION_PARAMETERS, Anonymous)
            + std::mem::size_of::<CF_OPERATION_PARAMETERS_0_0>()) as u32,
        Anonymous: CF_OPERATION_PARAMETERS_0 {
            TransferData: CF_OPERATION_PARAMETERS_0_0 {
                Flags: CF_OPERATION_TRANSFER_DATA_FLAG_NONE,
                CompletionStatus: status,
                Buffer: buffer,
                Offset: offset,
                Length: length,
            },
        },
    };
    CfExecute(&op_info, &mut params)
}

/// True when native CfAPI dehydrated placeholders are usable for `folder` right
/// now: Windows + package identity + the sync root registered AND a live hydration
/// connection. When false, callers fall back to the `.cloudsc` sidecar model
/// (macOS, non-packaged Windows, or a failed registration/connection) so cloud-only
/// content keeps working everywhere. A live connection is required because a
/// dehydrated placeholder with no `FETCH_DATA` handler connected can't be opened.
pub(crate) fn placeholders_active(folder: &str) -> bool {
    crate::windows_identity::has_package_identity() && ensure_registered(folder) && is_connected()
}

/// Whether the process currently holds a live `CfConnectSyncRoot` connection.
fn is_connected() -> bool {
    CONNECTION
        .get()
        .and_then(|m| m.lock().ok().map(|g| g.is_some()))
        .unwrap_or(false)
}

/// Create a real dehydrated (online-only) CfAPI placeholder named `name` inside the
/// real directory `parent`, backed by Dropbox path `remote_path` with remote byte
/// size `size`. It shows the cloud icon and hydrates on open via our `FETCH_DATA`
/// handler (`FileIdentity` carries `remote_path`; the handler also re-derives it
/// from the local path). Marked in-sync so no upload is triggered. The caller MUST
/// ensure `parent\name` does not already exist (`CfCreatePlaceholders` fails
/// otherwise). Returns whether the placeholder was created.
pub(crate) fn create_dehydrated_placeholder(
    parent: &Path,
    name: &str,
    remote_path: &str,
    size: i64,
) -> bool {
    let name_w = to_wide(name);
    let base_w = to_wide(&parent.to_string_lossy());
    let ident: Vec<u16> = remote_path.encode_utf16().collect();
    let mut info = CF_PLACEHOLDER_CREATE_INFO {
        RelativeFileName: PCWSTR(name_w.as_ptr()),
        FsMetadata: CF_FS_METADATA {
            BasicInfo: FILE_BASIC_INFO {
                CreationTime: 0,
                LastAccessTime: 0,
                LastWriteTime: 0,
                ChangeTime: 0,
                FileAttributes: FILE_ATTRIBUTE_ARCHIVE.0,
            },
            FileSize: size,
        },
        FileIdentity: ident.as_ptr() as *const _,
        FileIdentityLength: (ident.len() * 2) as u32,
        Flags: CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC,
        Result: windows::core::HRESULT(0),
        CreateUsn: 0,
    };
    let r = unsafe {
        CfCreatePlaceholders(
            PCWSTR(base_w.as_ptr()),
            std::slice::from_mut(&mut info),
            CF_CREATE_FLAG_NONE,
            None,
        )
    };
    tracing::info!(parent = %parent.display(), name, size, remote = %remote_path, result = ?r, entry = ?info.Result, "cfapi create dehydrated placeholder");
    r.is_ok() && info.Result.is_ok()
}

/// Post-hydration reconcile for the status column. The platform can leave a
/// just-hydrated placeholder marked NOT_IN_SYNC, and our `APPLIED` cache then makes
/// `sync_placeholder_states` skip re-applying it (it already believes the file is in
/// the desired synced state) — so the "Statut" column sticks on a "syncing" glyph
/// forever even though the file is fully hydrated and matches the cloud. Fix: drop
/// the cache entry (so a later overlay refresh re-applies pin/sync per policy) and
/// re-confirm IN_SYNC now so the column settles immediately. Right after hydration
/// the local bytes equal the remote, so IN_SYNC is correct. Fails soft.
fn reconcile_after_hydration(abs: &Path) {
    if let Some(state) = app_state() {
        if let Ok(Some(folder)) = state.db.get_sync_folder() {
            if let Ok(rel) = crate::path_util::relpath_under(Path::new(&folder), abs) {
                if let Some(applied) = APPLIED.get() {
                    if let Ok(mut m) = applied.lock() {
                        m.remove(&rel);
                    }
                }
            }
        }
    }
    let path = to_wide(&abs.to_string_lossy());
    unsafe {
        if let Ok(handle) = CreateFileW(
            PCWSTR(path.as_ptr()),
            FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        ) {
            let r = CfSetInSyncState(handle, CF_IN_SYNC_STATE_IN_SYNC, CF_SET_IN_SYNC_FLAG_NONE, None);
            let _ = CloseHandle(handle);
            tracing::debug!(file = %abs.display(), result = ?r, "cfapi reconcile after hydration (mark in-sync)");
        }
    }
}

/// Force full hydration of a native CfAPI dehydrated placeholder at `abs` (our
/// "Synchroniser sur le disque" menu on a cloud-only file). Opens a backup-semantics
/// handle and calls `CfHydratePlaceholder` for the whole file (offset 0, length -1),
/// which pulls every byte through our `FETCH_DATA` handler. Returns whether it
/// succeeded. Fails soft. Requires a live provider connection (see `is_connected`).
pub(crate) fn hydrate_placeholder(abs: &Path) -> bool {
    let path = to_wide(&abs.to_string_lossy());
    unsafe {
        let handle = match CreateFileW(
            PCWSTR(path.as_ptr()),
            FILE_GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(path = %abs.display(), error = %e, "hydrate_placeholder: open failed");
                return false;
            }
        };
        let r = CfHydratePlaceholder(handle, 0, -1, CF_HYDRATE_FLAG_NONE, None);
        let _ = CloseHandle(handle);
        tracing::info!(file = %abs.display(), result = ?r, "cfapi hydrate placeholder");
        if r.is_ok() {
            // Settle the status column: re-confirm IN_SYNC after hydration (see
            // reconcile_after_hydration). Safe here — we're on the app action thread,
            // and CfHydratePlaceholder returned synchronously.
            reconcile_after_hydration(abs);
        }
        r.is_ok()
    }
}

/// DEV / validation only (DBSYNC-59 Slice 1): turn a synced file into a real
/// dehydrated (online-only) placeholder so opening it triggers our `FETCH_DATA`
/// handler. A provider cannot force-dehydrate an existing file
/// (`CfDehydratePlaceholder` → DEHYDRATION_DISALLOWED), so we create the placeholder
/// fresh: remove the local file then create it dehydrated, so the fs-watcher debounce
/// coalesces it and no delete propagates to Dropbox. Returns whether it was created.
pub(crate) fn dehydrate_for_test(abs: &Path) -> bool {
    let Some(state) = app_state() else {
        return false;
    };
    let Ok(Some(folder)) = state.db.get_sync_folder() else {
        return false;
    };
    let (Ok(rel), Ok(size)) = (
        crate::path_util::relpath_under(Path::new(&folder), abs),
        std::fs::metadata(abs).map(|m| m.len() as i64),
    ) else {
        return false;
    };
    let Ok(remote) = crate::path_util::normalize_dropbox_path(&rel) else {
        return false;
    };
    let (Some(parent), Some(name)) = (abs.parent(), abs.file_name()) else {
        return false;
    };

    if let Err(e) = std::fs::remove_file(abs) {
        tracing::warn!(path = %abs.display(), error = %e, "dehydrate_for_test: remove failed");
        return false;
    }
    create_dehydrated_placeholder(parent, &name.to_string_lossy(), &remote, size)
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

/// Bump when the sync-root registration policy changes (e.g. enabling dehydration)
/// so an existing install re-registers once to apply it. NEVER lower it.
const REG_POLICY_VERSION: u32 = 2;
const POLICY_VERSION_KEY: &str = "Software\\DropboxSyncDesktop\\SyncRoot";

/// The registration policy version this machine last registered with (HKCU, our
/// own key — read-only, no elevation). `0` if never set / older build.
fn stored_policy_version() -> u32 {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(POLICY_VERSION_KEY)
        .ok()
        .and_then(|k| k.get_value::<u32, _>("PolicyVersion").ok())
        .unwrap_or(0)
}

fn set_stored_policy_version(v: u32) {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    if let Ok((k, _)) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(POLICY_VERSION_KEY) {
        let _ = k.set_value("PolicyVersion", &v);
    }
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
    // Let the platform dehydrate unpinned in-sync files (and let us call
    // CfDehydratePlaceholder directly) — required for on-demand / the cloud icon
    // (DBSYNC-59). Only affects UNPINNED files, so DBSYNC-41's pinned files (and the
    // status column) are unchanged.
    step!(
        "SetHydrationPolicyModifier",
        info.SetHydrationPolicyModifier(StorageProviderHydrationPolicyModifier::AutoDehydrationAllowed)
    );
    step!("SetPopulationPolicy", info.SetPopulationPolicy(StorageProviderPopulationPolicy::AlwaysFull));
    step!("SetProviderId", info.SetProviderId(PROVIDER_ID));
    step!("SetVersion", info.SetVersion(&HSTRING::from("1.0.0.0")));
    // Context is a required provider-defined blob; a small non-empty buffer suffices.
    let ctx = step!("CreateContext", CryptographicBuffer::CreateFromByteArray(b"DropboxSync"));
    step!("SetContext", info.SetContext(&ctx));
    // Clear any stale/partial registration for this id first, so a leftover entry
    // (e.g. one whose nav-pane node was stripped by `enable-status-column`, or a
    // crash remnant) can't make Register fail with 0x80070490 (ERROR_NOT_FOUND).
    // That failure is sticky against a stripped entry on the first try; a fresh
    // Unregister + Register retry clears it (observed empirically).
    let id_h = HSTRING::from(&id);
    let mut result = {
        let _ = StorageProviderSyncRootManager::Unregister(&id_h);
        StorageProviderSyncRootManager::Register(&info)
    };
    for attempt in 1..=3 {
        if result.is_ok() {
            break;
        }
        tracing::warn!(attempt, error = ?result, "cfapi Register failed; retrying after Unregister");
        let _ = StorageProviderSyncRootManager::Unregister(&id_h);
        result = StorageProviderSyncRootManager::Register(&info);
    }
    step!("Register", result);
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
