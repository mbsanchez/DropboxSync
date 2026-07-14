//! Menu labels localized to the system UI language (English fallback).
//!
//! NOTE: mirrors `src-tauri/src/windows_file_assoc.rs::system_ui_language` /
//! `label_sync_to_disk`. Kept duplicated because this DLL is a separate crate;
//! keep the two in sync when adding languages.

use winreg::enums::HKEY_CURRENT_USER;
use winreg::RegKey;

/// The 2-letter Windows UI language (e.g. `fr-FR` -> `fr`), defaulting to `en`.
pub fn system_ui_language() -> String {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Control Panel\International")
        .and_then(|k| k.get_value::<String, _>("LocaleName"))
        .ok()
        .and_then(|s| s.split(['-', '_']).next().map(|p| p.to_ascii_lowercase()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "en".to_string())
}

/// "Sync a cloud-only `.cloudsc` back to disk" (hydrate).
pub fn label_sync_to_disk(lang: &str) -> &'static str {
    match lang {
        "fr" => "Synchroniser sur le disque",
        "es" => "Sincronizar al disco",
        "de" => "Auf Datentr\u{00e4}ger synchronisieren",
        "it" => "Sincronizza su disco",
        "pt" => "Sincronizar no disco",
        _ => "Sync to disk",
    }
}

/// "Free up space by replacing the local copy with a placeholder" (dehydrate).
pub fn label_free_up_space(lang: &str) -> &'static str {
    match lang {
        "fr" => "Désynchroniser",
        "es" => "Liberar espacio",
        "de" => "Speicherplatz freigeben",
        "it" => "Libera spazio",
        "pt" => "Liberar espaço",
        _ => "Free up space",
    }
}

/// "Copy a Dropbox shared link for this item to the clipboard."
pub fn label_copy_link(lang: &str) -> &'static str {
    match lang {
        "fr" => "Copier le lien Dropbox",
        "es" => "Copiar enlace de Dropbox",
        "de" => "Dropbox-Link kopieren",
        "it" => "Copia link di Dropbox",
        "pt" => "Copiar link do Dropbox",
        _ => "Copy Dropbox link",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_localized_with_english_fallback() {
        assert_eq!(label_free_up_space("fr"), "Désynchroniser");
        assert_eq!(label_free_up_space("es"), "Liberar espacio");
        assert_eq!(label_free_up_space("en"), "Free up space");
        assert_eq!(label_free_up_space("xx"), "Free up space");
        assert_eq!(label_sync_to_disk("fr"), "Synchroniser sur le disque");
        assert_eq!(label_sync_to_disk("zz"), "Sync to disk");
        assert_eq!(label_copy_link("fr"), "Copier le lien Dropbox");
        assert_eq!(label_copy_link("es"), "Copiar enlace de Dropbox");
        assert_eq!(label_copy_link("xx"), "Copy Dropbox link");
    }
}
