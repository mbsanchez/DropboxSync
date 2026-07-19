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
use std::time::{Duration, Instant};

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
    CfHydratePlaceholder, CfRevertPlaceholder, CfSetInSyncState, CfSetPinState,
    CF_CALLBACK_DELETE_FLAG_IS_DIRECTORY, CF_CALLBACK_INFO, CF_CALLBACK_PARAMETERS,
    CF_CALLBACK_REGISTRATION, CF_CALLBACK_RENAME_FLAG_IS_DIRECTORY, CF_CALLBACK_TYPE,
    CF_CALLBACK_TYPE_CANCEL_FETCH_DATA, CF_CALLBACK_TYPE_FETCH_DATA,
    CF_CALLBACK_TYPE_NOTIFY_DELETE, CF_CALLBACK_TYPE_NOTIFY_DELETE_COMPLETION,
    CF_CALLBACK_TYPE_NOTIFY_RENAME, CF_CALLBACK_TYPE_NOTIFY_RENAME_COMPLETION, CF_CONNECTION_KEY,
    CF_CONNECT_FLAGS, CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH, CF_CONNECT_FLAG_REQUIRE_PROCESS_INFO,
    CF_CONVERT_FLAG_MARK_IN_SYNC, CF_CREATE_FLAG_NONE, CF_FS_METADATA, CF_HYDRATE_FLAG_NONE,
    CF_IN_SYNC_STATE_IN_SYNC, CF_IN_SYNC_STATE_NOT_IN_SYNC, CF_OPERATION_ACK_DELETE_FLAG_NONE,
    CF_OPERATION_ACK_RENAME_FLAG_NONE, CF_OPERATION_INFO, CF_OPERATION_PARAMETERS,
    CF_OPERATION_PARAMETERS_0, CF_OPERATION_PARAMETERS_0_0, CF_OPERATION_PARAMETERS_0_6,
    CF_OPERATION_PARAMETERS_0_7, CF_OPERATION_TRANSFER_DATA_FLAG_NONE,
    CF_OPERATION_TYPE_ACK_DELETE, CF_OPERATION_TYPE_ACK_RENAME, CF_OPERATION_TYPE_TRANSFER_DATA,
    CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC, CF_PLACEHOLDER_CREATE_INFO, CF_PIN_STATE_PINNED,
    CF_REVERT_FLAG_NONE, CF_SET_IN_SYNC_FLAG_NONE, CF_SET_PIN_FLAG_NONE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_ARCHIVE, FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentProcessId, OpenProcessToken};

use crate::error::{AppError, AppResult};

/// Stable provider id. NEVER change once shipped.
const PROVIDER_ID: GUID = GUID::from_u128(0xEA067F54_0C87_4DC2_9F90_3A632C2AAF9C);

/// The sync-root folder currently registered (register once per folder).
static REGISTERED: OnceLock<Mutex<Option<String>>> = OnceLock::new();
/// `rel → last-applied in-sync bool`, so we only touch a file when it changes.
static APPLIED: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
/// Directories already converted to in-sync placeholders this session (DBSYNC-59):
/// converting once is enough — under PopulationPolicy=AlwaysFull the shell then
/// tracks the folder's aggregate status from its children automatically.
static APPLIED_FOLDERS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

/// Timestamp of the last sync-root (un)registration. Registering a sync root does an
/// `Unregister` + `Register`, and `Unregister` **evicts the dehydrated placeholders
/// from disk** — the files vanish locally. Without a guard the sync engine reads that
/// mass disappearance as user deletes and propagates them to Dropbox (the DBSYNC-62
/// data-loss incident). Deletion propagation is therefore suppressed for a grace
/// window after any real (re)registration (see `in_post_registration_grace`), during
/// which the indexer re-materializes the evicted placeholders. This has zero effect on
/// normal launches, which REUSE the existing registration and never (re)register.
static LAST_REGISTRATION_AT: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();

/// Grace window after a (re)registration during which remote-delete propagation is
/// suppressed. Comfortably longer than the 5-min reconciliation sweep so the indexer
/// re-creates the evicted placeholders before deletes could resume.
const POST_REGISTRATION_GRACE: Duration = Duration::from_secs(15 * 60);

/// Record that a sync-root (un)registration just happened (starts the grace window).
fn mark_registration_activity() {
    if let Ok(mut g) = LAST_REGISTRATION_AT
        .get_or_init(|| Mutex::new(None))
        .lock()
    {
        *g = Some(Instant::now());
    }
}

/// True while within [`POST_REGISTRATION_GRACE`] of the last (un)registration — the
/// shell may still be evicting placeholders, so a locally-vanished tracked file must
/// NOT be propagated as a Dropbox delete. Checked by the sync pipeline's delete paths.
pub(crate) fn in_post_registration_grace() -> bool {
    LAST_REGISTRATION_AT
        .get()
        .and_then(|m| m.lock().ok().and_then(|g| *g))
        .map(|t| t.elapsed() < POST_REGISTRATION_GRACE)
        .unwrap_or(false)
}

/// `prefix (armed subtree) → last-armed-at`. Armed by the placeholder-materialization
/// drivers (`cloudsc_ops.rs`, DBSYNC-66 Slice 1) whenever they are about to create a
/// placeholder / real directory for restored content. While a prefix is armed, CfAPI
/// churn under it (Explorer/the shell deleting-then-recreating placeholders as part of
/// materializing a large restored subtree) must NOT be read as a user delete — see
/// `in_materialization_grace` and its use in `on_notify_delete` /
/// `delete_suppressed_by_dehydration`.
static MATERIALIZATION_IN_FLIGHT: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();

/// Grace window during which an armed subtree's delete notifications are suppressed.
/// Comfortably longer than the observed ~26s restore-churn lag (see the Slice 1 issue).
const MATERIALIZATION_GRACE_WINDOW: Duration = Duration::from_secs(120);

/// Arm `prefix` (a `/`-relative path — a folder's own rel, or a file's parent rel) for
/// [`MATERIALIZATION_GRACE_WINDOW`], starting/refreshing its grace window. Also prunes
/// any already-expired entries so the map doesn't grow unbounded across a long-running
/// session (no separate GC needed).
pub(crate) fn mark_materialization(prefix: &str) {
    if let Ok(mut m) = MATERIALIZATION_IN_FLIGHT.get_or_init(|| Mutex::new(HashMap::new())).lock() {
        let now = Instant::now();
        m.retain(|_, at| {
            now.checked_duration_since(*at)
                .map(|d| d < MATERIALIZATION_GRACE_WINDOW)
                .unwrap_or(true)
        });
        m.insert(prefix.to_string(), now);
    }
}

/// Pure decision (unit-testable, `now`/`window` injected — no wall-clock): is `rel`
/// under an armed, still-within-window prefix? `rel == prefix` also matches (a folder
/// arms its own rel; a delete notification can fire for the folder itself, not just a
/// child). Boundary-safe: `"foo/bar"` does NOT match `rel = "foo/barbaz"` — only an
/// exact match or a `/`-separated descendant counts.
fn materialization_grace_contains(
    entries: &HashMap<String, Instant>,
    rel: &str,
    now: Instant,
    window: Duration,
) -> bool {
    entries.iter().any(|(prefix, at)| {
        let in_window = now.checked_duration_since(*at).map(|d| d < window).unwrap_or(true);
        in_window && (rel == prefix || rel.starts_with(&format!("{prefix}/")))
    })
}

/// True when `rel` falls under a subtree currently armed by [`mark_materialization`]
/// (within its grace window) — i.e. a delete notification for it is very likely
/// materialization churn (a restore re-creating placeholders), not a genuine user
/// delete. Checked both at CfAPI event time (`on_notify_delete`) and at drain time
/// (`delete_suppressed_by_dehydration`), matching the `in_post_registration_grace`
/// two-checkpoint pattern.
pub(crate) fn in_materialization_grace(rel: &str) -> bool {
    let Some(lock) = MATERIALIZATION_IN_FLIGHT.get() else {
        return false;
    };
    let Ok(m) = lock.lock() else {
        return false;
    };
    materialization_grace_contains(&m, rel, Instant::now(), MATERIALIZATION_GRACE_WINDOW)
}

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

/// Give each real directory under the sync root a CfAPI placeholder identity so
/// Explorer's status column shows its aggregate state (e.g. the cloud icon when all
/// children are online-only) instead of a perpetual "syncing" glyph (DBSYNC-59). A
/// plain directory in a sync root has no state for the shell to render; our files are
/// placeholders (created via `CfCreatePlaceholders`) but folders are created as plain
/// dirs, which is why only folders showed "syncing". Converting once per session is
/// enough — under PopulationPolicy=AlwaysFull the shell then tracks the aggregate from
/// the children. `folder_rels` are `/`-relative paths under the sync root. No-ops
/// without package identity / registration. Never pins (that would hydrate children).
pub(crate) fn sync_folder_states(sync_folder: Option<&str>, folder_rels: &[String]) {
    let Some(folder) = sync_folder else {
        return;
    };
    if !crate::windows_identity::has_package_identity() || !ensure_registered(folder) {
        return;
    }
    let root = Path::new(folder);
    let applied = APPLIED_FOLDERS.get_or_init(|| Mutex::new(HashSet::new()));
    let Ok(mut applied) = applied.lock() else {
        return;
    };
    for rel in folder_rels {
        if rel.is_empty() || applied.contains(rel) {
            continue;
        }
        let abs = root.join(rel);
        if !abs.is_dir() {
            continue;
        }
        if apply_folder_state(&abs) {
            applied.insert(rel.clone());
        }
    }
}

/// Convert a directory to an in-sync placeholder (no pin) so the shell renders its
/// aggregate status. Under PopulationPolicy=AlwaysFull this never hides children.
/// Fails soft.
fn apply_folder_state(abs: &Path) -> bool {
    let path = to_wide(&abs.to_string_lossy());
    unsafe {
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
                tracing::debug!(dir = %abs.display(), error = %e, "apply_folder_state: open failed");
                return false;
            }
        };
        // Convert to a placeholder (ignore "already a placeholder") + mark in-sync.
        // NEVER pin: pinning a directory would force-hydrate every child.
        let conv = CfConvertToPlaceholder(handle, None, 0, CF_CONVERT_FLAG_MARK_IN_SYNC, None, None);
        let sync = CfSetInSyncState(handle, CF_IN_SYNC_STATE_IN_SYNC, CF_SET_IN_SYNC_FLAG_NONE, None);
        let _ = CloseHandle(handle);
        tracing::debug!(dir = %abs.display(), convert = ?conv, set_in_sync = ?sync, "cfapi apply_folder_state");
        true
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

/// Sync-root absolute path for the CURRENT live `CfConnectSyncRoot` connection, set
/// by `connect_provider` on success (DBSYNC-64 part 2). Lets `NOTIFY_DELETE` /
/// `NOTIFY_RENAME` resolve a callback path to a `/`-relative one WITHOUT a DB read
/// from inside the ABI callback — unlike `FETCH_DATA` (which the platform expects
/// to block while we download), these callbacks must ACK promptly.
static CONNECTED_ROOT: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

fn set_connected_root(folder: &str) {
    if let Ok(mut g) = CONNECTED_ROOT.get_or_init(|| Mutex::new(None)).lock() {
        *g = Some(PathBuf::from(folder));
    }
}

fn connected_root() -> Option<PathBuf> {
    CONNECTED_ROOT.get()?.lock().ok()?.clone()
}

/// Resolve an absolute placeholder path to its `/`-relative path under the
/// currently-connected sync root, or `None` if unresolvable (no live connection,
/// or the path isn't under the root). No DB access — see `CONNECTED_ROOT`.
fn rel_under_connected_root(abs: &Path) -> Option<String> {
    let root = connected_root()?;
    crate::path_util::relpath_under(&root, abs).ok()
}

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
    // Terminated callback table: FETCH_DATA (hydrate on open) + CANCEL, plus the
    // DBSYNC-64 part 2 authoritative user-delete/rename signals, + END.
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
            Type: CF_CALLBACK_TYPE_NOTIFY_DELETE,
            Callback: Some(on_notify_delete),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NOTIFY_DELETE_COMPLETION,
            Callback: Some(on_notify_delete_completion),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NOTIFY_RENAME,
            Callback: Some(on_notify_rename),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NOTIFY_RENAME_COMPLETION,
            Callback: Some(on_notify_rename_completion),
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
            set_connected_root(folder);
            tracing::info!(folder, "cfapi provider connected (on-demand hydration + event-driven deletes)");
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

/// Join a CfAPI-normalized, DRIVE-LESS path (e.g. `\Users\me\Dropbox\file.txt`) with
/// the callback's `VolumeDosName` (e.g. `C:`) into a full absolute path. EVERY
/// CfAPI-supplied path field is drive-less this way — `CF_CALLBACK_INFO.NormalizedPath`
/// AND `CF_CALLBACK_PARAMETERS.RenameCompletion.SourcePath` alike — so this is the ONE
/// helper for all of them (DBSYNC-64 CTO review H1: `on_notify_rename_completion`
/// previously resolved `SourcePath` as if it were already absolute via a bare
/// `PCWSTR::to_string`, which silently failed `relpath_under` against the sync root
/// and dropped every rename-completion event before `take_rename_in_flight` could even
/// run, leaking its in-flight marker).
unsafe fn full_path_from(vol: PCWSTR, normalized: PCWSTR) -> Option<PathBuf> {
    let vol = vol.to_string().ok()?;
    let norm = normalized.to_string().ok()?;
    if norm.is_empty() {
        return None;
    }
    Some(PathBuf::from(format!("{vol}{norm}")))
}

/// Reconstruct the placeholder's absolute path from the callback info
/// (`VolumeDosName` = `C:` + `NormalizedPath` = `\Users\…\file`).
unsafe fn full_local_path(info: &CF_CALLBACK_INFO) -> Option<PathBuf> {
    full_path_from(info.VolumeDosName, info.NormalizedPath)
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

// ── DBSYNC-64 part 2: event-driven deletes ──────────────────────────────────────
//
// While the provider is connected, CfAPI authoritatively tells us when the user
// deletes or renames a placeholder under the sync root — NOTIFY_DELETE(_COMPLETION)
// and NOTIFY_RENAME(_COMPLETION). That confirmed signal drives the remote delete,
// rather than relying solely on the scan's "tracked file absent on disk" inference.
//
// This increment ADDS the authoritative path; it does NOT remove the scan-inferred
// delete (`process_local_file_deletion` / `process_known_folder_deletion` in
// `sync_pipeline.rs`), which remains an offline-fallback backstop. Any duplicate
// `delete` job the two paths might both enqueue collapses via `enqueue_job`'s
// partial-unique-index dedup — see `storage/db.rs`. Disabling the scan inference
// while the provider is connected is a follow-up (needs live validation first).
//
// Every enqueued delete still runs through the normal drain-time guards
// (`delete_suppressed_by_dehydration`: DBSYNC-45 `.cloudsc`/native-dehydration
// suppression, DBSYNC-62 post-registration grace) — this is additive, not a bypass.
//
// CRITICAL (DBSYNC-64 CTO review C1): CfAPI fires NOTIFY_DELETE/NOTIFY_RENAME for ANY
// placeholder delete/rename under the sync root, INCLUDING our OWN `fs::remove_file`/
// `fs::remove_dir`/rename calls (free-up-space collapse in `cloudsc_ops.rs`, "use
// remote" conflict resolution, `local_delete`). Those operate on fully-synced
// placeholders whose content still lives on Dropbox — treating them as a user event
// would enqueue a Dropbox delete that the dehydration guard does NOT catch (the file
// is genuinely gone, no `.cloudsc` survives it), silently deleting the user's Dropbox
// content. `on_notify_delete`/`on_notify_rename` therefore check `is_own_process`
// (via `CF_CALLBACK_INFO.ProcessInfo`, populated by `CF_CONNECT_FLAG_REQUIRE_PROCESS_INFO`)
// and skip marking a self-initiated op in-flight — its completion then has no marker to
// consume and enqueues nothing. See `is_own_process` for the full rationale.

/// In-flight user-initiated deletes observed by `on_notify_delete`: rel → was a
/// directory. NOTIFY_DELETE_COMPLETION's parameters carry no IS_DIRECTORY flag, so
/// it must be captured at the pre-delete callback and handed across. Consumed
/// (removed) by `on_notify_delete_completion`.
static USER_DELETES_IN_FLIGHT: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
/// Same idea for `on_notify_rename` → `on_notify_rename_completion`, keyed by the
/// SOURCE rel (the path before the rename, which is stable across both callbacks).
static PENDING_RENAMES_IN_FLIGHT: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();

fn mark_in_flight(slot: &'static OnceLock<Mutex<HashMap<String, bool>>>, rel: &str, is_dir: bool) {
    if let Ok(mut m) = slot.get_or_init(|| Mutex::new(HashMap::new())).lock() {
        m.insert(rel.to_string(), is_dir);
    }
}

fn take_in_flight(slot: &'static OnceLock<Mutex<HashMap<String, bool>>>, rel: &str) -> Option<bool> {
    slot.get()?.lock().ok()?.remove(rel)
}

fn mark_user_delete_in_flight(rel: &str, is_dir: bool) {
    mark_in_flight(&USER_DELETES_IN_FLIGHT, rel, is_dir);
}

fn take_user_delete_in_flight(rel: &str) -> Option<bool> {
    take_in_flight(&USER_DELETES_IN_FLIGHT, rel)
}

fn mark_rename_in_flight(rel: &str, is_dir: bool) {
    mark_in_flight(&PENDING_RENAMES_IN_FLIGHT, rel, is_dir);
}

fn take_rename_in_flight(rel: &str) -> Option<bool> {
    take_in_flight(&PENDING_RENAMES_IN_FLIGHT, rel)
}

/// True when `info.ProcessInfo` identifies THIS process as the one performing the
/// delete/rename (DBSYNC-64 CTO review C1, CRITICAL / data-loss). CfAPI fires
/// `NOTIFY_DELETE`/`NOTIFY_RENAME` for EVERY placeholder delete/rename under the sync
/// root — including our OWN `fs::remove_file`/`fs::remove_dir`/rename calls (the
/// free-up-space collapse in `cloudsc_ops.rs`, "use remote" conflict resolution,
/// `local_delete`). Those delete/rename fully-synced placeholders whose content still
/// lives on Dropbox; if treated as a user event, the completion handler would enqueue
/// a Dropbox delete that `delete_suppressed_by_dehydration` does NOT catch (the file
/// is genuinely gone locally, no per-file `.cloudsc` survives it) — silently deleting
/// the user's Dropbox content (recursively, for a folder). `ProcessInfo` is populated
/// because the provider connects with `CF_CONNECT_FLAG_REQUIRE_PROCESS_INFO`. A null
/// `ProcessInfo` (should not happen given that flag, but fails safe in the direction
/// that only risks a harmless duplicate rather than a dropped user delete) is treated
/// as NOT self, i.e. still tracked as a genuine user event.
unsafe fn is_own_process(info: &CF_CALLBACK_INFO) -> bool {
    if info.ProcessInfo.is_null() {
        return false;
    }
    (*info.ProcessInfo).ProcessId == GetCurrentProcessId()
}

/// Pure classification of a rename TARGET against the sync root: `Some(rel)` when
/// `target` is still under `root` (a move WITHIN the sync root — `rel` is the new
/// `/`-relative path); `None` when `target` fell OUTSIDE `root` entirely. Per the
/// DBSYNC-64 CTO review (H2), the completion handler treats these asymmetrically:
/// within-root → NO-OP (no delete enqueued this increment — see
/// `on_notify_rename_completion`); outside-root → delete-of-source. No I/O —
/// unit-testable without a live sync root.
fn classify_rename_target(root: &Path, target: &Path) -> Option<String> {
    crate::path_util::relpath_under(root, target).ok()
}

/// Pure decision (DBSYNC-64 CTO review H2) for `on_notify_rename_completion`: given
/// `classify_rename_target`'s result, should the completed rename propagate a delete
/// of the source? `Some(_)` (target still under the sync root) → `false` — a move
/// within the root is a NO-OP this increment (see `on_notify_rename_completion` for
/// why: it would otherwise orphan a moved DEHYDRATED placeholder's Dropbox content).
/// `None` (target fell outside the sync root) → `true` — nothing is left behind, so
/// deleting the source is safe. No I/O — unit-testable without a live sync root.
fn rename_completion_should_delete_source(target_rel: &Option<String>) -> bool {
    target_rel.is_none()
}

/// `NOTIFY_DELETE`: fires BEFORE a placeholder under the sync root is deleted. We
/// never veto a user's delete — we only observe it (remembering whether it was a
/// directory, needed by the completion handler) and ACK immediately so the delete
/// proceeds. CRITICAL: registering this callback type makes CfAPI WAIT for our ACK
/// before the delete completes, so this must never block (no DB/network here — see
/// `CONNECTED_ROOT`). DBSYNC-64 CTO review C1: a delete performed by OUR OWN process
/// (`is_own_process`) is deliberately NOT marked in-flight — see `is_own_process` for
/// why treating our own `fs::remove_file` calls as a "user delete" is a data-loss bug.
/// Panic-firewalled like `on_fetch_data` — a panic crossing this `extern "system"`
/// boundary would abort the process.
unsafe extern "system" fn on_notify_delete(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    if info.is_null() {
        return;
    }
    let info = &*info;
    let is_dir = if params.is_null() {
        false
    } else {
        (*params).Anonymous.Delete.Flags.contains(CF_CALLBACK_DELETE_FLAG_IS_DIRECTORY)
    };
    let _: Option<String> = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let rel = full_local_path(info).and_then(|abs| rel_under_connected_root(&abs))?;
        if is_own_process(info) {
            tracing::debug!(rel = %rel, is_dir, "cfapi notify-delete: self-initiated (our own process) — not tracked, no remote delete will be enqueued");
        } else if in_materialization_grace(&rel) {
            mark_materialization(&rel); // refresh — keeps active churn armed
            tracing::info!(rel = %rel, is_dir, "cfapi notify-delete: suppressed — under active materialization grace (restore churn, not a user delete)");
        } else {
            mark_user_delete_in_flight(&rel, is_dir);
            tracing::debug!(rel = %rel, is_dir, "cfapi notify-delete: user delete observed");
        }
        Some(rel)
    }))
    .unwrap_or(None);
    // Never veto, never block: ACK unconditionally, even if path resolution above
    // failed or panicked — the user's delete must proceed either way.
    let _ = ack_delete(info.ConnectionKey, info.TransferKey, STATUS_SUCCESS);
}

/// `NOTIFY_DELETE_COMPLETION`: fires AFTER the delete completed, i.e. the file is
/// CONFIRMED gone by user action. Only now do we propagate it to Dropbox — routed
/// off-thread via `enqueue_remote_delete_async` (never touch the DB/network from
/// inside this ABI callback). If no matching `on_notify_delete` marker is found
/// (unexpected callback ordering, or the provider connected mid-delete) we
/// conservatively do nothing here — the scan/watcher backstop still covers it this
/// increment. Panic-firewalled like `on_fetch_data`.
unsafe extern "system" fn on_notify_delete_completion(
    info: *const CF_CALLBACK_INFO,
    _params: *const CF_CALLBACK_PARAMETERS,
) {
    if info.is_null() {
        return;
    }
    let info = &*info;
    let rel: Option<String> = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        full_local_path(info).and_then(|abs| rel_under_connected_root(&abs))
    }))
    .unwrap_or(None);
    let Some(rel) = rel else {
        return;
    };
    match take_user_delete_in_flight(&rel) {
        Some(is_dir) => enqueue_remote_delete_async(rel, is_dir),
        None => {
            tracing::debug!(rel = %rel, "cfapi notify-delete-completion: no matching in-flight marker; skipping (scan backstop covers it)");
        }
    }
}

/// `NOTIFY_RENAME`: fires BEFORE a placeholder under the sync root is renamed or
/// moved. We never veto — we only record whether it was a directory (needed by the
/// completion handler, same reasoning as `on_notify_delete`) and ACK promptly. DBSYNC-64
/// CTO review C1: a rename performed by OUR OWN process is NOT marked in-flight — same
/// data-loss reasoning as `on_notify_delete` (see `is_own_process`). All routing happens
/// on completion (`on_notify_rename_completion`), where both the source and final path
/// are available. Panic-firewalled like `on_fetch_data`.
unsafe extern "system" fn on_notify_rename(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    if info.is_null() {
        return;
    }
    let info = &*info;
    let is_dir = if params.is_null() {
        false
    } else {
        (*params).Anonymous.Rename.Flags.contains(CF_CALLBACK_RENAME_FLAG_IS_DIRECTORY)
    };
    let _: Option<String> = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let rel = full_local_path(info).and_then(|abs| rel_under_connected_root(&abs))?;
        if is_own_process(info) {
            tracing::debug!(rel = %rel, is_dir, "cfapi notify-rename: self-initiated (our own process) — not tracked, no remote delete will be enqueued");
        } else {
            mark_rename_in_flight(&rel, is_dir);
        }
        Some(rel)
    }))
    .unwrap_or(None);
    let _ = ack_rename(info.ConnectionKey, info.TransferKey, STATUS_SUCCESS);
}

/// `NOTIFY_RENAME_COMPLETION`: fires AFTER the rename/move completed.
/// `CF_CALLBACK_INFO.NormalizedPath` (via `full_local_path`) is the item's NEW
/// (post-rename) location; `params.RenameCompletion.SourcePath` is the ORIGINAL
/// location — ALSO drive-less, so it's resolved via the SAME `full_path_from` helper
/// (with `info.VolumeDosName`) as the target, not a bare `PCWSTR::to_string` (DBSYNC-64
/// CTO review H1: the previous bare resolution always failed `relpath_under` against the
/// absolute sync root, so `source_rel` was always `None`, the function returned BEFORE
/// `take_rename_in_flight` ran, and every `on_notify_rename` marker leaked forever).
///
/// Handling for this increment (kept deliberately conservative, per DBSYNC-64 part 2
/// scope, tightened by CTO review H2):
/// - Target still under the sync root (a move/rename within it): NO-OP — ACK already
///   happened in `on_notify_rename`; here we only log and enqueue NOTHING. Moving a
///   DEHYDRATED placeholder within the root would otherwise delete the (still
///   Dropbox-only) source while the new path is silently skipped by the scan's
///   dehydrated-placeholder guard (`process_local_file_change`) — the content would be
///   deleted on Dropbox with NO local or remote copy left (data loss). Deferring
///   entirely to the existing scan/watcher for real (hydrated) files is safe (nothing
///   is deleted); a dedicated move-aware path (`move_v2`, handling both hydrated and
///   dehydrated placeholders) is a documented follow-up.
/// - Target outside the sync root: unambiguously a delete of the source (nothing at
///   the target for a dehydrated placeholder to orphan).
///
/// If no matching `on_notify_rename` marker is found (self-initiated rename per C1, or
/// unexpected callback ordering) we skip (same conservative default as
/// delete-completion). Panic-firewalled like `on_fetch_data`.
unsafe extern "system" fn on_notify_rename_completion(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    if info.is_null() || params.is_null() {
        return;
    }
    let info = &*info;
    let params = &*params;
    let outcome: Option<(String, Option<String>)> =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let root = connected_root()?;
            let source_abs =
                full_path_from(info.VolumeDosName, params.Anonymous.RenameCompletion.SourcePath)?;
            let source_rel = crate::path_util::relpath_under(&root, &source_abs).ok()?;
            let target_rel =
                full_local_path(info).and_then(|target_abs| classify_rename_target(&root, &target_abs));
            Some((source_rel, target_rel))
        }))
        .unwrap_or(None);

    let Some((source_rel, target_rel)) = outcome else {
        return;
    };
    // Consumed unconditionally (matching in-flight marker or not) — never leaves a
    // dangling entry, regardless of which branch below runs (DBSYNC-64 CTO review M1).
    let Some(is_dir) = take_rename_in_flight(&source_rel) else {
        tracing::debug!(rel = %source_rel, "cfapi notify-rename-completion: no matching in-flight marker (self-initiated, or unexpected ordering); skipping (scan backstop covers it)");
        return;
    };
    if rename_completion_should_delete_source(&target_rel) {
        tracing::info!(
            source = %source_rel,
            "cfapi notify-rename-completion: moved outside the sync root — propagating delete(source)"
        );
        enqueue_remote_delete_async(source_rel, is_dir);
    } else {
        let new_rel = target_rel.as_deref().unwrap_or("?");
        tracing::info!(
            source = %source_rel, target = %new_rel,
            "cfapi notify-rename-completion: move within sync root — deferring entirely to the existing scan/watcher (no remote delete enqueued this increment, per DBSYNC-64 CTO review H2; a proper Dropbox move is a documented follow-up)"
        );
    }
}

/// One `CfExecute(CF_OPERATION_TYPE_ACK_DELETE)` — unconditionally lets the
/// platform's delete proceed (we never veto). Mirrors `cf_transfer`'s
/// `CF_OPERATION_INFO` / `CF_OPERATION_PARAMETERS` construction.
unsafe fn ack_delete(conn: CF_CONNECTION_KEY, transfer: i64, status: NTSTATUS) -> windows::core::Result<()> {
    let op_info = CF_OPERATION_INFO {
        StructSize: std::mem::size_of::<CF_OPERATION_INFO>() as u32,
        Type: CF_OPERATION_TYPE_ACK_DELETE,
        ConnectionKey: conn,
        TransferKey: transfer,
        CorrelationVector: std::ptr::null(),
        SyncStatus: std::ptr::null(),
        RequestKey: 0,
    };
    let mut params = CF_OPERATION_PARAMETERS {
        ParamSize: (core::mem::offset_of!(CF_OPERATION_PARAMETERS, Anonymous)
            + std::mem::size_of::<CF_OPERATION_PARAMETERS_0_7>()) as u32,
        Anonymous: CF_OPERATION_PARAMETERS_0 {
            AckDelete: CF_OPERATION_PARAMETERS_0_7 {
                Flags: CF_OPERATION_ACK_DELETE_FLAG_NONE,
                CompletionStatus: status,
            },
        },
    };
    CfExecute(&op_info, &mut params)
}

/// One `CfExecute(CF_OPERATION_TYPE_ACK_RENAME)` — unconditionally lets the
/// platform's rename/move proceed (we never veto). Mirrors `cf_transfer`'s
/// `CF_OPERATION_INFO` / `CF_OPERATION_PARAMETERS` construction.
unsafe fn ack_rename(conn: CF_CONNECTION_KEY, transfer: i64, status: NTSTATUS) -> windows::core::Result<()> {
    let op_info = CF_OPERATION_INFO {
        StructSize: std::mem::size_of::<CF_OPERATION_INFO>() as u32,
        Type: CF_OPERATION_TYPE_ACK_RENAME,
        ConnectionKey: conn,
        TransferKey: transfer,
        CorrelationVector: std::ptr::null(),
        SyncStatus: std::ptr::null(),
        RequestKey: 0,
    };
    let mut params = CF_OPERATION_PARAMETERS {
        ParamSize: (core::mem::offset_of!(CF_OPERATION_PARAMETERS, Anonymous)
            + std::mem::size_of::<CF_OPERATION_PARAMETERS_0_6>()) as u32,
        Anonymous: CF_OPERATION_PARAMETERS_0 {
            AckRename: CF_OPERATION_PARAMETERS_0_6 {
                Flags: CF_OPERATION_ACK_RENAME_FLAG_NONE,
                CompletionStatus: status,
            },
        },
    };
    CfExecute(&op_info, &mut params)
}

// ── DBSYNC-66 Slice 2: coalesced folder deletes ─────────────────────────────────
//
// Deleting a folder tree makes CfAPI fire ONE NOTIFY_DELETE(_COMPLETION) pair per
// DESCENDANT (every file + every subfolder) — e.g. 32 events for a 32-item tree.
// Turning each into its own `files/delete_v2` job serializes 32 network round-trips
// (~30s observed) and, worse, holds the delete queue open for that whole window,
// during which a concurrent restore can re-materialize part of the tree the user
// just deleted. Dropbox's `delete_v2` on a FOLDER is already recursive — deleting
// the top-level folder alone deletes its entire remote subtree — so the fix is to
// BUFFER the burst of per-descendant events and, once it goes quiet, collapse it to
// the MINIMAL set of top-level roots and enqueue exactly one recursive `delete_v2`
// job per root (`minimal_delete_roots` / `flush_pending_deletes`).

/// One buffered CfAPI delete event, captured from a confirmed
/// `on_notify_delete_completion` (or a rename treated as a delete), awaiting
/// coalescing by the debounce-flush worker.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingDelete {
    rel: String,
    is_dir: bool,
}

/// Delete events observed since the last flush. Drained wholesale (and coalesced —
/// see `minimal_delete_roots`) by `run_delete_flush_worker` once the burst goes
/// quiet for [`DELETE_DEBOUNCE_WINDOW`].
static PENDING_EVENT_DELETES: OnceLock<Mutex<Vec<PendingDelete>>> = OnceLock::new();
/// Instant of the most recent push into `PENDING_EVENT_DELETES`, used by the
/// debounce worker to decide whether the burst has gone quiet.
static PENDING_DELETES_LAST_ACTIVITY: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
/// Guards against spawning more than one debounce-flush worker at a time. Set by
/// whichever `enqueue_remote_delete_async` call wins the race to start a worker;
/// cleared by the worker itself once the buffer has been empty for a couple of
/// polls (with a re-check to avoid dropping a push that lands in that exact
/// window — see `run_delete_flush_worker`).
static DELETE_FLUSH_RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// How long the buffer must be quiet (no new pushes) before it is flushed.
/// Comfortably longer than the gap between consecutive descendant
/// NOTIFY_DELETE_COMPLETION events within a single folder-delete burst, so the
/// whole burst is captured by one flush.
const DELETE_DEBOUNCE_WINDOW: Duration = Duration::from_millis(700);
/// Poll interval for the debounce worker.
const DELETE_DEBOUNCE_POLL: Duration = Duration::from_millis(400);

/// Buffer one CfAPI delete event and make sure a debounce-flush worker is running.
/// Runs off the CfAPI callback thread (called from `enqueue_remote_delete_async`,
/// itself only ever called from a spawned thread) — cheap, no DB/network here.
fn push_pending_delete(rel: String, is_dir: bool) {
    if let Ok(mut buf) = PENDING_EVENT_DELETES
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
    {
        buf.push(PendingDelete { rel, is_dir });
    }
    if let Ok(mut last) = PENDING_DELETES_LAST_ACTIVITY
        .get_or_init(|| Mutex::new(None))
        .lock()
    {
        *last = Some(Instant::now());
    }
    ensure_delete_flush_worker();
}

/// Start the debounce-flush worker if one isn't already running (single-thread
/// gate via `DELETE_FLUSH_RUNNING`). A no-op when a worker is already draining the
/// buffer — that worker will pick up this push on its next poll.
fn ensure_delete_flush_worker() {
    use std::sync::atomic::Ordering;
    if DELETE_FLUSH_RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    std::thread::spawn(run_delete_flush_worker);
}

/// The single debounce-flush worker thread: polls every [`DELETE_DEBOUNCE_POLL`],
/// and once the buffer has been quiet for [`DELETE_DEBOUNCE_WINDOW`] and is
/// non-empty, takes the whole buffer and flushes it (`flush_pending_deletes`).
/// Stops once the buffer has been empty for a couple of polls — but re-checks
/// after clearing `DELETE_FLUSH_RUNNING` so a push landing in the narrow window
/// between "decided to stop" and "flag cleared" is still picked up (by this same
/// worker looping again, or ceded to a fresh one if another thread already won the
/// race), rather than silently stranded in the buffer.
fn run_delete_flush_worker() {
    use std::sync::atomic::Ordering;
    loop {
        let mut idle_polls = 0u32;
        loop {
            std::thread::sleep(DELETE_DEBOUNCE_POLL);
            let quiet_long_enough = PENDING_DELETES_LAST_ACTIVITY
                .get()
                .and_then(|m| m.lock().ok().and_then(|g| *g))
                .map(|t| t.elapsed() >= DELETE_DEBOUNCE_WINDOW)
                .unwrap_or(true);
            let batch = if quiet_long_enough {
                PENDING_EVENT_DELETES
                    .get()
                    .and_then(|m| m.lock().ok().map(|mut b| std::mem::take(&mut *b)))
            } else {
                None
            };
            match batch {
                Some(batch) if !batch.is_empty() => {
                    idle_polls = 0;
                    match app_state() {
                        Some(state) => flush_pending_deletes(&state, batch),
                        None => tracing::debug!(
                            "cfapi event-delete: app state unavailable; dropping buffered batch (scan backstop covers it)"
                        ),
                    }
                }
                _ => {
                    idle_polls += 1;
                    if idle_polls >= 2 {
                        break;
                    }
                }
            }
        }
        DELETE_FLUSH_RUNNING.store(false, Ordering::Release);
        let more_pending = PENDING_EVENT_DELETES
            .get()
            .and_then(|m| m.lock().ok().map(|b| !b.is_empty()))
            .unwrap_or(false);
        if !more_pending {
            break;
        }
        if DELETE_FLUSH_RUNNING
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            break; // another thread already spawned a fresh worker; let it take over
        }
    }
}

/// Pure core of the coalescing (DBSYNC-66 Slice 2): given the descendant-level
/// `(rel, is_dir)` events buffered from one delete burst, return only the entries
/// that have no OTHER entry in `batch` as an ancestor. Dropbox's `delete_v2` on a
/// folder is already recursive, so a root's own delete job already covers every
/// entry it is an ancestor of — only the roots need a job. Boundary-safe (mirrors
/// `materialization_grace_contains`): `"foo/bar"` is NOT an ancestor of
/// `"foo/barbaz"` — only an exact `/`-separated descendant counts. Pure / no I/O,
/// no wall-clock — fully unit-testable.
fn minimal_delete_roots(batch: &[(String, bool)]) -> Vec<(String, bool)> {
    batch
        .iter()
        .filter(|(rel, _)| {
            !batch
                .iter()
                .any(|(other, _)| other != rel && rel.starts_with(&format!("{other}/")))
        })
        .cloned()
        .collect()
}

/// Flush one debounced batch: coalesce it to its minimal root set
/// (`minimal_delete_roots`), enqueue ONE recursive `delete_v2` job per root, untrack
/// EVERY entry in the batch from the local index (so it matches what was deleted —
/// the remote index rows for the deleted subtree are cleared by the delete drain's
/// `remove_remote_file` / `prune_empty_deleted_ancestors`), then kick a single
/// drain. Draining goes through the normal `drain_sync_queue`, so the DBSYNC-45/62
/// guards (`delete_suppressed_by_dehydration`) still apply per job at execution
/// time — this is additive, not a bypass. No-op if `sync_running` is already held
/// by another drain — the jobs are already persisted and will be picked up by that
/// drain or the next cycle. Called from `run_delete_flush_worker`, off the CfAPI
/// callback thread.
fn flush_pending_deletes(state: &crate::state::AppState, batch: Vec<PendingDelete>) {
    if batch.is_empty() {
        return;
    }
    let entries: Vec<(String, bool)> =
        batch.iter().map(|p| (p.rel.clone(), p.is_dir)).collect();
    let roots = minimal_delete_roots(&entries);

    for (rel, is_dir) in &roots {
        // DBSYNC-65 (Slice 1): capture the remote rev at enqueue time (files only —
        // folders aren't keyed in `remote_file_index`) so a later drain can detect
        // whether the remote copy changed since this delete was observed. A lookup
        // failure here must not abort the delete enqueue; fall back to `None`.
        let parent_rev = if *is_dir {
            None
        } else {
            match state.db.get_remote_file(rel) {
                Ok(row) => row.map(|r| r.rev),
                Err(e) => {
                    tracing::warn!(rel = %rel, error = %e, "cfapi event-delete: parent rev lookup failed; enqueuing without it");
                    None
                }
            }
        };
        if let Err(e) = state.db.enqueue_delete_job(rel, parent_rev.as_deref()) {
            tracing::warn!(rel = %rel, error = %e, "cfapi event-delete: enqueue failed");
        }
    }

    for entry in &batch {
        let untrack = if entry.is_dir {
            state.db.remove_known_folder(&entry.rel)
        } else {
            state.db.remove_local_file(&entry.rel)
        };
        if let Err(e) = untrack {
            tracing::warn!(rel = %entry.rel, is_dir = entry.is_dir, error = %e, "cfapi event-delete: untrack failed (job still enqueued)");
        }
    }

    tracing::info!(
        events = batch.len(),
        roots = roots.len(),
        "cfapi event-delete: coalesced burst flushed into recursive delete_v2 job(s)"
    );

    use std::sync::atomic::Ordering;
    if state
        .sync_running
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        // Tray tooltip refresh is UI-only (not correctness-load-bearing here —
        // the enqueues above already persisted the jobs). Gated out of `cfg(test)`
        // for the SAME reason as the notification plugin in `lib.rs`: taking
        // `Some(on_notify_delete_completion)`'s address for the CfAPI callback
        // table (above) makes this call chain statically reachable in the
        // `cargo test --lib` harness binary — which, unlike the real shipped
        // `.exe`, has no embedded manifest requesting ComCtl32 v6 — and the
        // `tray-icon`/`muda` Windows backend statically imports
        // `TaskDialogIndirect` (a ComCtl32 v6-only export), so the test binary
        // fails to LOAD at all (STATUS_ENTRYPOINT_NOT_FOUND) rather than merely
        // failing a test. The shipped app is unaffected (real manifest, and this
        // path already runs on a background thread there regardless).
        #[cfg(not(test))]
        crate::auth_session::refresh_tray_tooltip(state);
        crate::sync_pipeline::drain_sync_queue(state);
        state.sync_running.store(false, Ordering::Release);
        #[cfg(not(test))]
        crate::auth_session::refresh_tray_tooltip(state);
    }
}

/// Route a CONFIRMED user delete (from `on_notify_delete_completion`) or a
/// rename-treated-as-delete (from `on_notify_rename_completion`) into the sync
/// engine. DBSYNC-66 Slice 2: rather than enqueueing immediately, this now BUFFERS
/// the event (`push_pending_delete`) so a burst of per-descendant events from one
/// user folder-delete coalesces into a handful of recursive `delete_v2` jobs
/// instead of one job per descendant — see `run_delete_flush_worker` /
/// `flush_pending_deletes`. Still runs OFF the CfAPI callback thread (never touch
/// the DB/network from inside the `extern "system"` ABI).
fn enqueue_remote_delete_async(rel: String, is_dir: bool) {
    std::thread::spawn(move || {
        push_pending_delete(rel, is_dir);
    });
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
/// otherwise) — callers that are replacing a real file should MOVE it aside first
/// (see `cloudsc_ops::dehydrate_file_atomic`, DBSYNC-64) rather than deleting it, so
/// a failed placeholder create never loses data. Returns whether the placeholder
/// was created.
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

/// Hydrate every dehydrated (online-only) file under directory `abs` — our
/// "Synchroniser sur le disque" on a cloud-only folder (DBSYNC-59). Walks the subtree
/// and pulls each dehydrated placeholder on device via [`hydrate_placeholder`]
/// (which also reconciles its column state). Non-dehydrated files are left untouched.
/// Returns the number of files hydrated.
pub(crate) fn hydrate_folder(abs: &Path) -> usize {
    let mut n = 0usize;
    for entry in walkdir::WalkDir::new(abs).into_iter().flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        // Stat only — is_dehydrated_placeholder uses symlink_metadata, no open/recall.
        if crate::path_util::is_dehydrated_placeholder(entry.path()) && hydrate_placeholder(entry.path()) {
            n += 1;
        }
    }
    n
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
/// v3 (DBSYNC-62): re-register with package identity so WinRT populates the sync
/// root's `AUMID`, linking it to the package that declares the branded
/// CloudFilesContextMenus verbs (a pre-identity registration lacked it). The
/// re-register's placeholder eviction is suppressed by the DBSYNC-63 grace guard.
const REG_POLICY_VERSION: u32 = 3;
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
    // Start the delete-suppression grace window NOW: the Unregister below evicts the
    // dehydrated placeholders from disk, and that mass disappearance must never be read
    // as user deletes (DBSYNC-62).
    mark_registration_activity();
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
    // Unregister evicts placeholders from disk — open the grace window so the resulting
    // disappearances aren't propagated as Dropbox deletes (DBSYNC-62).
    mark_registration_activity();
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

#[cfg(test)]
mod tests {
    use super::*;

    // DBSYNC-64 part 2: `classify_rename_target` is the only pure (I/O-free) piece
    // of the NOTIFY_RENAME_COMPLETION routing — the rest needs a live sync root /
    // CfAPI connection and is exercised by manual live validation instead.

    #[test]
    fn classify_rename_target_within_root_returns_new_rel() {
        let root = Path::new(r"C:\Users\me\Dropbox");
        let target = Path::new(r"C:\Users\me\Dropbox\Docs\renamed.txt");
        assert_eq!(
            classify_rename_target(root, target),
            Some("Docs/renamed.txt".to_string())
        );
    }

    #[test]
    fn classify_rename_target_at_root_returns_bare_name() {
        let root = Path::new(r"C:\Users\me\Dropbox");
        let target = Path::new(r"C:\Users\me\Dropbox\renamed.txt");
        assert_eq!(
            classify_rename_target(root, target),
            Some("renamed.txt".to_string())
        );
    }

    #[test]
    fn classify_rename_target_outside_root_returns_none() {
        let root = Path::new(r"C:\Users\me\Dropbox");
        let target = Path::new(r"C:\Users\me\Desktop\moved.txt");
        assert_eq!(classify_rename_target(root, target), None);
    }

    #[test]
    fn classify_rename_target_sibling_directory_returns_none() {
        // Same parent prefix as a string, but NOT actually under `root` as a path
        // component — must not be misclassified as "within root".
        let root = Path::new(r"C:\Users\me\Dropbox");
        let target = Path::new(r"C:\Users\me\DropboxOther\moved.txt");
        assert_eq!(classify_rename_target(root, target), None);
    }

    // DBSYNC-64 CTO review H2: the completion decision built on top of
    // `classify_rename_target` — within-root → no delete; outside-root → delete.

    #[test]
    fn rename_completion_should_delete_source_within_root_is_false() {
        // classify_rename_target's Some(rel) case: target is still under the sync
        // root — a move within it must NOT delete the source this increment.
        assert!(!rename_completion_should_delete_source(&Some(
            "Docs/renamed.txt".to_string()
        )));
    }

    #[test]
    fn rename_completion_should_delete_source_outside_root_is_true() {
        // classify_rename_target's None case: target left the sync root entirely —
        // nothing is orphaned, so deleting the source is safe.
        assert!(rename_completion_should_delete_source(&None));
    }

    // DBSYNC-64 CTO review H1: the shared path-resolution helper used for BOTH the
    // rename target (`full_local_path`) and the rename-completion source
    // (`RenameCompletion.SourcePath`) — both are drive-less CfAPI paths.

    #[test]
    fn full_path_from_joins_volume_and_normalized_path() {
        let vol = to_wide("C:");
        let norm = to_wide(r"\Users\me\Dropbox\file.txt");
        let joined = unsafe { full_path_from(PCWSTR(vol.as_ptr()), PCWSTR(norm.as_ptr())) };
        assert_eq!(joined, Some(PathBuf::from(r"C:\Users\me\Dropbox\file.txt")));
    }

    #[test]
    fn full_path_from_empty_normalized_is_none() {
        let vol = to_wide("C:");
        let norm = to_wide("");
        let joined = unsafe { full_path_from(PCWSTR(vol.as_ptr()), PCWSTR(norm.as_ptr())) };
        assert_eq!(joined, None);
    }

    // DBSYNC-66 Slice 1: `materialization_grace_contains` is the pure core of the
    // subtree-scoped materialization grace window — `now`/`window` are injected so
    // these tests don't touch the wall clock (like `is_mass_deletion`).

    #[test]
    fn materialization_grace_contains_true_within_window_under_armed_prefix() {
        let base = Instant::now();
        let mut entries = HashMap::new();
        entries.insert("Photos".to_string(), base);
        let now = base.checked_add(Duration::from_secs(10)).unwrap();
        assert!(materialization_grace_contains(
            &entries,
            "Photos/img.jpg",
            now,
            Duration::from_secs(120)
        ));
    }

    #[test]
    fn materialization_grace_contains_false_when_expired() {
        let base = Instant::now();
        let mut entries = HashMap::new();
        entries.insert("Photos".to_string(), base);
        let now = base.checked_add(Duration::from_secs(130)).unwrap();
        assert!(!materialization_grace_contains(
            &entries,
            "Photos/img.jpg",
            now,
            Duration::from_secs(120)
        ));
    }

    #[test]
    fn materialization_grace_contains_false_for_unrelated_subtree() {
        let base = Instant::now();
        let mut entries = HashMap::new();
        entries.insert("Photos".to_string(), base);
        let now = base.checked_add(Duration::from_secs(10)).unwrap();
        assert!(!materialization_grace_contains(
            &entries,
            "Docs/report.pdf",
            now,
            Duration::from_secs(120)
        ));
    }

    #[test]
    fn materialization_grace_contains_boundary_safe() {
        let base = Instant::now();
        let mut entries = HashMap::new();
        entries.insert("foo/bar".to_string(), base);
        let now = base.checked_add(Duration::from_secs(10)).unwrap();
        assert!(!materialization_grace_contains(
            &entries,
            "foo/barbaz",
            now,
            Duration::from_secs(120)
        ));
    }

    #[test]
    fn materialization_grace_contains_exact_rel_match() {
        let base = Instant::now();
        let mut entries = HashMap::new();
        entries.insert("top.txt".to_string(), base);
        let now = base.checked_add(Duration::from_secs(10)).unwrap();
        assert!(materialization_grace_contains(
            &entries,
            "top.txt",
            now,
            Duration::from_secs(120)
        ));
    }

    // DBSYNC-66 Slice 2: `minimal_delete_roots` is the pure core of coalescing a
    // NOTIFY_DELETE burst (one event per descendant) into the minimal set of
    // top-level roots that actually need a `delete_v2` job (Dropbox's `delete_v2`
    // on a folder is already recursive, so a root's job covers every descendant
    // under it that also appears in the same burst).

    fn pd(rel: &str, is_dir: bool) -> (String, bool) {
        (rel.to_string(), is_dir)
    }

    #[test]
    fn minimal_delete_roots_empty_input_is_empty() {
        let batch: Vec<(String, bool)> = Vec::new();
        assert_eq!(minimal_delete_roots(&batch), Vec::new());
    }

    #[test]
    fn minimal_delete_roots_deep_tree_collapses_to_top_folder() {
        // Deleting "Photos" (with subfolders and files) fires one event per
        // descendant + the folder itself — only "Photos" should survive.
        let batch = vec![
            pd("Photos", true),
            pd("Photos/2024", true),
            pd("Photos/2024/img1.jpg", false),
            pd("Photos/2024/img2.jpg", false),
            pd("Photos/2025", true),
            pd("Photos/2025/img3.jpg", false),
        ];
        assert_eq!(minimal_delete_roots(&batch), vec![pd("Photos", true)]);
    }

    #[test]
    fn minimal_delete_roots_keeps_both_sibling_folders() {
        let batch = vec![
            pd("Photos", true),
            pd("Photos/img.jpg", false),
            pd("Docs", true),
            pd("Docs/report.pdf", false),
        ];
        let mut roots = minimal_delete_roots(&batch);
        roots.sort();
        let mut expected = vec![pd("Docs", true), pd("Photos", true)];
        expected.sort();
        assert_eq!(roots, expected);
    }

    #[test]
    fn minimal_delete_roots_boundary_safe_not_treated_as_ancestor() {
        // "foo/bar" must NOT be treated as an ancestor of "foo/barbaz" — only an
        // exact `/`-separated descendant counts (same boundary rule as
        // `materialization_grace_contains`). Both are roots.
        let batch = vec![pd("foo/bar", false), pd("foo/barbaz", false)];
        let mut roots = minimal_delete_roots(&batch);
        roots.sort();
        let mut expected = batch.clone();
        expected.sort();
        assert_eq!(roots, expected);
    }

    #[test]
    fn minimal_delete_roots_file_with_parent_folder_in_batch_keeps_only_parent() {
        let batch = vec![pd("Photos", true), pd("Photos/img.jpg", false)];
        assert_eq!(minimal_delete_roots(&batch), vec![pd("Photos", true)]);
    }

    #[test]
    fn minimal_delete_roots_single_file_no_folder_is_its_own_root() {
        let batch = vec![pd("top.txt", false)];
        assert_eq!(minimal_delete_roots(&batch), vec![pd("top.txt", false)]);
    }
}
