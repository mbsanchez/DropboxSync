//! DropboxSync Windows Explorer context-menu — an in-process COM server
//! implementing `IExplorerCommand` as a cascading **"DropboxSync"** parent with
//! two children:
//!   - **Désynchroniser** (free up space) — regular files/folders under the sync root
//!   - **Synchroniser sur le disque** (hydrate) — `.cloudsc` placeholders under the root
//!
//! Visibility is decided per item in [`command::DropboxSyncCommand::GetState`] by
//! reading the app's `overlay_state.json` (for the sync root) and the item's
//! extension. Selecting a child launches the app over the existing shell-action
//! channel: `<app.exe> --action=<free_up_space|hydrate> --path="<item>"`
//! (see `src-tauri/src/shell_actions.rs`).
//!
//! A single CLSID is registered on `*` (all files, incl. `.cloudsc`) and
//! `Directory` (folders). Registration is per-user (HKCU) — no elevation.
//! This DLL is built and registered separately from the app (see README.md).
#![cfg(windows)]

use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicU32, Ordering};

use windows::core::{Interface, GUID, HRESULT};
use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, E_FAIL, E_NOINTERFACE, S_FALSE, S_OK,
};
use windows::Win32::System::Com::IClassFactory;

mod command;
mod enumerator;
pub mod exeserver; // DBSYNC-62: out-of-proc CloudFilesContextMenus ExeServer
mod factory;
mod labels;
mod registry;
mod scope;
mod util;

/// Stable class id for the DropboxSync explorer command. NEVER change this once
/// shipped — the app-side registration (`windows_shell_menu.rs`) hard-codes the
/// same GUID string `{FBF4F890-5407-47BF-BE25-F5B2595FA839}`.
pub(crate) const CLSID_DROPBOXSYNC_COMMAND: GUID =
    GUID::from_u128(0xFBF4F890_5407_47BF_BE25_F5B2595FA839);

/// Live COM object count; DLL may unload only when this (and locks) reach zero.
pub(crate) static OBJECT_COUNT: AtomicU32 = AtomicU32::new(0);
/// `IClassFactory::LockServer` count.
pub(crate) static LOCK_COUNT: AtomicU32 = AtomicU32::new(0);

/// COM entry point: hand the shell our class factory for our CLSID.
///
/// # Safety
/// Called by the COM runtime. `rclsid` and `riid` must be valid `GUID` pointers
/// and `ppv` a valid `*mut *mut c_void` out-parameter (all guaranteed by COM).
#[no_mangle]
pub unsafe extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if ppv.is_null() {
        return E_NOINTERFACE;
    }
    *ppv = null_mut();
    if rclsid.is_null() || riid.is_null() {
        return E_NOINTERFACE;
    }
    if *rclsid != CLSID_DROPBOXSYNC_COMMAND {
        return CLASS_E_CLASSNOTAVAILABLE;
    }
    let factory: IClassFactory = factory::DropboxSyncCommandFactory::new().into();
    factory.query(riid, ppv)
}

/// The shell polls this before unloading the DLL.
#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    if OBJECT_COUNT.load(Ordering::SeqCst) == 0 && LOCK_COUNT.load(Ordering::SeqCst) == 0 {
        S_OK
    } else {
        S_FALSE
    }
}

/// `regsvr32 <dll>` — writes the HKCU keys (no elevation needed).
#[no_mangle]
pub extern "system" fn DllRegisterServer() -> HRESULT {
    match registry::register_hkcu(&util::current_module_path()) {
        Ok(()) => S_OK,
        Err(_) => E_FAIL,
    }
}

/// `regsvr32 /u <dll>` — removes the HKCU keys.
#[no_mangle]
pub extern "system" fn DllUnregisterServer() -> HRESULT {
    match registry::unregister_hkcu() {
        Ok(()) => S_OK,
        Err(_) => E_FAIL,
    }
}
