//! Per-user (HKCU) registration of the COM server + the context-menu verb.
//!
//! Registered on **`AllFilesystemObjects`** (files, folders and drives) as an
//! `IExplorerCommand` verb. On Windows 11 this surfaces under "Show more
//! options" (the classic menu); the compact menu needs an MSIX sparse package
//! (out of scope). The verb REQUIRES a `command` subkey with `DelegateExecute`
//! and `MultiSelectModel` — a bare `shell\<verb>\ExplorerCommandHandler` (no
//! `command`) is culled by the Win11 shell and never appears.
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

/// The `shell\<verb>` subkey name.
const VERB: &str = "DropboxSync";
/// Class the verb attaches to: all files, folders and drives.
const TARGET_CLASS: &str = "AllFilesystemObjects";
/// App-written config the DLL reads to find the exe to launch.
const SHELLEXT_KEY: &str = r"Software\DropboxSyncDesktop\ShellExt";

/// Write the COM server + verb under HKCU.
pub fn register_hkcu(dll_path: &Path) -> io::Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);

    // 1) COM server.
    let (clsid_key, _) = hkcu.create_subkey(format!(r"Software\Classes\CLSID\{CLSID_STR}"))?;
    clsid_key.set_value("", &"DropboxSync Explorer Command")?;
    let (inproc, _) =
        hkcu.create_subkey(format!(r"Software\Classes\CLSID\{CLSID_STR}\InprocServer32"))?;
    inproc.set_value("", &dll_path.to_string_lossy().as_ref())?;
    inproc.set_value("ThreadingModel", &"Apartment")?;

    // 2) The context-menu verb. Use the app icon if we can find the exe next to
    //    the DLL (the app overwrites this with its real exe icon at startup).
    let icon = exe_next_to_dll(dll_path).map(|e| format!("{},0", e.to_string_lossy()));
    write_verb(&hkcu, icon.as_deref())?;

    // 3) Record the DLL + exe paths for the handler to launch.
    let (cfg, _) = hkcu.create_subkey(SHELLEXT_KEY)?;
    cfg.set_value("DllPath", &dll_path.to_string_lossy().as_ref())?;
    if let Some(exe) = exe_next_to_dll(dll_path) {
        cfg.set_value("ExePath", &exe.to_string_lossy().as_ref())?;
    }

    Ok(())
}

/// Write the `AllFilesystemObjects\shell\DropboxSync` verb (+ its `command`
/// subkey). Shared shape; the app mirrors it in `windows_shell_menu.rs`.
fn write_verb(hkcu: &RegKey, icon: Option<&str>) -> io::Result<()> {
    let (k, _) = hkcu.create_subkey(format!(r"Software\Classes\{TARGET_CLASS}\shell\{VERB}"))?;
    k.set_value("MUIVerb", &"DropboxSync")?;
    // Show the verb once for a multi-selection rather than per-item.
    k.set_value("MultiSelectModel", &"Player")?;
    if let Some(icon) = icon {
        k.set_value("Icon", &icon)?;
    }
    k.set_value("ExplorerCommandHandler", &CLSID_STR)?;

    // The `command` subkey is required for the verb to surface on Win11; the
    // parent never executes directly (it has subcommands), so DelegateExecute is
    // effectively inert but must be present.
    let (cmd, _) =
        hkcu.create_subkey(format!(r"Software\Classes\{TARGET_CLASS}\shell\{VERB}\command"))?;
    cmd.set_value("", &"")?;
    cmd.set_value("DelegateExecute", &CLSID_STR)?;
    Ok(())
}

/// Remove everything `register_hkcu` wrote.
pub fn unregister_hkcu() -> io::Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let _ = hkcu.delete_subkey_all(format!(r"Software\Classes\CLSID\{CLSID_STR}"));
    let _ = hkcu.delete_subkey_all(format!(r"Software\Classes\{TARGET_CLASS}\shell\{VERB}"));
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
