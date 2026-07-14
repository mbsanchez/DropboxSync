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

    register_cloudsc_context_menu(&hkcu, &exe_str, &icon_ref)?;

    Ok(())
}

/// Sub-menu ProgID that holds the cascading "DropboxSync" children (referenced by
/// the parent verb's `ExtendedSubCommandsKey`).
const CLOUDSC_MENU_PROG_ID: &str = "DropboxSyncDesktop.CloudscMenu";

/// DBSYNC-51 (Slice 2): a cascading **"DropboxSync"** context-menu on `.cloudsc`
/// whose children invoke the running app over the shell-action channel
/// (`--action/--path`). Labels are written in the system UI language (no resource
/// DLL — the app localizes at registration). More children (Desincronizar,
/// Créer un lien) are appended by their own tickets (DBSYNC-33 / DBSYNC-52).
fn register_cloudsc_context_menu(
    hkcu: &RegKey,
    exe_str: &str,
    icon_ref: &str,
) -> io::Result<()> {
    // Remove the previous flat verb from earlier builds, if present.
    let _ = hkcu.delete_subkey_all(format!(
        r"Software\Classes\{PROG_ID}\shell\dropboxsync_hydrate"
    ));

    let lang = system_ui_language();

    // Parent flyout "DropboxSync" (a brand name; kept untranslated).
    let (parent, _) =
        hkcu.create_subkey(format!(r"Software\Classes\{PROG_ID}\shell\DropboxSync"))?;
    parent.set_value("MUIVerb", &"DropboxSync")?;
    parent.set_value("Icon", &icon_ref)?;
    parent.set_value("ExtendedSubCommandsKey", &CLOUDSC_MENU_PROG_ID)?;

    // Child: hydrate ("Sync to disk"), localized to the system language.
    let hydrate_cmd = format!("\"{exe_str}\" --action=hydrate --path=\"%1\"");
    let (child, _) = hkcu.create_subkey(format!(
        r"Software\Classes\{CLOUDSC_MENU_PROG_ID}\shell\10hydrate"
    ))?;
    child.set_value("MUIVerb", &label_sync_to_disk(&lang))?;
    child.set_value("Icon", &icon_ref)?;
    let (child_cmd, _) = hkcu.create_subkey(format!(
        r"Software\Classes\{CLOUDSC_MENU_PROG_ID}\shell\10hydrate\command"
    ))?;
    child_cmd.set_value("", &hydrate_cmd)?;

    Ok(())
}

/// The 2-letter Windows UI language (e.g. `fr-FR` -> `fr`), defaulting to `en`.
fn system_ui_language() -> String {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Control Panel\International")
        .and_then(|k| k.get_value::<String, _>("LocaleName"))
        .ok()
        .and_then(|s| {
            s.split(['-', '_'])
                .next()
                .map(|p| p.to_ascii_lowercase())
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "en".to_string())
}

/// Localized label for the hydrate ("sync a cloud-only file back to disk") verb.
fn label_sync_to_disk(lang: &str) -> &'static str {
    match lang {
        "fr" => "Synchroniser sur le disque",
        "es" => "Sincronizar al disco",
        "de" => "Auf Datentr\u{00e4}ger synchronisieren",
        "it" => "Sincronizza su disco",
        "pt" => "Sincronizar no disco",
        _ => "Sync to disk",
    }
}

#[cfg(test)]
mod tests {
    use super::label_sync_to_disk;

    #[test]
    fn hydrate_label_is_localized_with_english_fallback() {
        assert_eq!(label_sync_to_disk("fr"), "Synchroniser sur le disque");
        assert_eq!(label_sync_to_disk("es"), "Sincronizar al disco");
        assert_eq!(label_sync_to_disk("en"), "Sync to disk");
        assert_eq!(label_sync_to_disk("xx"), "Sync to disk");
    }
}
