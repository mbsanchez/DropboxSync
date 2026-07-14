//! App-side registration for the COM `IExplorerCommand` context menu
//! (DBSYNC-33 Slice 2). The menu logic lives in the standalone cdylib
//! `dropbox_sync_shell_menu.dll` (`native/windows/shell-menu/`); this module
//! hooks it up under HKCU on startup so no `regsvr32`/installer step is needed.
//!
//! It is **gated on the DLL being present next to the exe**:
//!   - DLL found → register the COM handler + record the exe to launch, and
//!     remove the legacy `.cloudsc` `ExtendedSubCommandsKey` flyout (written by
//!     `windows_file_assoc.rs`) so `.cloudsc` files don't show two menus.
//!   - DLL missing → no-op, leaving the legacy flyout intact (no regression).
//!
//! The key layout MUST match the DLL's `registry.rs`; both use the same CLSID.

use std::io;
use std::path::{Path, PathBuf};

use winreg::enums::HKEY_CURRENT_USER;
use winreg::RegKey;

use crate::windows_file_assoc::{CLOUDSC_MENU_PROG_ID, PROG_ID};

/// Must equal `dropbox_sync_shell_menu::CLSID_DROPBOXSYNC_COMMAND` /
/// `registry::CLSID_STR`. Never change once shipped.
const CLSID_STR: &str = "{FBF4F890-5407-47BF-BE25-F5B2595FA839}";
/// File name the cdylib builds to.
const DLL_NAME: &str = "dropbox_sync_shell_menu.dll";
/// `shell\<verb>` subkey carrying `ExplorerCommandHandler`.
const VERB: &str = "DropboxSync";
/// Classes the handler attaches to: all files (incl. `.cloudsc`) and folders.
const TARGET_CLASSES: [&str; 2] = ["*", "Directory"];
/// App-written config the DLL reads to find the exe to launch.
const SHELLEXT_KEY: &str = r"Software\DropboxSyncDesktop\ShellExt";

/// Register the shell menu if its DLL is deployed next to the app; otherwise
/// leave the legacy `.cloudsc` flyout in place. Logs and swallows errors.
pub fn sync_shell_menu_registration() {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, "shell menu: cannot resolve current_exe");
            return;
        }
    };
    let Some(dll) = dll_next_to_exe(&exe) else {
        tracing::info!(
            "shell menu: {DLL_NAME} not deployed next to exe; keeping legacy .cloudsc flyout"
        );
        return;
    };

    match register_com_handler(&dll, &exe) {
        Ok(()) => {
            // COM handler now serves `.cloudsc` via `*`; drop the legacy flyout
            // so those files don't get a duplicate "DropboxSync" menu.
            remove_legacy_flyout();
            tracing::info!(dll = %dll.display(), "registered COM shell context menu (HKCU)");
        }
        Err(e) => tracing::error!(error = %e, "shell menu: COM handler registration failed"),
    }
}

fn dll_next_to_exe(exe: &Path) -> Option<PathBuf> {
    let candidate = exe.parent()?.join(DLL_NAME);
    candidate.is_file().then_some(candidate)
}

fn register_com_handler(dll: &Path, exe: &Path) -> io::Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);

    // 1) COM server (in-proc, apartment-threaded).
    let (clsid_key, _) = hkcu.create_subkey(format!(r"Software\Classes\CLSID\{CLSID_STR}"))?;
    clsid_key.set_value("", &"DropboxSync Explorer Command")?;
    let (inproc, _) =
        hkcu.create_subkey(format!(r"Software\Classes\CLSID\{CLSID_STR}\InprocServer32"))?;
    inproc.set_value("", &dll.to_string_lossy().as_ref())?;
    inproc.set_value("ThreadingModel", &"Apartment")?;

    // 2) Handler hookup on each target class.
    for class in TARGET_CLASSES {
        let (k, _) = hkcu.create_subkey(format!(r"Software\Classes\{class}\shell\{VERB}"))?;
        k.set_value("MUIVerb", &"DropboxSync")?;
        k.set_value("ExplorerCommandHandler", &CLSID_STR)?;
    }

    // 3) Record the exe the DLL should launch, and the DLL path.
    let (cfg, _) = hkcu.create_subkey(SHELLEXT_KEY)?;
    cfg.set_value("ExePath", &exe.to_string_lossy().as_ref())?;
    cfg.set_value("DllPath", &dll.to_string_lossy().as_ref())?;
    Ok(())
}

/// Remove the legacy `.cloudsc` `ExtendedSubCommandsKey` flyout so it doesn't
/// duplicate the COM menu. Best-effort.
fn remove_legacy_flyout() {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let _ = hkcu.delete_subkey_all(format!(r"Software\Classes\{PROG_ID}\shell\{VERB}"));
    let _ = hkcu.delete_subkey_all(format!(r"Software\Classes\{CLOUDSC_MENU_PROG_ID}"));
}
