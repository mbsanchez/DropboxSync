use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

use chrono::Utc;
use sha2::{Digest, Sha256};

use crate::error::{AppError, AppResult};

pub(crate) fn should_ignore_local_path(relative: &str) -> bool {
    let p = relative.replace('\\', "/");
    p == ".DS_Store"
        || p.ends_with("/.DS_Store")
        || p.starts_with("._")
        || p.contains("/._")
        || p == "Thumbs.db"
        || p.ends_with("/Thumbs.db")
}

/// True if the path looks like an editor/download temp or lock file that should
/// not trigger a sync on its own (DBSYNC-29). The real file's own event arrives
/// separately; watching these just causes churn (and they're often deleted
/// moments later). Matches the last path component only.
pub(crate) fn is_editor_temp_path(relative: &str) -> bool {
    let name = relative
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(relative)
        .to_ascii_lowercase();
    name.ends_with(".tmp")
        || name.ends_with(".swp")
        || name.ends_with(".swx")
        || name.ends_with(".crdownload")
        || name.ends_with(".part")
        || name.ends_with('~') // Vim/Emacs/gedit backup
        || name.starts_with("~$") // MS Office lock/owner file
        || (name.starts_with(".~lock.") && name.ends_with('#')) // LibreOffice lock
}

/// True if `relative` matches any of `globs` (DBSYNC-36 user-defined ignore
/// patterns). Pure/side-effect-free — takes the pattern list as an argument so it
/// is trivially unit-testable without touching the process-wide
/// [`USER_IGNORE_GLOBS`] static.
///
/// KISS matching, case-insensitive, three forms (no real `**`/glob crate):
/// - Exact basename, e.g. `Thumbs.db` — matches the last path component only.
/// - `*`-prefixed suffix, e.g. `*.log` — matches if the whole relative path ends
///   with the suffix (so it also matches nested files like `sub/app.log`).
/// - Exact relative path (contains `/`), e.g. `Notes/scratch.txt` — matches only
///   that exact path, not any other file with the same basename.
pub(crate) fn matches_ignore_globs(relative: &str, globs: &[String]) -> bool {
    let rel_lower = relative.replace('\\', "/").to_ascii_lowercase();
    let basename_lower = rel_lower.rsplit('/').next().unwrap_or(&rel_lower);

    globs.iter().any(|pattern| {
        let pattern = pattern.trim();
        if pattern.is_empty() {
            return false;
        }
        let pattern_lower = pattern.to_ascii_lowercase();
        if let Some(suffix) = pattern_lower.strip_prefix('*') {
            !suffix.is_empty() && rel_lower.ends_with(suffix)
        } else if pattern_lower.contains('/') {
            rel_lower == pattern_lower
        } else {
            basename_lower == pattern_lower
        }
    })
}

/// Process-wide store of the user-defined ignore globs (DBSYNC-36), set once at
/// startup from the persisted `ignore_globs_csv` app_config value and again
/// whenever the user saves the settings panel ([`set_user_ignore_globs`]).
/// `RwLock` because the predicate is read on every scan/watch event but written
/// rarely (a settings save).
static USER_IGNORE_GLOBS: OnceLock<RwLock<Vec<String>>> = OnceLock::new();

fn user_ignore_globs_cell() -> &'static RwLock<Vec<String>> {
    USER_IGNORE_GLOBS.get_or_init(|| RwLock::new(Vec::new()))
}

/// Replaces the active set of user-defined ignore globs. Called at startup (with
/// the persisted value) and after the user saves the settings panel.
pub(crate) fn set_user_ignore_globs(globs: Vec<String>) {
    if let Ok(mut guard) = user_ignore_globs_cell().write() {
        *guard = globs;
    }
}

/// Checks `relative` against the current process-wide user ignore globs. A
/// poisoned lock fails open to "not ignored" rather than panicking or wedging the
/// sync pipeline.
fn user_ignore_globs_match(relative: &str) -> bool {
    match user_ignore_globs_cell().read() {
        Ok(guard) => matches_ignore_globs(relative, &guard),
        Err(_) => false,
    }
}

/// Parses the comma-separated `ignore_globs_csv` app_config value into a list of
/// trimmed, non-empty patterns. Mirrors [`parse_prefix_csv`] but does not strip a
/// leading `/` — ignore patterns are basenames/suffixes/relative paths, not
/// selective-sync prefixes.
pub(crate) fn parse_ignore_globs_csv(csv: Option<String>) -> Vec<String> {
    match csv {
        None => Vec::new(),
        Some(s) => s
            .split(',')
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .map(|p| p.to_string())
            .collect(),
    }
}

/// The BUILT-IN "never sync this path" predicate: OS junk
/// ([`should_ignore_local_path`]) plus editor/download temp & lock files
/// ([`is_editor_temp_path`]), WITHOUT the user's own ignore globs (DBSYNC-36). Use
/// this — not [`is_ignored_local_path`] — at any site whose meaning is "a path that
/// should never have been tracked" (e.g. startup `cleanup_stale_upload_state`), so a
/// user ignoring a real, already-synced file can never make that site drop the
/// file's index row (which would make it look "new" and re-download/churn). Keeps
/// such sites decoupled from user-glob load ordering.
pub(crate) fn is_builtin_ignored_local_path(relative: &str) -> bool {
    should_ignore_local_path(relative) || is_editor_temp_path(relative)
}

/// Single "never sync this local path" predicate for the whole pipeline: the
/// built-in ignores ([`is_builtin_ignored_local_path`]) plus user-defined ignore
/// globs ([`set_user_ignore_globs`], DBSYNC-36). Editor temps must be excluded from
/// the full scan too — not just the fs watcher — or an `~$doc.docx` the scan sees
/// gets enqueued and tracked, then fails the upload when the editor deletes it
/// (DBSYNC-55).
pub(crate) fn is_ignored_local_path(relative: &str) -> bool {
    is_builtin_ignored_local_path(relative) || user_ignore_globs_match(relative)
}

/// True if `abs` is a Windows CfAPI dehydrated (online-only) placeholder — a real
/// file whose data is NOT resident locally
/// ([`FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`]). Reading or hashing it would trigger
/// an on-demand download (hydration), so the sync pipeline must treat it as
/// cloud-only in-sync and NEVER hash it (DBSYNC-59). Uses `symlink_metadata`, which
/// only stats — it does not open the file — so it never causes a recall. Always
/// `false` off Windows.
pub(crate) fn is_dehydrated_placeholder(abs: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
        std::fs::symlink_metadata(abs)
            .map(|m| m.file_attributes() & FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS != 0)
            .unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        let _ = abs;
        false
    }
}

pub(crate) fn relpath_under(sync_folder: &Path, absolute: &Path) -> AppResult<String> {
    Ok(absolute
        .strip_prefix(sync_folder)
        .map_err(|e| AppError::Other(format!("failed to compute relative path: {e}")))?
        .to_string_lossy()
        // Canonicalize to '/' (Dropbox convention) so relative-path keys match
        // across the local index, remote index and placeholder logic on Windows,
        // where `to_string_lossy` yields '\' separators (DBSYNC-45).
        .replace('\\', "/"))
}

/// True if `input` contains a path-traversal component (`..`) — checked against
/// both `/` and `\` separators — or an embedded NUL byte. Such inputs must never
/// be used to build a Dropbox path or a local filesystem path (DBSYNC-27).
fn has_traversal(input: &str) -> bool {
    input.contains('\0') || input.split(['/', '\\']).any(|c| c == "..")
}

/// True if `input` is an OS-absolute path that has no business appearing as a
/// path relative to the sync root: a Windows drive prefix (`C:\`, `C:/`) or a
/// UNC path (`\\server`). A single leading `/` is intentionally NOT treated as
/// absolute here — it is the legitimate Dropbox root convention.
fn is_os_absolute(input: &str) -> bool {
    let bytes = input.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return true; // Windows drive-absolute, e.g. `C:\Users` or `C:/Users`
    }
    input.starts_with("\\\\") // UNC path, e.g. `\\server\share`
}

/// Turn a relative path into a Dropbox API path (`/`-prefixed), rejecting any
/// input that could escape the intended tree. Returns an error for paths that
/// contain `..`, NUL bytes, or an OS-absolute prefix instead of silently
/// building a traversing path (DBSYNC-27). A leading `/` is preserved.
pub(crate) fn normalize_dropbox_path(input: &str) -> AppResult<String> {
    if has_traversal(input) || is_os_absolute(input) {
        return Err(AppError::Sync(format!("rejected unsafe path: {input:?}")));
    }
    if input.is_empty() {
        return Ok(String::new());
    }
    // Windows relative paths use `\` separators; Dropbox requires `/`.
    let forward = input.replace('\\', "/");
    Ok(if forward.starts_with('/') {
        forward
    } else {
        format!("/{forward}")
    })
}

/// Validate that `rel` is a safe path *relative* to the sync root: no `..`, no
/// NUL byte, and not absolute (no leading `/` or `\`, no drive/UNC prefix).
/// Used before joining any remote-derived path onto the local sync folder.
pub(crate) fn validate_relative(rel: &str) -> AppResult<()> {
    if has_traversal(rel)
        || is_os_absolute(rel)
        || rel.starts_with('/')
        || rel.starts_with('\\')
    {
        return Err(AppError::Sync(format!(
            "rejected unsafe relative path: {rel:?}"
        )));
    }
    Ok(())
}

/// Join an untrusted relative path onto `root`, refusing to escape it. Validates
/// `rel` with [`validate_relative`], then verifies (lexically, without touching
/// the filesystem — the target may not exist yet) that the result stays under
/// `root`. This is the single choke point for every remote→local write sink.
pub(crate) fn safe_join(root: &Path, rel: &str) -> AppResult<PathBuf> {
    validate_relative(rel)?;
    let joined = root.join(rel);
    if !joined.starts_with(root) {
        return Err(AppError::Sync(format!("path escapes sync root: {rel:?}")));
    }
    Ok(joined)
}

pub(crate) fn parse_prefix_csv(csv: Option<String>) -> Vec<String> {
    match csv {
        None => Vec::new(),
        Some(s) => s
            .split(',')
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .map(|p| p.trim_start_matches('/').to_string())
            .collect(),
    }
}

pub(crate) fn is_path_allowed(
    relative: &str,
    include_prefixes: &[String],
    exclude_prefixes: &[String],
) -> bool {
    let included = if include_prefixes.is_empty() {
        true
    } else {
        include_prefixes.iter().any(|p| relative.starts_with(p))
    };

    if !included {
        return false;
    }

    if exclude_prefixes.iter().any(|p| relative.starts_with(p)) {
        return false;
    }

    true
}

/// Block size used by the Dropbox content_hash algorithm (4 MiB).
const DROPBOX_BLOCK_SIZE: usize = 4 * 1024 * 1024;

/// Compute a [Dropbox content_hash](https://www.dropbox.com/developers/reference/content-hash)
/// for the file at `path`.
///
/// The algorithm:
/// 1. Split the file into 4 MiB blocks (last block may be smaller).
/// 2. SHA-256 each block individually.
/// 3. Concatenate the raw 32-byte block digests.
/// 4. SHA-256 the concatenation.
/// 5. Return the result as a lowercase hex string.
///
/// Returns `(content_hash_hex, file_size_bytes, modified_unix_timestamp)`.
///
/// # Errors
///
/// Returns an error string if the file cannot be opened, read, or stat-ed.
pub(crate) fn hash_file(path: &Path) -> AppResult<(String, i64, i64)> {
    let mut file =
        File::open(path).map_err(|e| AppError::Io(format!("cannot open file for hash: {e}")))?;
    let mut buffer = [0_u8; 8192];

    // Accumulates the raw 32-byte SHA-256 digest of each 4 MiB block.
    let mut block_digests: Vec<u8> = Vec::new();
    let mut block_hasher = Sha256::new();
    let mut bytes_in_block: usize = 0;

    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }

        let mut remaining = &buffer[..read];
        while !remaining.is_empty() {
            let space_in_block = DROPBOX_BLOCK_SIZE - bytes_in_block;
            let to_consume = remaining.len().min(space_in_block);
            block_hasher.update(&remaining[..to_consume]);
            bytes_in_block += to_consume;
            remaining = &remaining[to_consume..];

            if bytes_in_block == DROPBOX_BLOCK_SIZE {
                // Finalise this block and start the next one.
                let digest = block_hasher.finalize_reset();
                block_digests.extend_from_slice(&digest);
                bytes_in_block = 0;
            }
        }
    }

    // Finalise the trailing partial block (if any bytes were buffered).
    // An empty file has zero blocks: block_digests stays empty, and
    // SHA-256(b"") = e3b0c44298fc1c149afbf4c8996fb924... — which is the
    // correct Dropbox content_hash for an empty file.
    if bytes_in_block > 0 {
        let digest = block_hasher.finalize();
        block_digests.extend_from_slice(&digest);
    }

    // SHA-256 the concatenation of all block digests.
    let content_hash = Sha256::digest(&block_digests);

    let metadata = file.metadata()?;
    let size = metadata.len() as i64;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|ts| ts.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    Ok((format!("{:x}", content_hash), size, modified))
}

pub(crate) fn create_conflicted_copy(path: &Path) -> AppResult<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::Other("file has no parent for conflict copy".to_string()))?;
    let stem = path
        .file_stem()
        .ok_or_else(|| AppError::Other("missing file stem".to_string()))?
        .to_string_lossy();
    let ext = path.extension().map(|e| e.to_string_lossy().to_string());
    let ts = Utc::now().format("%Y%m%d%H%M%S");

    let name = match ext {
        Some(ext) => format!("{} (conflicted copy {}).{}", stem, ts, ext),
        None => format!("{} (conflicted copy {})", stem, ts),
    };

    let dest = parent.join(name);
    fs::copy(path, &dest)
        .map_err(|e| AppError::Io(format!("failed to create conflicted copy: {e}")))?;
    Ok(dest)
}

pub(crate) fn backoff_seconds(attempt: i64) -> i64 {
    let safe_attempt = attempt.clamp(1, 10) as u32;
    2_i64.pow(safe_attempt)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use sha2::{Digest, Sha256};
    use tempfile::NamedTempFile;

    use std::path::Path;

    use super::{
        backoff_seconds, hash_file, is_builtin_ignored_local_path, is_dehydrated_placeholder,
        is_ignored_local_path, matches_ignore_globs, normalize_dropbox_path, safe_join,
        set_user_ignore_globs, should_ignore_local_path, validate_relative, DROPBOX_BLOCK_SIZE,
    };

    #[test]
    fn dehydrated_placeholder_is_false_for_plain_and_missing_files() {
        // A normal (fully-resident) file has no RECALL_ON_DATA_ACCESS attribute, so it
        // is never treated as a dehydrated placeholder — the sync pipeline must hash
        // it normally. A non-existent path is likewise not a placeholder.
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"resident bytes").unwrap();
        assert!(!is_dehydrated_placeholder(f.path()));
        assert!(!is_dehydrated_placeholder(Path::new(
            "does-not-exist-xyz.bin"
        )));
    }

    #[test]
    fn ignored_local_path_covers_os_junk_and_editor_temps() {
        // Editor/office temp & lock files (the DBSYNC-55 fix — must be excluded
        // from the full scan, not just the fs watcher).
        for p in [
            "~$report.docx",
            "sub/~$sheet.xlsx",
            "doc.txt.tmp",
            "~WRD0001.tmp",
            "a.swp",
            "video.crdownload",
            "dl.part",
            "notes.txt~",
        ] {
            assert!(is_ignored_local_path(p), "should ignore {p:?}");
        }
        // OS junk still ignored.
        assert!(is_ignored_local_path(".DS_Store"));
        assert!(is_ignored_local_path("dir/._resource"));
        // Real user files are NOT ignored.
        for p in ["report.docx", "dir/photo.jpg", "notes.txt"] {
            assert!(!is_ignored_local_path(p), "should not ignore {p:?}");
        }
    }

    #[test]
    fn thumbs_db_is_ignored_by_default() {
        assert!(should_ignore_local_path("Thumbs.db"));
        assert!(should_ignore_local_path("sub/Thumbs.db"));
    }

    #[test]
    fn matches_ignore_globs_suffix_form() {
        let globs = vec!["*.log".to_string()];
        assert!(matches_ignore_globs("app.log", &globs));
        assert!(matches_ignore_globs("sub/app.log", &globs));
        assert!(!matches_ignore_globs("app.txt", &globs));
    }

    #[test]
    fn matches_ignore_globs_exact_basename_form() {
        let globs = vec!["secret.key".to_string()];
        assert!(matches_ignore_globs("dir/secret.key", &globs));
        assert!(matches_ignore_globs("secret.key", &globs));
        assert!(!matches_ignore_globs("dir/other.key", &globs));
    }

    #[test]
    fn matches_ignore_globs_exact_relative_path_form() {
        let globs = vec!["Notes/scratch.txt".to_string()];
        assert!(matches_ignore_globs("Notes/scratch.txt", &globs));
        // Same basename elsewhere is NOT matched — the pattern is a full relative path.
        assert!(!matches_ignore_globs("Other/scratch.txt", &globs));
        assert!(!matches_ignore_globs("scratch.txt", &globs));
    }

    #[test]
    fn user_ignore_globs_apply_through_is_ignored_local_path() {
        // The static is process-wide and cargo tests run in parallel — reset it to
        // empty when done so other tests in this file aren't affected.
        set_user_ignore_globs(vec!["*.log".to_string()]);
        assert!(is_ignored_local_path("a.log"));
        set_user_ignore_globs(Vec::new());
        assert!(!is_ignored_local_path("a.log"));
    }

    #[test]
    fn builtin_ignore_predicate_excludes_user_globs() {
        // The startup cleanup uses the BUILT-IN predicate so a user ignoring a real,
        // already-synced file never makes cleanup drop that file's index row. Even
        // while `*.log` is an active user glob, `is_builtin_ignored_local_path` must
        // NOT match `a.log` (only the combined predicate does). Built-in junk still
        // matches. Reset the process-wide static when done (parallel tests).
        set_user_ignore_globs(vec!["*.log".to_string()]);
        assert!(!is_builtin_ignored_local_path("a.log"));
        assert!(is_ignored_local_path("a.log"));
        set_user_ignore_globs(Vec::new());
        assert!(is_builtin_ignored_local_path("Thumbs.db"));
        assert!(is_builtin_ignored_local_path("~$doc.docx"));
        assert!(!is_builtin_ignored_local_path("a.log"));
    }

    // ---------------------------------------------------------------------------
    // Helper: compute the Dropbox content_hash for an in-memory byte slice so
    // that tests can build the expected value without touching disk.
    // ---------------------------------------------------------------------------
    fn expected_content_hash(data: &[u8]) -> String {
        let mut block_digests: Vec<u8> = Vec::new();
        let mut block_hasher = Sha256::new();
        let mut bytes_in_block: usize = 0;

        let mut remaining = data;
        while !remaining.is_empty() {
            let space_in_block = DROPBOX_BLOCK_SIZE - bytes_in_block;
            let to_consume = remaining.len().min(space_in_block);
            block_hasher.update(&remaining[..to_consume]);
            bytes_in_block += to_consume;
            remaining = &remaining[to_consume..];

            if bytes_in_block == DROPBOX_BLOCK_SIZE {
                let digest = block_hasher.finalize_reset();
                block_digests.extend_from_slice(&digest);
                bytes_in_block = 0;
            }
        }
        if bytes_in_block > 0 {
            let digest = block_hasher.finalize();
            block_digests.extend_from_slice(&digest);
        }
        format!("{:x}", Sha256::digest(&block_digests))
    }

    // ---------------------------------------------------------------------------
    // Existing test
    // ---------------------------------------------------------------------------

    #[test]
    fn backoff_grows_exponentially() {
        assert_eq!(backoff_seconds(1), 2);
        assert_eq!(backoff_seconds(2), 4);
        assert_eq!(backoff_seconds(3), 8);
    }

    // ---------------------------------------------------------------------------
    // content_hash tests
    // ---------------------------------------------------------------------------

    #[test]
    fn content_hash_empty_file() {
        let tmp = NamedTempFile::new().expect("tempfile");
        let (hash, size, mtime) = hash_file(tmp.path()).expect("hash_file");
        assert_eq!(
            hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "empty file must match SHA-256 of empty bytes"
        );
        assert_eq!(size, 0);
        // mtime may legally be 0 on some platforms, so just assert it is non-negative.
        assert!(mtime >= 0);
    }

    #[test]
    fn content_hash_small_file() {
        // A file smaller than 4 MiB has exactly one block.
        // content_hash = SHA-256( SHA-256(content) )
        let content = b"hello, dropbox content hash!";
        let mut tmp = NamedTempFile::new().expect("tempfile");
        tmp.write_all(content).expect("write");
        tmp.flush().expect("flush");

        let (hash, size, mtime) = hash_file(tmp.path()).expect("hash_file");

        let expected = expected_content_hash(content);
        assert_eq!(hash, expected);
        assert_eq!(size, content.len() as i64);
        assert!(mtime > 0, "modified timestamp should be non-zero");
    }

    #[test]
    fn content_hash_exactly_one_block() {
        // Exactly 4 MiB of zeros: one full block, no remainder.
        let content = vec![0_u8; DROPBOX_BLOCK_SIZE];
        let mut tmp = NamedTempFile::new().expect("tempfile");
        tmp.write_all(&content).expect("write");
        tmp.flush().expect("flush");

        let (hash, size, _mtime) = hash_file(tmp.path()).expect("hash_file");

        let expected = expected_content_hash(&content);
        assert_eq!(hash, expected);
        assert_eq!(size, DROPBOX_BLOCK_SIZE as i64);
    }

    #[test]
    fn content_hash_one_block_plus_one_byte() {
        // 4 MiB + 1 byte: two blocks (one full, one 1-byte).
        let mut content = vec![0_u8; DROPBOX_BLOCK_SIZE];
        content.push(0_u8);

        let mut tmp = NamedTempFile::new().expect("tempfile");
        tmp.write_all(&content).expect("write");
        tmp.flush().expect("flush");

        let (hash, size, _mtime) = hash_file(tmp.path()).expect("hash_file");

        let expected = expected_content_hash(&content);
        assert_eq!(hash, expected);
        assert_eq!(size, (DROPBOX_BLOCK_SIZE + 1) as i64);
    }

    #[test]
    fn content_hash_known_value_independent() {
        // Independent cross-check: compute expected hash WITHOUT using the
        // helper function, to avoid the "same oracle" testing pitfall.
        // For a single-block file: content_hash = SHA-256(SHA-256(content)).
        let content = b"hello, dropbox content hash!";
        let inner_digest = Sha256::digest(content);
        let expected = format!("{:x}", Sha256::digest(inner_digest));

        let mut tmp = NamedTempFile::new().expect("tempfile");
        tmp.write_all(content).expect("write");
        tmp.flush().expect("flush");

        let (hash, _, _) = hash_file(tmp.path()).expect("hash_file");
        assert_eq!(hash, expected);
    }

    // ---------------------------------------------------------------------------
    // Path-traversal hardening (DBSYNC-27)
    // ---------------------------------------------------------------------------

    #[test]
    fn normalize_accepts_legit_paths() {
        assert_eq!(normalize_dropbox_path("").unwrap(), "");
        assert_eq!(normalize_dropbox_path("Cocina/Pizza").unwrap(), "/Cocina/Pizza");
        // A leading '/' is the Dropbox root convention and must be preserved.
        assert_eq!(normalize_dropbox_path("/Cocina").unwrap(), "/Cocina");
        // Windows separators are canonicalised to '/'.
        assert_eq!(normalize_dropbox_path("Cocina\\Pizza").unwrap(), "/Cocina/Pizza");
    }

    #[test]
    fn normalize_rejects_traversal_payloads() {
        for bad in [
            "../etc/passwd",
            "Cocina/../../secret",
            "..\\Windows\\System32",
            "a/../../b",
            "C:\\Windows",
            "C:/Windows",
            "\\\\server\\share",
            "with\0null",
        ] {
            assert!(
                normalize_dropbox_path(bad).is_err(),
                "expected rejection for {bad:?}"
            );
        }
    }

    #[test]
    fn validate_relative_rejects_absolute_and_traversal() {
        assert!(validate_relative("Cocina/Pizza").is_ok());
        assert!(validate_relative("a/b/c.txt").is_ok());

        for bad in [
            "..",
            "../x",
            "a/../../b",
            "/etc/passwd",
            "\\Windows",
            "C:\\x",
            "\\\\unc\\x",
            "x\0y",
        ] {
            assert!(validate_relative(bad).is_err(), "expected rejection for {bad:?}");
        }
    }

    #[test]
    fn safe_join_stays_under_root() {
        let root = Path::new("/sync/root");
        let joined = safe_join(root, "Cocina/Pizza.txt").expect("legit join");
        assert!(joined.starts_with(root));
        assert_eq!(joined, Path::new("/sync/root/Cocina/Pizza.txt"));
    }

    #[test]
    fn editor_temp_paths_are_recognized() {
        use super::is_editor_temp_path;
        for temp in [
            "doc.txt.tmp",
            "sub/.file.swp",
            "a.swx",
            "video.crdownload",
            "big.iso.part",
            "notes.txt~",
            "~$report.docx",
            ".~lock.sheet.ods#",
        ] {
            assert!(is_editor_temp_path(temp), "should flag {temp:?}");
        }
        for real in ["report.docx", "a/b/c.txt", "photo.jpg", "archive.tar.gz"] {
            assert!(!is_editor_temp_path(real), "should NOT flag {real:?}");
        }
    }

    #[test]
    fn safe_join_refuses_to_escape_root() {
        let root = Path::new("/sync/root");
        for bad in [
            "../outside",
            "a/../../b",
            "/abs/path",
            "..",
            "x\0y",
            // A remote-derived child name carrying embedded separators (DBSYNC-27
            // review finding #1: the `.cloudsc` placeholder write sink).
            "..\\..\\evil.cloudsc",
            "sub\\..\\..\\evil",
        ] {
            assert!(
                safe_join(root, bad).is_err(),
                "safe_join must refuse {bad:?}"
            );
        }
    }

    #[test]
    fn content_hash_returns_correct_size_and_mtime() {
        let content = b"size and mtime check";
        let mut tmp = NamedTempFile::new().expect("tempfile");
        tmp.write_all(content).expect("write");
        tmp.flush().expect("flush");

        let (_hash, size, mtime) = hash_file(tmp.path()).expect("hash_file");
        assert_eq!(size, content.len() as i64);
        assert!(mtime > 0, "modified timestamp must be non-zero for a newly created file");
    }
}
