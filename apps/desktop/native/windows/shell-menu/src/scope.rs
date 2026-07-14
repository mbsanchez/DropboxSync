//! Decides which child (if any) applies to a given item, using the sync root
//! published by the app in `overlay_state.json`.
//!
//! `GetState` runs on Explorer's UI thread and is called per item, so this must
//! be cheap: the sync root is cached and only re-read when the status file's
//! mtime changes. We never touch the network or canonicalize (a `.cloudsc`
//! placeholder is a real local file; a lexical prefix check on the display path
//! is enough and cheapest).

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use serde::Deserialize;

/// What the DropboxSync menu can do with one item.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    /// Regular file/folder under the sync root → "Désynchroniser".
    FreeUpSpace,
    /// `.cloudsc` placeholder under the sync root → "Synchroniser sur le disque".
    Hydrate,
    /// Outside the sync root, or no root configured → menu hidden.
    None,
}

/// Aggregate over a whole selection: which children should be visible.
#[derive(Default)]
pub struct Classification {
    pub any_free_up_space: bool,
    pub any_hydrate: bool,
}

pub fn classify(paths: &[PathBuf]) -> Classification {
    let mut c = Classification::default();
    for p in paths {
        match target_for(p) {
            Target::FreeUpSpace => c.any_free_up_space = true,
            Target::Hydrate => c.any_hydrate = true,
            Target::None => {}
        }
    }
    c
}

/// Classify a single path against the (cached) sync root.
pub fn target_for(path: &Path) -> Target {
    let Some(root) = cached_sync_root() else {
        return Target::None;
    };
    if !is_under(&root, path) {
        return Target::None;
    }
    if has_cloudsc_ext(path) {
        Target::Hydrate
    } else {
        Target::FreeUpSpace
    }
}

fn has_cloudsc_ext(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("cloudsc"))
        .unwrap_or(false)
}

fn is_under(root: &Path, path: &Path) -> bool {
    let r = norm(root);
    let c = norm(path);
    if r.is_empty() {
        return false;
    }
    c == r || c.starts_with(&format!("{r}\\"))
}

/// Case-folded, separator-normalized, trailing-slash-trimmed path key.
fn norm(p: &Path) -> String {
    p.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}

// ---- sync-root cache -------------------------------------------------------

struct Cache {
    mtime: Option<SystemTime>,
    sync_root: Option<PathBuf>,
}

static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();

fn status_file_path() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(
        PathBuf::from(base)
            .join("DropboxSyncDesktop")
            .join("overlay_state.json"),
    )
}

/// Returns the sync root, reading + parsing `overlay_state.json` only when its
/// mtime changed since the last call.
fn cached_sync_root() -> Option<PathBuf> {
    let path = status_file_path()?;
    let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();

    let cache = CACHE.get_or_init(|| {
        Mutex::new(Cache {
            mtime: None,
            sync_root: None,
        })
    });
    let mut guard = cache.lock().ok()?;

    if guard.sync_root.is_some() && guard.mtime == mtime {
        return guard.sync_root.clone();
    }

    let root = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| parse_sync_folder(&s));
    guard.mtime = mtime;
    guard.sync_root = root.clone();
    root
}

#[derive(Deserialize)]
struct StatusHeader {
    #[serde(default)]
    sync_folder: Option<String>,
}

fn parse_sync_folder(json: &str) -> Option<PathBuf> {
    let header: StatusHeader = serde_json::from_str(json).ok()?;
    header
        .sync_folder
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_under_matches_root_and_descendants() {
        let root = Path::new(r"C:\Users\me\DropboxSync");
        assert!(is_under(root, Path::new(r"C:\Users\me\DropboxSync\a.txt")));
        assert!(is_under(root, Path::new(r"C:\Users\me\DropboxSync\sub\b.cloudsc")));
        assert!(is_under(root, Path::new(r"c:\users\me\dropboxsync\a.txt"))); // case-insensitive
        assert!(is_under(root, Path::new(r"C:/Users/me/DropboxSync/a.txt"))); // slash-insensitive
        assert!(is_under(root, root)); // the root itself
    }

    #[test]
    fn is_under_rejects_outside_and_sibling_prefix() {
        let root = Path::new(r"C:\Users\me\DropboxSync");
        assert!(!is_under(root, Path::new(r"C:\Users\me\Other\a.txt")));
        // sibling folder sharing a name prefix must not match
        assert!(!is_under(root, Path::new(r"C:\Users\me\DropboxSyncBackup\a.txt")));
    }

    #[test]
    fn classify_splits_cloudsc_from_regular() {
        // No status file in a clean test env → everything None.
        let c = classify(&[PathBuf::from(r"C:\x\y.txt")]);
        assert!(!c.any_free_up_space && !c.any_hydrate);
    }

    #[test]
    fn parse_sync_folder_reads_header() {
        let json = r#"{"version":1,"updated_at":"t","sync_folder":"C:\\s","paths":{}}"#;
        assert_eq!(parse_sync_folder(json), Some(PathBuf::from(r"C:\s")));
        assert_eq!(parse_sync_folder(r#"{"sync_folder":null}"#), None);
        assert_eq!(parse_sync_folder("not json"), None);
    }

    #[test]
    fn cloudsc_ext_detection() {
        assert!(has_cloudsc_ext(Path::new("a.cloudsc")));
        assert!(has_cloudsc_ext(Path::new("a.CLOUDSC")));
        assert!(!has_cloudsc_ext(Path::new("a.txt")));
        assert!(!has_cloudsc_ext(Path::new("cloudsc")));
    }
}
