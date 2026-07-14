//! DBSYNC-41 (Windows): expose per-file sync status in Explorer's built-in cloud
//! "Status" column via the Cloud Files API (CfAPI), WITHOUT changing the `.cloudsc`
//! sidecar model or the DBSYNC-33 hydrate/dehydrate flow.
//!
//! - `CfRegisterSyncRoot` (Win32, unpackaged / no admin) with FULL + ALWAYS_FULL
//!   policy — files are always fully present (no on-demand), which suppresses
//!   CfAPI's own "Free up space" verb (so it doesn't clash with our COM menu) and
//!   means no fetch-data provider is needed.
//! - Each REAL (hydrated) file under the root is converted IN PLACE to a pinned
//!   in-sync placeholder (`CfConvertToPlaceholder`, no data movement / no download),
//!   and its in-sync state is driven from `overlay_state.json`: Synced → green
//!   check, syncing/out-of-sync → sync-pending.
//! - `.cloudsc` sidecars are left as plain files → blank status (by design).
//!
//! Every call fails soft: if CfAPI is unavailable (non-NTFS, older Windows, a
//! locked file, …) the affected step no-ops and the app is otherwise unaffected.
#![cfg(windows)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use windows::core::{GUID, PCWSTR};
use windows::Win32::Storage::CloudFilters::{
    CfCloseHandle, CfConvertToPlaceholder, CfOpenFileWithOplock, CfRegisterSyncRoot,
    CfSetInSyncState, CfSetPinState, CfUnregisterSyncRoot, CF_CONVERT_FLAG_MARK_IN_SYNC,
    CF_HARDLINK_POLICY_NONE, CF_HYDRATION_POLICY, CF_HYDRATION_POLICY_ALWAYS_FULL,
    CF_HYDRATION_POLICY_MODIFIER_NONE, CF_INSYNC_POLICY_NONE, CF_IN_SYNC_STATE_IN_SYNC,
    CF_IN_SYNC_STATE_NOT_IN_SYNC, CF_OPEN_FILE_FLAG_WRITE_ACCESS,
    CF_PLACEHOLDER_MANAGEMENT_POLICY_CONVERT_TO_UNRESTRICTED, CF_PIN_STATE_PINNED,
    CF_POPULATION_POLICY, CF_POPULATION_POLICY_ALWAYS_FULL, CF_POPULATION_POLICY_MODIFIER_NONE,
    CF_REGISTER_FLAG_UPDATE, CF_SET_IN_SYNC_FLAG_NONE, CF_SET_PIN_FLAG_NONE, CF_SYNC_POLICIES,
    CF_SYNC_REGISTRATION,
};

/// Stable provider id for the DropboxSync sync root. NEVER change once shipped.
const PROVIDER_ID: GUID = GUID::from_u128(0xEA067F54_0C87_4DC2_9F90_3A632C2AAF9C);

/// The sync-root folder we've registered, so we register only once per folder.
static REGISTERED: OnceLock<Mutex<Option<String>>> = OnceLock::new();
/// `rel → last-applied in-sync bool`, so we only touch a file when it changes.
static APPLIED: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Update Explorer's cloud Status column from the current per-file states.
/// `items` is `(rel, is_synced)` for every tracked local file (`rel` is
/// `/`-relative under `sync_folder`). Registers the sync root on first use / when
/// the folder changes. Called from `overlay_state::refresh_overlay_state_internal`
/// so the column shares the single `overlay_state.json` source of truth.
pub(crate) fn sync_placeholder_states(sync_folder: Option<&str>, items: &[(&str, bool)]) {
    let Some(folder) = sync_folder else {
        return;
    };
    if !ensure_registered(folder) {
        return; // registration failed (e.g. non-NTFS) → feature disabled, app unaffected
    }
    let root = Path::new(folder);
    let applied = APPLIED.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut applied) = applied.lock() else {
        return;
    };

    for (rel, is_synced) in items {
        if rel.ends_with(".cloudsc") {
            continue; // cloud-only placeholder → intentionally blank in the column
        }
        if matches!(applied.get(*rel), Some(prev) if prev == is_synced) {
            continue; // unchanged since we last applied it
        }
        let abs = root.join(rel);
        if !abs.is_file() {
            continue;
        }
        if apply_file_state(&abs, *is_synced) {
            applied.insert((*rel).to_string(), *is_synced);
        }
    }
}

/// Register the sync root once per folder (idempotent via `UPDATE`). Returns
/// whether a root is currently registered for `folder`.
fn ensure_registered(folder: &str) -> bool {
    let reg = REGISTERED.get_or_init(|| Mutex::new(None));
    let Ok(mut cur) = reg.lock() else {
        return false;
    };
    if cur.as_deref() == Some(folder) {
        return true;
    }
    if let Some(old) = cur.take() {
        let _ = unregister(&old);
        if let Some(applied) = APPLIED.get() {
            if let Ok(mut applied) = applied.lock() {
                applied.clear();
            }
        }
    }
    match register(folder) {
        Ok(()) => {
            tracing::info!(folder, "registered CfAPI sync root (Explorer status column)");
            *cur = Some(folder.to_string());
            true
        }
        Err(e) => {
            tracing::warn!(folder, error = %e, "CfAPI sync-root registration failed; status column disabled");
            false
        }
    }
}

fn register(folder: &str) -> windows::core::Result<()> {
    let path = to_wide(folder);
    let identity = to_wide(folder); // sync-root identity (must outlive the call)
    let name = to_wide("DropboxSync");
    let version = to_wide("1.0");

    let registration = CF_SYNC_REGISTRATION {
        StructSize: std::mem::size_of::<CF_SYNC_REGISTRATION>() as u32,
        ProviderName: PCWSTR(name.as_ptr()),
        ProviderVersion: PCWSTR(version.as_ptr()),
        SyncRootIdentity: identity.as_ptr() as *const core::ffi::c_void,
        SyncRootIdentityLength: (identity.len() * std::mem::size_of::<u16>()) as u32,
        ProviderId: PROVIDER_ID,
        ..Default::default()
    };
    // FULL + ALWAYS_FULL: files are always present, so no on-demand hydration verb
    // ("Free up space") and no fetch-data provider needed. CONVERT_TO_UNRESTRICTED
    // lets us convert placeholders without a connected provider.
    let policies = CF_SYNC_POLICIES {
        StructSize: std::mem::size_of::<CF_SYNC_POLICIES>() as u32,
        Hydration: CF_HYDRATION_POLICY {
            Primary: CF_HYDRATION_POLICY_ALWAYS_FULL,
            Modifier: CF_HYDRATION_POLICY_MODIFIER_NONE,
        },
        Population: CF_POPULATION_POLICY {
            Primary: CF_POPULATION_POLICY_ALWAYS_FULL,
            Modifier: CF_POPULATION_POLICY_MODIFIER_NONE,
        },
        InSync: CF_INSYNC_POLICY_NONE,
        HardLink: CF_HARDLINK_POLICY_NONE,
        PlaceholderManagement: CF_PLACEHOLDER_MANAGEMENT_POLICY_CONVERT_TO_UNRESTRICTED,
    };

    unsafe {
        CfRegisterSyncRoot(
            PCWSTR(path.as_ptr()),
            &registration,
            &policies,
            CF_REGISTER_FLAG_UPDATE,
        )
    }
}

fn unregister(folder: &str) -> windows::core::Result<()> {
    let path = to_wide(folder);
    unsafe { CfUnregisterSyncRoot(PCWSTR(path.as_ptr())) }
}

/// Convert `abs` in place to a pinned in-sync placeholder and set its state.
/// Returns true if the in-sync state was applied. Fails soft.
fn apply_file_state(abs: &Path, is_synced: bool) -> bool {
    let path = to_wide(&abs.to_string_lossy());
    unsafe {
        let handle = match CfOpenFileWithOplock(PCWSTR(path.as_ptr()), CF_OPEN_FILE_FLAG_WRITE_ACCESS)
        {
            Ok(h) => h,
            Err(e) => {
                tracing::debug!(path = %abs.display(), error = %e, "CfOpenFileWithOplock failed");
                return false;
            }
        };
        // Convert to a placeholder if it isn't one yet (an "already a placeholder"
        // error is expected and ignored). Keeps the file's bytes — no download.
        let _ = CfConvertToPlaceholder(handle, None, 0, CF_CONVERT_FLAG_MARK_IN_SYNC, None, None);
        // Pin so the platform never dehydrates it — our `.cloudsc` menu owns freeing space.
        let _ = CfSetPinState(handle, CF_PIN_STATE_PINNED, CF_SET_PIN_FLAG_NONE, None);
        let state = if is_synced {
            CF_IN_SYNC_STATE_IN_SYNC
        } else {
            CF_IN_SYNC_STATE_NOT_IN_SYNC
        };
        let applied = CfSetInSyncState(handle, state, CF_SET_IN_SYNC_FLAG_NONE, None).is_ok();
        CfCloseHandle(handle);
        applied
    }
}
