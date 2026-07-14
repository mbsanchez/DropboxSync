//! Per-user (HKCU) registration of the COM server + the `ExplorerCommandHandler`
//! hookup on `*` (all files) and `Directory` (folders). No elevation required.
//!
//! In production the app writes these same keys on startup (see
//! `src-tauri/src/windows_shell_menu.rs`); `DllRegisterServer`/`regsvr32` here is
//! the standalone dev/test path. Both MUST use the same CLSID string.

use std::io;
use std::path::{Path, PathBuf};

use winreg::enums::HKEY_CURRENT_USER;
use winreg::RegKey;

/// String form of `CLSID_DROPBOXSYNC_COMMAND`.
pub const CLSID_STR: &str = "{FBF4F890-5407-47BF-BE25-F5B2595FA839}";

/// The `shell\<verb>` subkey name carrying `ExplorerCommandHandler`.
const VERB: &str = "DropboxSync";
/// Classes the handler attaches to.
const TARGET_CLASSES: [&str; 2] = ["*", "Directory"];
/// App-written config the DLL reads to find the exe to launch.
const SHELLEXT_KEY: &str = r"Software\DropboxSyncDesktop\ShellExt";

/// Write the COM server + handler hookup under HKCU.
pub fn register_hkcu(dll_path: &Path) -> io::Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);

    // 1) COM server.
    let (clsid_key, _) =
        hkcu.create_subkey(format!(r"Software\Classes\CLSID\{CLSID_STR}"))?;
    clsid_key.set_value("", &"DropboxSync Explorer Command")?;
    let (inproc, _) =
        hkcu.create_subkey(format!(r"Software\Classes\CLSID\{CLSID_STR}\InprocServer32"))?;
    inproc.set_value("", &dll_path.to_string_lossy().as_ref())?;
    inproc.set_value("ThreadingModel", &"Apartment")?;

    // 2) Handler hookup on each target class.
    for class in TARGET_CLASSES {
        let (k, _) =
            hkcu.create_subkey(format!(r"Software\Classes\{class}\shell\{VERB}"))?;
        // MUIVerb is a fallback label if the handler can't load; the live menu
        // text comes from IExplorerCommand::GetTitle.
        k.set_value("MUIVerb", &"DropboxSync")?;
        k.set_value("ExplorerCommandHandler", &CLSID_STR)?;
    }

    // 3) Record the DLL path; record an exe path if we can find one next to it
    //    (the app overwrites this with its real current_exe() at startup).
    let (cfg, _) = hkcu.create_subkey(SHELLEXT_KEY)?;
    cfg.set_value("DllPath", &dll_path.to_string_lossy().as_ref())?;
    if let Some(exe) = exe_next_to_dll(dll_path) {
        cfg.set_value("ExePath", &exe.to_string_lossy().as_ref())?;
    }

    Ok(())
}

/// Remove everything `register_hkcu` wrote.
pub fn unregister_hkcu() -> io::Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let _ = hkcu.delete_subkey_all(format!(r"Software\Classes\CLSID\{CLSID_STR}"));
    for class in TARGET_CLASSES {
        let _ = hkcu.delete_subkey_all(format!(r"Software\Classes\{class}\shell\{VERB}"));
    }
    let _ = hkcu.delete_subkey_all(SHELLEXT_KEY);
    Ok(())
}

/// Resolve the app exe to launch: prefer the app-written `ExePath`, else an exe
/// sitting next to the DLL.
pub fn resolve_app_exe(dll_path: &Path) -> Option<PathBuf> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    if let Ok(cfg) = hkcu.open_subkey(SHELLEXT_KEY) {
        if let Ok(exe) = cfg.get_value::<String, _>("ExePath") {
            let p = PathBuf::from(exe);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    exe_next_to_dll(dll_path)
}

/// Look for the app exe in the DLL's directory (bundle name first, cargo name next).
fn exe_next_to_dll(dll_path: &Path) -> Option<PathBuf> {
    let dir = dll_path.parent()?;
    for name in ["DropboxSyncDesktop.exe", "dropbox_sync_desktop.exe"] {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}
