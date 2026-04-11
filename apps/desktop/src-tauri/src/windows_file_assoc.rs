//! Per-user `.cloudsc` registration under `HKCU\Software\Classes` (no elevation).
//! Lets you double-click `.cloudsc` when running a portable `.exe` without NSIS/MSI.
//!
//! Disable with env `DROPBOXSYNC_SKIP_WINDOWS_FILE_ASSOC=1` if needed.

use std::io;
use std::path::Path;

use winreg::enums::*;
use winreg::RegKey;

/// ProgID for HKCU classes; distinct from any machine-wide installer ProgID.
const PROG_ID: &str = "DropboxSyncDesktop.CloudscPortable.1";

/// Registers `.cloudsc` to launch the current executable with `"%1"`.
pub fn register_user_cloudsc_association() -> io::Result<()> {
    let exe = std::env::current_exe()?;
    register_user_cloudsc_association_for_exe(&exe)
}

fn register_user_cloudsc_association_for_exe(exe: &Path) -> io::Result<()> {
    let exe_str = exe.to_string_lossy();
    let open_cmd = format!("\"{}\" \"%1\"", exe_str);
    let icon_ref = format!("\"{}\",0", exe_str);

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);

    let (dot_key, _) = hkcu.create_subkey(r"Software\Classes\.cloudsc")?;
    dot_key.set_value("", &PROG_ID)?;

    let (prog_key, _) = hkcu.create_subkey(format!(r"Software\Classes\{PROG_ID}"))?;
    prog_key.set_value(
        "",
        &"Cloudsc placeholder (Dropbox Sync Desktop)",
    )?;

    let (icon_key, _) =
        hkcu.create_subkey(format!(r"Software\Classes\{PROG_ID}\DefaultIcon"))?;
    icon_key.set_value("", &icon_ref)?;

    let (cmd_key, _) = hkcu.create_subkey(format!(
        r"Software\Classes\{PROG_ID}\shell\open\command"
    ))?;
    cmd_key.set_value("", &open_cmd)?;

    Ok(())
}
