use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

use chrono::Utc;
use sha2::{Digest, Sha256};

use crate::error::{AppError, AppResult};

/// OS junk that must never be synced. Matching is done on a `/`-canonical view of
/// the path.
///
/// The separator fold is Windows-only (DBSYNC-104): `\\` is a legal byte in a macOS
/// filename, and folding it here meant a root file genuinely named `a\\.DS_Store` was
/// matched as `.DS_Store` and silently never synced.
pub(crate) fn should_ignore_local_path(relative: &str) -> bool {
    #[cfg(windows)]
    let p: &str = &relative.replace('\\', "/");
    #[cfg(not(windows))]
    let p: &str = relative;
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
    // DBSYNC-103 #188: the key is compared in canonical form (NFC on macOS), the same form
    // `parse_ignore_globs_csv` gives the patterns, so the spelling either side was typed in
    // cannot decide the outcome. Borrowed (no allocation) when it is already canonical.
    let relative = canonical_cow(relative);
    // Separator fold is Windows-only (DBSYNC-104) — see `should_ignore_local_path`.
    #[cfg(windows)]
    let rel_lower = relative.replace('\\', "/").to_ascii_lowercase();
    #[cfg(not(windows))]
    let rel_lower = relative.to_ascii_lowercase();
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
///
/// Each pattern is put in the canonical key form (NFC on macOS, identity elsewhere;
/// DBSYNC-103 #188). A name typed or pasted in NFD would otherwise never match the NFC
/// keys the index now holds, and the ignored file would start syncing. The stored CSV is
/// not rewritten: only the parsed view is canonical.
pub(crate) fn parse_ignore_globs_csv(csv: Option<String>) -> Vec<String> {
    match csv {
        None => Vec::new(),
        Some(s) => s
            .split(',')
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .map(to_canonical_form)
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

/// [`to_canonical_form`] without the allocation when `s` is already canonical (always, off
/// macOS). For predicates that run once per scanned file.
fn canonical_cow(s: &str) -> std::borrow::Cow<'_, str> {
    if is_canonical_form(s) {
        std::borrow::Cow::Borrowed(s)
    } else {
        std::borrow::Cow::Owned(to_canonical_form(s))
    }
}

/// The single place an **absolute local path** becomes a relative index key.
///
/// Every local producer goes through here (DBSYNC-104). The storage layer relies on that:
/// it no longer normalizes separators itself, it asserts that callers have.
///
/// Callers that produce an index key use [`key_from_disk`], which adds NFC on macOS
/// (DBSYNC-103); this function alone is for paths that are only joined back onto the root.
///
/// Remote keys have a second, independent source: `path_display` from the Dropbox API,
/// turned into a key by [`key_from_dropbox`] in `remote_index`, `cloudsc_ops` and
/// `dropbox_transfer`.
/// Those never reach this function and do not need to — a Dropbox path is `/`-canonical by
/// construction. Review L1 flagged an earlier version of this comment for claiming
/// otherwise, which mattered because this claim is the justification for deleting the
/// storage layer's own rewrites.
pub(crate) fn relpath_under(sync_folder: &Path, absolute: &Path) -> AppResult<String> {
    let rel = absolute
        .strip_prefix(sync_folder)
        .map_err(|e| AppError::Other(format!("failed to compute relative path: {e}")))?
        .to_string_lossy()
        .into_owned();

    // Canonicalize to '/' (Dropbox convention) so relative-path keys match across the
    // local index, remote index and placeholder logic on Windows, where
    // `to_string_lossy` yields '\' separators (DBSYNC-45).
    //
    // Windows ONLY, and deliberately so (DBSYNC-104). `\` cannot occur in a Windows
    // filename, so there the rewrite can only ever hit a separator. On macOS it is a
    // legal byte in a name, and rewriting it unconditionally made a root file literally
    // named `a\b.txt` and the genuine `a/b.txt` inside folder `a` collapse onto one
    // `TEXT PRIMARY KEY`: they alternated ownership of the row and each re-uploaded over
    // the other, and deleting the unrelated folder `a` issued a recursive `delete_v2`
    // that destroyed the remote copy of the other file.
    #[cfg(windows)]
    let rel = rel.replace('\\', "/");

    Ok(rel)
}

/// NFC-normalize `s` on macOS; identity elsewhere (DBSYNC-103).
///
/// APFS looks names up form-insensitively, so one file can be reached as NFC or NFD, but
/// the index is byte-exact. Probe (2026-10-13): Dropbox stores and returns NFC and treats
/// NFC/NFD as one name. Every index key is therefore NFC on macOS. Windows and Linux keep
/// bytes untouched: NTFS preserves whatever form it was given and nothing here changes it.
#[cfg(target_os = "macos")]
pub(crate) fn to_canonical_form(s: &str) -> String {
    use unicode_normalization::{is_nfc, UnicodeNormalization};
    if is_nfc(s) {
        s.to_owned()
    } else {
        s.nfc().collect()
    }
}

/// Identity off macOS, see the macOS variant.
#[cfg(not(target_os = "macos"))]
pub(crate) fn to_canonical_form(s: &str) -> String {
    s.to_owned()
}

/// True if `s` is already in the canonical Unicode form for index keys: NFC on macOS,
/// always true elsewhere. Used by the storage layer's debug assertion (DBSYNC-103).
#[cfg(target_os = "macos")]
pub(crate) fn is_canonical_form(s: &str) -> bool {
    unicode_normalization::is_nfc(s)
}

/// Always true off macOS, see the macOS variant.
#[cfg(not(target_os = "macos"))]
pub(crate) fn is_canonical_form(_s: &str) -> bool {
    true
}

/// The single place an **absolute local path** becomes an index key: [`relpath_under`]
/// plus NFC on macOS (DBSYNC-103). Unchanged on every other platform.
///
/// Use this, not `relpath_under`, whenever the result is stored, compared with a stored
/// key, or enqueued. `relpath_under` alone is right only where the result is just joined
/// back onto the sync root to reach the file (APFS resolves any form).
pub(crate) fn key_from_disk(sync_folder: &Path, absolute: &Path) -> AppResult<String> {
    relpath_under(sync_folder, absolute).map(|rel| to_canonical_form(&rel))
}

/// The single place a Dropbox `path_display` becomes an index key: the leading `/`
/// trimmed plus NFC on macOS (DBSYNC-103). The input is NOT altered as an API address:
/// callers that send the path back to Dropbox keep their own `path_display`.
pub(crate) fn key_from_dropbox(path_display: &str) -> String {
    to_canonical_form(path_display.trim_start_matches('/'))
}

/// Comparison form of an index key or path: NFC then lowercase on macOS, plain lowercase
/// elsewhere (the pre-existing behaviour). Never stored (DBSYNC-103).
///
/// Both sides of every local-vs-remote or remote-vs-remote match must go through this one
/// function. Dropbox is case-insensitive and form-insensitive, so the match must be too.
pub(crate) fn fold(s: &str) -> String {
    to_canonical_form(s).to_lowercase()
}

/// True if `input` contains a path-traversal component (`..`) or an embedded NUL byte.
/// Such inputs must never be used to build a Dropbox path or a local filesystem path
/// (DBSYNC-27).
///
/// The NUL check is unconditional. The separator set is not: `\` counts as a separator
/// only on Windows (DBSYNC-104).
///
/// On Unix `\` is a legal filename byte, so `a\..\c.txt` is ONE component, not three.
/// Treating it as a traversal rejected a legal file that the pipeline had already indexed
/// and enqueued — the upload then failed with `AppError::Sync`, which is not an
/// `AppError::Dropbox` and so escapes the permanent-path classifier, burning five attempts
/// and reporting a generic error. That is precisely the outcome the classifier exists to
/// remove. The row was also permanently unnormalizable, so both remote sweeps logged a
/// warning for it on every tick, forever.
///
/// Nothing is given up by gating it. Measured: `Path::join` treats every such name as a
/// single component, so even `a\..\..\..\etc\passwd` stays under the sync root, and
/// `safe_join`'s `starts_with(root)` check confirms it. Dropbox rejects the name anyway
/// with `path/malformed_path`, which is now classified as permanent and reported with a
/// message the user can act on — a strictly better outcome than a local rejection.
///
/// Do NOT "fix" this back to an unconditional split: the `/`-only split on Unix and the
/// both-separator split on Windows are each correct for their platform's filename rules.
fn has_traversal(input: &str) -> bool {
    if input.contains('\0') {
        return true;
    }
    #[cfg(windows)]
    let separators: &[char] = &['/', '\\'];
    #[cfg(not(windows))]
    let separators: &[char] = &['/'];
    input.split(separators).any(|c| c == "..")
}

/// True if `input` is an OS-absolute path that has no business appearing as a
/// path relative to the sync root: a Windows drive prefix (`C:\`, `C:/`, and the
/// drive-*relative* `C:foo`) or a UNC path (`\\server`). A single leading `/` is
/// intentionally NOT treated as absolute here — it is the legitimate Dropbox root
/// convention.
///
/// **Windows only** (DBSYNC-104). On Unix, `:` and `\` are legal filename bytes and
/// none of these forms can escape: `Path::join` keeps every one of them under the
/// root (measured). Applying the check there rejected ordinary macOS names such as
/// `C:notas.txt` and `D:2024/informe.pdf`, and because the rejection propagated out
/// of the remote sweep loop, one such file stopped reconciliation — downloads, delete
/// reconciliation, remote-present updates — for every other file, indefinitely.
fn is_os_absolute(input: &str) -> bool {
    #[cfg(windows)]
    {
        // Deliberately NOT requiring a separator after the colon, despite what an
        // earlier version of this comment implied. On Windows `C:foo` is drive-relative
        // and `PathBuf::push` replaces the base for it exactly as it does for `C:\foo`,
        // so demanding `C:\` or `C:/` here would open the escape this function closes.
        let bytes = input.as_bytes();
        if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            return true;
        }
        input.starts_with("\\\\") // UNC path, e.g. `\\server\share`
    }
    #[cfg(not(windows))]
    {
        let _ = input;
        false
    }
}

/// Turn a relative path into a Dropbox API path (`/`-prefixed), rejecting any
/// input that could escape the intended tree. Returns an error for paths that
/// contain `..`, NUL bytes, or an OS-absolute prefix instead of silently
/// building a traversing path (DBSYNC-27). A leading `/` is preserved.
///
/// The `..` and OS-absolute checks are separator-sensitive and therefore platform-gated —
/// see [`has_traversal`] and [`is_os_absolute`]. On Unix `is_os_absolute` is always false.
/// The NUL check is unconditional.
pub(crate) fn normalize_dropbox_path(input: &str) -> AppResult<String> {
    if has_traversal(input) || is_os_absolute(input) {
        return Err(AppError::Sync(format!("rejected unsafe path: {input:?}")));
    }
    if input.is_empty() {
        return Ok(String::new());
    }
    // Windows relative paths use `\` separators; Dropbox requires `/`. Windows-only
    // (DBSYNC-104): after `relpath_under` the key already arrives `/`-canonical, so on
    // Unix this fold could only ever corrupt a legal filename.
    #[cfg(windows)]
    let forward = input.replace('\\', "/");
    #[cfg(not(windows))]
    let forward = input.to_string();
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
    // A leading `\` is a root marker on Windows and an ordinary first byte of a filename
    // on Unix, so it is gated like every other separator rule in this file (DBSYNC-104,
    // review round 2). Ungated, a legal macOS root file named `\x.txt` was indexed and
    // enqueued and then rejected here, inside `safe_join`, as `AppError::Sync` — which is
    // not an `AppError::Dropbox`, so it escaped the permanent-path classifier and burned
    // five attempts for a generic error. That is the same failure shape `has_traversal`
    // had, one byte along.
    //
    // Containment is unaffected: `safe_join`'s `starts_with(root)` check still applies, and
    // `root.join("\\x.txt")` on Unix is one component under the root.
    #[cfg(windows)]
    let os_root_marker = rel.starts_with('\\');
    #[cfg(not(windows))]
    let os_root_marker = false;

    if has_traversal(rel) || is_os_absolute(rel) || rel.starts_with('/') || os_root_marker {
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

/// Parses a comma-separated selective-sync prefix list. Each entry is trimmed, loses its
/// leading and trailing `/`, and is put in the canonical key form (NFC on macOS, identity
/// elsewhere; DBSYNC-103 #188). Entries that are empty **after** the slashes are stripped
/// are dropped, so a lone `/` yields no prefix instead of an empty one that matches every
/// path (DBSYNC-114 H8). The stored CSV is not rewritten.
///
/// Case folding is deliberately not done here: [`path_scope`] folds both sides at compare
/// time, once, so a prefix list built by hand (tests, callers) behaves the same as a parsed one.
pub(crate) fn parse_prefix_csv(csv: Option<String>) -> Vec<String> {
    match csv {
        None => Vec::new(),
        Some(s) => s
            .split(',')
            .map(|p| p.trim().trim_matches('/'))
            // `.` and `..` name no folder in the index (keys never contain them), so as an
            // entry they would never match anything: an exclude would silently do nothing, and
            // under an include they would put every real path outside the include list.
            .filter(|p| !p.is_empty() && *p != "." && *p != "..")
            .map(to_canonical_form)
            .collect(),
    }
}

/// Where a relative path sits with respect to the selective-sync lists (DBSYNC-114).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PathScope {
    /// Inside the include list (or the list is empty) and not under an exclude.
    Synced,
    /// Outside the include list, or under an exclude.
    Excluded,
    /// A strict folder-ancestor of an include entry (`Fotos` for include `Fotos/2024`).
    /// The predicate cannot tell a file from a directory, so it reports `Ancestor` for any
    /// path that *would be* such a folder; callers know whether the entry is a directory
    /// (the directory itself must be traversed, a file directly inside it is `Excluded`
    /// semantically and they must treat it so). [`is_path_allowed`] treats it as not allowed.
    Ancestor,
}

/// True if `path` equals `prefix` or lies under it, on a `/` boundary. Both already folded.
fn under_prefix(path: &str, prefix: &str) -> bool {
    path.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Classifies `relative` against the include and exclude prefix lists.
///
/// Both sides go through [`fold`] (case-insensitive, NFC on macOS) and a leading or trailing
/// `/` on a prefix is ignored; prefixes that are empty after that are skipped. Matching is on
/// folder boundaries: `Fotos` covers `Fotos` and `Fotos/x`, not `Fotos2024`. Exclude wins
/// over include. An empty include list means everything is included.
#[cfg(test)]
pub(crate) fn path_scope(
    relative: &str,
    include_prefixes: &[String],
    exclude_prefixes: &[String],
) -> PathScope {
    PrefixFilter::new(include_prefixes, exclude_prefixes).scope(relative)
}

/// The include and exclude lists folded ONCE, so a pass that classifies thousands of paths
/// (the scan walk, a delta batch, the queue drain) does not re-fold every prefix per path
/// (DBSYNC-114). [`path_scope`] is a thin wrapper over this; the two cannot disagree.
#[derive(Debug, Clone, Default)]
pub(crate) struct PrefixFilter {
    include: Vec<String>,
    exclude: Vec<String>,
    /// The paths whose first upload had not landed when the filter was loaded (DBSYNC-114).
    /// A pass loads its filter before it releases anything, so this is the marker set as it
    /// stood BEFORE any file-level release in the pass; see [`Self::unconfirmed_beneath`].
    /// Not part of the scope rules or of [`Self::signature`].
    /// Every marked path AND every ancestor folder of one, so a lookup is one hash probe.
    unconfirmed: std::collections::HashSet<String>,
}

impl PrefixFilter {
    /// Folds both lists (trim, strip `/`, [`fold`]); entries empty after that are dropped.
    pub(crate) fn new(include_prefixes: &[String], exclude_prefixes: &[String]) -> Self {
        let fold_prefixes = |list: &[String]| -> Vec<String> {
            list.iter()
                .map(|p| fold(p.trim().trim_matches('/')))
                .filter(|p| !p.is_empty())
                .collect()
        };
        Self {
            include: fold_prefixes(include_prefixes),
            exclude: fold_prefixes(exclude_prefixes),
            unconfirmed: std::collections::HashSet::new(),
        }
    }

    /// Reads and folds the two stored CSVs. Marker-free: the first-upload markers are NOT read
    /// (that is a whole-table read, and this runs per drained job and per download); only a
    /// deletion pass needs them, see [`Self::load_for_deletion_pass`].
    pub(crate) fn load(db: &crate::storage::db::Db) -> AppResult<Self> {
        Ok(Self::new(
            &parse_prefix_csv(db.get_include_prefixes_csv()?),
            &parse_prefix_csv(db.get_exclude_prefixes_csv()?),
        ))
    }

    /// [`Self::load`] plus a snapshot of the first-upload markers. Used ONLY where a deletion
    /// pass loads its filter (the scan and the watcher batch); call it BEFORE the pass
    /// releases anything so [`Self::unconfirmed_beneath`] sees the markers as they stood.
    pub(crate) fn load_for_deletion_pass(db: &crate::storage::db::Db) -> AppResult<Self> {
        let mut filter = Self::load(db)?;
        for path in db.list_unconfirmed_uploads()? {
            let mut end = path.len();
            filter.unconfirmed.insert(path.clone());
            while let Some(i) = path[..end].rfind('/') {
                filter.unconfirmed.insert(path[..i].to_string());
                end = i;
            }
        }
        Ok(filter)
    }

    /// Whether a path at or beneath the folder `prefix` was still awaiting its first upload
    /// when a deletion-pass filter was loaded. Boundary-safe (`d` does not match `d-other`),
    /// exact (binary) comparison like the table it mirrors. Always false for a plain `load`.
    pub(crate) fn unconfirmed_beneath(&self, prefix: &str) -> bool {
        self.unconfirmed.contains(prefix)
    }

    /// A stable string for the folded lists, to tell "the filter changed" from "it did not".
    pub(crate) fn signature(&self) -> String {
        format!(
            "i:{}|x:{}",
            self.include.join("\u{1f}"),
            self.exclude.join("\u{1f}")
        )
    }

    /// True when no list has an effective entry: every path is [`PathScope::Synced`].
    pub(crate) fn is_inactive(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty()
    }

    /// Classifies `relative`; see [`path_scope`] for the rules.
    pub(crate) fn scope(&self, relative: &str) -> PathScope {
        if self.is_inactive() {
            return PathScope::Synced;
        }
        let path = fold(relative);
        let path = path.trim_matches('/');
        if self.exclude.iter().any(|p| under_prefix(path, p)) {
            return PathScope::Excluded;
        }
        if self.include.is_empty() || self.include.iter().any(|p| under_prefix(path, p)) {
            return PathScope::Synced;
        }
        // Strict ancestor: the include entry lies under `path` (and is not equal to it).
        if self
            .include
            .iter()
            .any(|p| p != path && (path.is_empty() || under_prefix(p, path)))
        {
            return PathScope::Ancestor;
        }
        PathScope::Excluded
    }

    /// Scope of a path known to be a FILE: `Ancestor` is a folder-only notion, so a file
    /// reported as one is not under any include and is `Excluded`.
    pub(crate) fn file_scope(&self, relative: &str) -> PathScope {
        match self.scope(relative) {
            PathScope::Ancestor => PathScope::Excluded,
            other => other,
        }
    }

    /// True if a file at `relative` is in the synced zone.
    pub(crate) fn file_synced(&self, relative: &str) -> bool {
        self.file_scope(relative) == PathScope::Synced
    }

    /// True when `relative` is itself in the synced zone but an EXCLUDED path lies strictly
    /// beneath it (DBSYNC-114, maintainer decision on #213). A recursive remote `delete_v2` of
    /// such a folder would take the excluded content with it, so every folder-delete source
    /// refuses it and deletes the synced files one by one instead.
    ///
    /// Only an exclude prefix can be beneath a Synced folder: under an include list a Synced
    /// folder lies wholly inside an include entry, and the folders ABOVE an entry are
    /// [`PathScope::Ancestor`], which is [`Self::not_deletable`] already. The empty path (the
    /// sync root) reports `true` when any exclude exists.
    pub(crate) fn excluded_beneath(&self, relative: &str) -> bool {
        if self.exclude.is_empty() || self.scope(relative) != PathScope::Synced {
            return false;
        }
        let path = fold(relative);
        let path = path.trim_matches('/');
        self.exclude
            .iter()
            .any(|p| p != path && (path.is_empty() || under_prefix(p, path)))
    }

    /// True if `relative` must never be a deletion candidate, whatever kind of entry it is:
    /// anything not [`PathScope::Synced`] (Excluded and Ancestor alike; DBSYNC-114 H2).
    pub(crate) fn not_deletable(&self, relative: &str) -> bool {
        self.scope(relative) != PathScope::Synced
    }
}

#[cfg(test)]
pub(crate) fn is_path_allowed(
    relative: &str,
    include_prefixes: &[String],
    exclude_prefixes: &[String],
) -> bool {
    path_scope(relative, include_prefixes, exclude_prefixes) == PathScope::Synced
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

/// A name for a conflicted copy of `path` that nothing occupies yet, in the same folder
/// (`stem (conflicted copy <stamp>[ n]).ext`). Nothing is created or moved.
pub(crate) fn unique_conflicted_name(path: &Path) -> AppResult<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::Other("file has no parent for conflict copy".to_string()))?;
    let stem = path
        .file_stem()
        .ok_or_else(|| AppError::Other("missing file stem".to_string()))?
        .to_string_lossy();
    let ext = path.extension().map(|e| e.to_string_lossy().to_string());
    let ts = Utc::now().format("%Y%m%d%H%M%S");

    // The stamp has one-second resolution, so a second copy of the same file within the same
    // second would silently overwrite the first with `fs::copy`. A copy exists because its
    // bytes are not anywhere else (DBSYNC-114: the local bytes of a colliding new file), so an
    // existing name is never reused: a counter makes the name unique.
    let mut attempt = 1u32;
    let dest = loop {
        let tag = if attempt == 1 {
            ts.to_string()
        } else {
            format!("{ts} {attempt}")
        };
        let name = match &ext {
            Some(ext) => format!("{} (conflicted copy {}).{}", stem, tag, ext),
            None => format!("{} (conflicted copy {})", stem, tag),
        };
        let candidate = parent.join(name);
        if fs::symlink_metadata(&candidate).is_err() {
            break candidate;
        }
        attempt += 1;
    };
    Ok(dest)
}

/// Copies `path` to a fresh conflicted-copy name (see [`unique_conflicted_name`]); an existing
/// file is never overwritten.
pub(crate) fn create_conflicted_copy(path: &Path) -> AppResult<PathBuf> {
    let dest = unique_conflicted_name(path)?;
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

    #[cfg(not(target_os = "macos"))]
    use super::to_canonical_form;
    use super::{
        backoff_seconds, hash_file, is_builtin_ignored_local_path, is_dehydrated_placeholder,
        is_ignored_local_path, matches_ignore_globs, normalize_dropbox_path, relpath_under,
        safe_join, set_user_ignore_globs, should_ignore_local_path, validate_relative,
        DROPBOX_BLOCK_SIZE,
    };
    use super::{fold, is_canonical_form, key_from_disk, key_from_dropbox};
    use super::{is_path_allowed, parse_ignore_globs_csv, parse_prefix_csv, path_scope, PathScope};

    #[test]
    fn a_conflicted_copy_never_overwrites_an_existing_one() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        std::fs::write(&a, b"first").unwrap();
        let c1 = super::create_conflicted_copy(&a).unwrap();
        std::fs::write(&a, b"second").unwrap();
        let c2 = super::create_conflicted_copy(&a).unwrap();
        std::fs::write(&a, b"third").unwrap();
        let c3 = super::create_conflicted_copy(&a).unwrap();
        // Three copies made in (almost certainly) the same second: three distinct files.
        let mut names = vec![c1.clone(), c2.clone(), c3.clone()];
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 3);
        assert_eq!(std::fs::read(&c1).unwrap(), b"first");
        assert_eq!(std::fs::read(&c2).unwrap(), b"second");
        assert_eq!(std::fs::read(&c3).unwrap(), b"third");
        assert!(c1.to_string_lossy().ends_with(".txt"));
    }

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
            hash, "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
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
        assert_eq!(
            normalize_dropbox_path("Cocina/Pizza").unwrap(),
            "/Cocina/Pizza"
        );
        // A leading '/' is the Dropbox root convention and must be preserved.
        assert_eq!(normalize_dropbox_path("/Cocina").unwrap(), "/Cocina");
        // Windows separators are canonicalised to '/' — on Windows. On Unix (DBSYNC-104)
        // a backslash is a legal filename byte, so `Cocina\Pizza` is ONE name and folding
        // it would alias it onto the genuine `Cocina/Pizza` above.
        assert_eq!(
            normalize_dropbox_path("Cocina\\Pizza").unwrap(),
            if cfg!(windows) {
                "/Cocina/Pizza"
            } else {
                "/Cocina\\Pizza"
            }
        );
    }

    #[test]
    fn normalize_rejects_traversal_payloads() {
        // Rejected on every platform: traversal and NUL are never a filename.
        for bad in [
            "../etc/passwd",
            "Cocina/../../secret",
            "a/../../b",
            "with\0null",
        ] {
            assert!(
                normalize_dropbox_path(bad).is_err(),
                "expected rejection for {bad:?}"
            );
        }

        // Drive and UNC prefixes are a Windows concept (DBSYNC-104). There they can
        // replace the base in `PathBuf::push`; on Unix they are ordinary filename bytes
        // that `Path::join` keeps under the root.
        // `..\Windows\System32` is a traversal on Windows and ONE legal filename on Unix
        // (DBSYNC-104). Grouped with the drive shapes below for the same reason.
        for drive_shaped in [
            "C:\\Windows",
            "C:/Windows",
            "\\\\server\\share",
            "..\\Windows\\System32",
        ] {
            let got = normalize_dropbox_path(drive_shaped);
            if cfg!(windows) {
                assert!(got.is_err(), "expected rejection for {drive_shaped:?}");
            } else {
                assert!(
                    got.is_ok(),
                    "{drive_shaped:?} is a legal Unix name and must not be rejected"
                );
            }
        }
    }

    /// DBSYNC-104. `C:notas.txt` and `D:2024/informe.pdf` are legal, unremarkable macOS
    /// filenames. They used to be rejected as Windows drive-absolute, and because the
    /// rejection propagated out of the remote sweep loop, one such file stopped syncing
    /// for every other file, indefinitely.
    #[cfg(not(windows))]
    #[test]
    fn drive_letter_lookalikes_are_ordinary_unix_filenames() {
        assert_eq!(
            normalize_dropbox_path("C:notas.txt").expect("legal macOS name"),
            "/C:notas.txt"
        );
        assert_eq!(
            normalize_dropbox_path("D:2024/informe.pdf").expect("legal macOS name"),
            "/D:2024/informe.pdf"
        );
        assert!(validate_relative("C:notas.txt").is_ok());
        assert!(validate_relative("D:2024/informe.pdf").is_ok());

        // And they still resolve under the root rather than escaping it.
        let root = Path::new("/sync/root");
        let joined = safe_join(root, "C:notas.txt").expect("stays under root");
        assert!(joined.starts_with(root));
    }

    #[test]
    fn validate_relative_rejects_absolute_and_traversal() {
        assert!(validate_relative("Cocina/Pizza").is_ok());
        assert!(validate_relative("a/b/c.txt").is_ok());

        // Rejected on every platform. `\\unc\\x` and `\\Windows` stay rejected here even
        // on Unix because `validate_relative` also refuses a leading separator outright,
        // and that check is deliberately NOT platform-gated.
        for bad in ["..", "../x", "a/../../b", "/etc/passwd", "x\0y"] {
            assert!(
                validate_relative(bad).is_err(),
                "expected rejection for {bad:?}"
            );
        }

        // Windows-only: a drive prefix, a UNC prefix, and a leading `\` root marker. On
        // Unix each of these is one legal filename, and `safe_join` — not a rejection here
        // — is what keeps them under the root (DBSYNC-104).
        for windows_only in ["C:\\x", "\\Windows", "\\\\unc\\x"] {
            assert_eq!(
                validate_relative(windows_only).is_err(),
                cfg!(windows),
                "{windows_only:?} must be refused on Windows and accepted on Unix"
            );
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
        for bad in ["../outside", "a/../../b", "/abs/path", "..", "x\0y"] {
            assert!(
                safe_join(root, bad).is_err(),
                "safe_join must refuse {bad:?}"
            );
        }

        // A remote-derived child name carrying embedded backslashes (DBSYNC-27 review
        // finding #1: the `.cloudsc` placeholder write sink). On Windows these are
        // traversals and must be refused. On Unix each is ONE legal filename
        // (DBSYNC-104) — so the safety property is not "refused" but "cannot escape",
        // which is what `safe_join` actually guarantees. Assert that directly.
        for backslashed in ["..\\..\\evil.cloudsc", "sub\\..\\..\\evil"] {
            let joined = safe_join(root, backslashed);
            assert_eq!(
                joined.is_err(),
                cfg!(windows),
                "{backslashed:?} must be refused on Windows and accepted on Unix"
            );
            if let Ok(joined) = joined {
                assert!(
                    joined.starts_with(root),
                    "{backslashed:?} joined outside the root: {joined:?}"
                );
                assert_eq!(
                    joined.components().count(),
                    root.components().count() + 1,
                    "{backslashed:?} must be exactly one component under the root"
                );
            }
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
        assert!(
            mtime > 0,
            "modified timestamp must be non-zero for a newly created file"
        );
    }

    /// DBSYNC-104. A root file literally named `a\b.txt` and the genuine `a/b.txt`
    /// inside folder `a` are two different files with two different contents. Before
    /// the fix, `relpath_under` rewrote every `\` to `/` on every platform, so both
    /// produced the key `a/b.txt` — one row in a `TEXT PRIMARY KEY`, each file
    /// re-uploading over the other, and a recursive `delete_v2` of `/a` destroying the
    /// remote copy of the backslash-named one.
    ///
    /// `\` is a legal byte in a macOS filename and cannot occur in a Windows one, so
    /// the rewrite belongs behind `#[cfg(windows)]` and nowhere else.
    #[cfg(not(windows))]
    #[test]
    fn a_backslash_in_a_name_is_not_a_separator_on_unix() {
        let root = Path::new("/sync/root");

        let weird = relpath_under(root, Path::new("/sync/root/a\\b.txt")).expect("relpath");
        let genuine = relpath_under(root, Path::new("/sync/root/a/b.txt")).expect("relpath");

        assert_eq!(
            weird, "a\\b.txt",
            "the backslash must survive as part of the name"
        );
        assert_eq!(genuine, "a/b.txt");
        assert_ne!(
            weird, genuine,
            "two distinct files must not collapse onto one index key"
        );
    }

    /// Regression guard on the common case: gating the rewrite must not stop ordinary
    /// nested paths from producing `/`-separated keys, which every consumer relies on.
    #[test]
    fn ordinary_nested_paths_still_produce_forward_slash_keys() {
        let root = Path::new("/sync/root");

        assert_eq!(
            relpath_under(root, Path::new("/sync/root/Cocina/Pizza.txt")).expect("relpath"),
            "Cocina/Pizza.txt"
        );
        assert_eq!(
            relpath_under(root, Path::new("/sync/root/a/b/c/d.txt")).expect("relpath"),
            "a/b/c/d.txt"
        );
        // The sync root itself is the empty key, as several callers depend on.
        assert_eq!(relpath_under(root, root).expect("relpath"), "");
    }

    /// A path outside the root is an error, not a silently-wrong key. Every call site
    /// converted in DBSYNC-104 relies on this staying an `Err` so its own
    /// skip-or-propagate choice keeps working.
    #[test]
    fn a_path_outside_the_root_is_an_error() {
        assert!(relpath_under(Path::new("/sync/root"), Path::new("/etc/passwd")).is_err());
    }

    /// DBSYNC-104. Each of these predicates folded `\` to `/` before matching, so on
    /// macOS — where `\` is a legal filename byte — a name containing one was matched
    /// as though it were a nested path. The consequence is a file that is silently
    /// never synced, or one that matches an ignore rule meant for something else.
    #[cfg(not(windows))]
    #[test]
    fn matching_predicates_do_not_treat_a_backslash_as_a_separator() {
        // A root file genuinely named `a\.DS_Store` is not `.DS_Store`.
        assert!(
            !should_ignore_local_path("a\\.DS_Store"),
            "a legal macOS filename must not be matched as OS junk"
        );
        assert!(should_ignore_local_path(".DS_Store"));
        assert!(should_ignore_local_path("sub/.DS_Store"));

        // Same for `._` resource forks: `a\._x` is one name, not `a/._x`.
        assert!(!should_ignore_local_path("a\\._x"));
        assert!(should_ignore_local_path("a/._x"));

        // A user glob naming a nested path must not catch a root file whose name
        // merely contains a backslash.
        let globs = vec!["Notes/scratch.txt".to_string()];
        assert!(!matches_ignore_globs("Notes\\scratch.txt", &globs));
        assert!(matches_ignore_globs("Notes/scratch.txt", &globs));

        // The basename form must not be fooled either: the basename of `a\b.log` is
        // the whole name, not `b.log`.
        let by_name = vec!["b.log".to_string()];
        assert!(!matches_ignore_globs("a\\b.log", &by_name));
        assert!(matches_ignore_globs("a/b.log", &by_name));
    }

    // -------------------------------------------------------------------------------
    // DBSYNC-103: key constructors and the one comparison fold.
    // -------------------------------------------------------------------------------
    const NFD: &str = "cafe\u{301}.txt";
    const NFC: &str = "caf\u{e9}.txt";

    #[cfg(target_os = "macos")]
    #[test]
    fn key_from_disk_yields_nfc_for_an_nfd_name() {
        let root = Path::new("/sync/root");
        let abs = root.join("Dir").join(NFD);
        assert_eq!(
            key_from_disk(root, &abs).expect("key"),
            format!("Dir/{NFC}")
        );
        // Already-NFC input and the root itself are untouched.
        assert_eq!(key_from_disk(root, &root.join(NFC)).expect("key"), NFC);
        assert_eq!(key_from_disk(root, root).expect("key"), "");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn key_from_dropbox_trims_the_slash_and_yields_nfc() {
        assert_eq!(
            key_from_dropbox(&format!("/Dir/{NFD}")),
            format!("Dir/{NFC}")
        );
        assert_eq!(
            key_from_dropbox(&format!("/Dir/{NFC}")),
            format!("Dir/{NFC}")
        );
        assert_eq!(key_from_dropbox("/plain.txt"), "plain.txt");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn fold_equates_nfd_nfc_and_case_but_not_different_names() {
        assert_eq!(fold(NFD), fold(NFC));
        assert_eq!(fold(&NFC.to_uppercase()), fold(NFD));
        assert_ne!(fold("cafe.txt"), fold(NFC), "e and é are different names");
        assert!(is_canonical_form(NFC));
        assert!(!is_canonical_form(NFD));
    }

    /// The Windows leg: nothing is normalized, so NFC and NFD stay distinct keys and
    /// behaviour is byte-for-byte what it was before DBSYNC-103 (NTFS preserves the form it
    /// was given). Gated to the platforms where it is the contract.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn off_macos_no_helper_merges_distinct_unicode_forms() {
        assert_ne!(to_canonical_form(NFD), to_canonical_form(NFC));
        assert_eq!(to_canonical_form(NFD), NFD);
        assert_ne!(
            key_from_dropbox(&format!("/{NFD}")),
            key_from_dropbox(&format!("/{NFC}"))
        );
        assert_eq!(key_from_dropbox(&format!("/{NFD}")), NFD);
        assert_ne!(fold(NFD), fold(NFC));
        // `fold` is exactly the old plain lowercase.
        assert_eq!(fold("ÉA.TXT"), "ÉA.TXT".to_lowercase());
        assert!(is_canonical_form(NFD));
        let root = Path::new("/sync/root");
        assert_ne!(
            key_from_disk(root, &root.join(NFD)).unwrap(),
            key_from_disk(root, &root.join(NFC)).unwrap()
        );
    }

    // -------------------------------------------------------------------------------
    // DBSYNC-103 #188: the user's typed prefixes and globs are canonicalised when parsed.
    // The parse tests assert the parsed output only and the match tests feed the predicate
    // already-canonical patterns, so each guard is the only thing its own test depends on.
    // -------------------------------------------------------------------------------

    #[cfg(target_os = "macos")]
    #[test]
    fn parse_prefix_csv_canonicalises_each_prefix_and_still_strips_the_slash() {
        let csv = format!("/{NFD}dir, plain ,,/{NFC}b");
        assert_eq!(
            parse_prefix_csv(Some(csv)),
            vec![format!("{NFC}dir"), "plain".to_string(), format!("{NFC}b")]
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn parse_ignore_globs_csv_canonicalises_each_pattern_and_keeps_a_leading_slash() {
        let csv = format!("{NFD}, *{NFD}, /abs/{NFD} ,other");
        assert_eq!(
            parse_ignore_globs_csv(Some(csv)),
            vec![
                NFC.to_string(),
                format!("*{NFC}"),
                format!("/abs/{NFC}"),
                "other".to_string()
            ]
        );
    }

    /// An NFC key against a prefix list the user typed in NFD, end to end through the parser.
    #[cfg(target_os = "macos")]
    #[test]
    fn an_nfd_typed_exclude_prefix_excludes_the_nfc_key() {
        let exclude = parse_prefix_csv(Some(format!("{NFD}dir")));
        assert!(!is_path_allowed(&format!("{NFC}dir/a.txt"), &[], &exclude));
        assert!(is_path_allowed("other/a.txt", &[], &exclude));
        let include = parse_prefix_csv(Some(format!("{NFD}dir")));
        assert!(is_path_allowed(&format!("{NFC}dir/a.txt"), &include, &[]));
    }

    /// Vice versa: an NFC prefix still matches a key that arrives in NFD.
    #[cfg(target_os = "macos")]
    #[test]
    fn is_path_allowed_compares_an_nfd_key_against_canonical_prefixes() {
        let prefixes = vec![format!("{NFC}dir")];
        assert!(!is_path_allowed(&format!("{NFD}dir/a.txt"), &[], &prefixes));
        assert!(is_path_allowed(&format!("{NFD}dir/a.txt"), &prefixes, &[]));
        assert!(is_path_allowed("other/a.txt", &[], &prefixes));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_nfd_typed_glob_ignores_the_nfc_key() {
        let globs = parse_ignore_globs_csv(Some(format!("{NFD}, *{NFD}x, dir/{NFD}")));
        assert!(matches_ignore_globs(NFC, &globs), "basename form");
        assert!(matches_ignore_globs(&format!("sub/{NFC}"), &globs));
        assert!(matches_ignore_globs(&format!("a{NFC}x"), &globs), "suffix");
        assert!(matches_ignore_globs(&format!("dir/{NFC}"), &globs), "path");
        assert!(!matches_ignore_globs("cafe.txt", &globs));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn matches_ignore_globs_compares_an_nfd_key_against_canonical_patterns() {
        let globs = vec![NFC.to_string(), format!("*{NFC}x"), format!("dir/{NFC}")];
        assert!(matches_ignore_globs(NFD, &globs), "basename form");
        assert!(matches_ignore_globs(&format!("a{NFD}x"), &globs), "suffix");
        assert!(matches_ignore_globs(&format!("dir/{NFD}"), &globs), "path");
        assert!(!matches_ignore_globs("cafe.txt", &globs));
    }

    /// Off macOS nothing changes: the parsers return the bytes typed and the predicates
    /// keep NFC and NFD apart, exactly as before DBSYNC-103.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn off_macos_prefix_and_glob_parsing_is_unchanged() {
        assert_eq!(
            parse_prefix_csv(Some(format!("/{NFD}dir, b"))),
            vec![format!("{NFD}dir"), "b".to_string()]
        );
        assert_eq!(
            parse_ignore_globs_csv(Some(format!("{NFD}, *.log"))),
            vec![NFD.to_string(), "*.log".to_string()]
        );
        assert!(is_path_allowed(
            &format!("{NFC}dir/a"),
            &[],
            &[format!("{NFD}dir")]
        ));
        assert!(!matches_ignore_globs(NFC, &[NFD.to_string()]));
    }

    // -------------------------------------------------------------------------------
    // DBSYNC-114 slice 1: one folder-boundary, case-insensitive scope predicate.
    // -------------------------------------------------------------------------------

    fn csv(s: &str) -> Vec<String> {
        parse_prefix_csv(Some(s.to_string()))
    }

    #[test]
    fn parse_prefix_csv_strips_slashes_before_dropping_empties() {
        assert!(csv("/").is_empty(), "a lone slash is no prefix (H8)");
        assert!(csv("//, ,/").is_empty());
        assert!(csv("").is_empty());
        assert!(parse_prefix_csv(None).is_empty());
        assert_eq!(csv("/a/,b//, ,,c"), vec!["a", "b", "c"]);
    }

    /// `.` and `..` name no folder, so they are no entry (an include of `.` would put every
    /// real path outside the include list and remove all local copies).
    #[test]
    fn parse_prefix_csv_drops_dot_and_dotdot_entries() {
        assert!(csv(".").is_empty());
        assert!(csv("./, .., /../").is_empty());
        assert_eq!(csv("a,.,b,..,c"), vec!["a", "b", "c"]);
        assert_eq!(csv("./x"), vec!["./x"], "only the bare entry is dropped");
    }

    #[test]
    fn path_scope_table() {
        use PathScope::{Ancestor, Excluded, Synced};
        // (include csv, exclude csv, path, expected)
        let cases: &[(&str, &str, &str, PathScope)] = &[
            // empty lists: everything
            ("", "", "anything/at/all", Synced),
            // boundary
            ("", "Fotos", "Fotos", Excluded),
            ("", "Fotos", "Fotos/x", Excluded),
            ("", "Fotos", "Fotos2024", Synced),
            ("", "Fotos", "Fotos2024/x", Synced),
            ("", "Fotos", "Otra/Fotos", Synced),
            ("Fotos", "", "Fotos/x", Synced),
            ("Fotos", "", "Fotos", Synced),
            ("Fotos", "", "Fotos2024", Excluded),
            // case
            ("", "fotos", "Fotos/x", Excluded),
            ("", "FOTOS", "fotos/x", Excluded),
            ("fotos", "", "FOTOS/x", Synced),
            // leading / trailing slash on either side
            ("", "/Fotos/", "Fotos/x", Excluded),
            ("", "Fotos/", "Fotos", Excluded),
            ("/Fotos/", "", "Fotos/x", Synced),
            ("", "Fotos", "/Fotos/x", Excluded),
            // "/" excludes nothing
            ("", "/", "Fotos/x", Synced),
            ("", "/", "x", Synced),
            // empty CSV entries
            ("", ",, ,Fotos,,", "Fotos/x", Excluded),
            ("", ",, ,Fotos,,", "Docs/x", Synced),
            (",,", "", "Docs/x", Synced),
            // exclude wins over include
            ("Fotos", "Fotos", "Fotos/x", Excluded),
            ("Fotos", "Fotos/Privado", "Fotos/Privado/a", Excluded),
            ("Fotos", "Fotos/Privado", "Fotos/Publico/a", Synced),
            ("Fotos", "fotos", "Fotos", Excluded),
            // include only
            ("Fotos", "", "Docs/x", Excluded),
            // ancestor of a nested include
            ("Fotos/2024", "", "Fotos", Ancestor),
            ("Fotos/2024", "", "fotos", Ancestor),
            ("Fotos/2024", "", "Fotos/2024/a", Synced),
            ("Fotos/2024", "", "Fotos/2024", Synced),
            ("Fotos/2024", "", "Fotos/2023/a", Excluded),
            ("Fotos/2024", "", "Fotos/20", Excluded),
            ("Fotos/2024", "", "Fotos2", Excluded),
            ("Fotos/2024", "", "Otra/a", Excluded),
            // an excluded ancestor is Excluded, not Ancestor
            ("Fotos/2024", "Fotos", "Fotos", Excluded),
            // several includes, deeper nesting
            ("A/B/C, D", "", "A", Ancestor),
            ("A/B/C, D", "", "A/B", Ancestor),
            ("A/B/C, D", "", "A/B/C/x", Synced),
            ("A/B/C, D", "", "D/x", Synced),
            ("A/B/C, D", "", "A/X", Excluded),
            // include inside an include
            ("A, A/B", "", "A/Z", Synced),
            // the root (empty path) is an ancestor of any include entry
            ("Fotos/2024", "", "", Ancestor),
        ];
        for (inc, exc, path, want) in cases {
            assert_eq!(
                path_scope(path, &csv(inc), &csv(exc)),
                *want,
                "include={inc:?} exclude={exc:?} path={path:?}"
            );
            assert_eq!(
                is_path_allowed(path, &csv(inc), &csv(exc)),
                *want == Synced,
                "is_path_allowed include={inc:?} exclude={exc:?} path={path:?}"
            );
        }
    }

    /// The predicate must not depend on the caller having parsed the lists.
    /// `excluded_beneath`: only a Synced folder with an exclude strictly under it.
    #[test]
    fn excluded_beneath_table() {
        let f = |inc: &[&str], exc: &[&str]| {
            super::PrefixFilter::new(
                &inc.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                &exc.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            )
        };
        let ex = f(&[], &["Docs/Archive"]);
        assert!(ex.excluded_beneath("Docs"));
        assert!(ex.excluded_beneath("docs/"), "case and slash");
        assert!(ex.excluded_beneath(""), "the sync root");
        assert!(
            !ex.excluded_beneath("Docs/Archive"),
            "the excluded folder itself"
        );
        assert!(
            !ex.excluded_beneath("Docs/Archive/x"),
            "beneath the excluded one"
        );
        assert!(!ex.excluded_beneath("Docs/Other"));
        assert!(!ex.excluded_beneath("Docs2"), "folder boundary");
        assert!(
            !f(&[], &["Docs2/x"]).excluded_beneath("Docs"),
            "folder boundary, other way"
        );
        assert!(!f(&[], &[]).excluded_beneath("Docs"), "inactive");
        assert!(
            !f(&["Fotos"], &[]).excluded_beneath("Fotos"),
            "an include has no excluded beneath"
        );
        assert!(
            !f(&["Fotos/2024"], &[]).excluded_beneath("Fotos"),
            "Ancestor is not deletable anyway"
        );
        assert!(f(&["Fotos"], &["Fotos/Priv"]).excluded_beneath("Fotos"));
    }

    #[test]
    fn path_scope_tolerates_unparsed_prefixes() {
        let raw = |s: &str| vec![s.to_string()];
        assert_eq!(
            path_scope("Fotos/x", &[], &raw("/FOTOS/")),
            PathScope::Excluded
        );
        assert_eq!(path_scope("Fotos/x", &[], &raw("/")), PathScope::Synced);
        assert_eq!(path_scope("Fotos/x", &[], &raw("")), PathScope::Synced);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn path_scope_folds_nfc_and_nfd_on_both_sides() {
        let nfd_dir = format!("{NFD}dir");
        let nfc_dir = format!("{NFC}dir");
        assert_eq!(
            path_scope(&format!("{nfc_dir}/a"), &[], std::slice::from_ref(&nfd_dir)),
            PathScope::Excluded
        );
        assert_eq!(
            path_scope(&format!("{nfd_dir}/a"), &[], std::slice::from_ref(&nfc_dir)),
            PathScope::Excluded
        );
        assert_eq!(
            path_scope(
                &format!("{}dir/a", NFD.to_uppercase()),
                std::slice::from_ref(&nfc_dir),
                &[]
            ),
            PathScope::Synced
        );
        assert_eq!(
            path_scope(&nfd_dir, &[format!("{nfc_dir}/sub")], &[]),
            PathScope::Ancestor
        );
        assert_eq!(
            path_scope(&format!("{nfc_dir}x/a"), &[nfd_dir], &[]),
            PathScope::Excluded,
            "boundary holds across normalisation forms"
        );
    }
}
