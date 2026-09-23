use crate::error::{AppError, AppResult};
use chrono::Utc;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::Serialize;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileIndexRow {
    pub relative_path: String,
    pub hash: String,
    pub size_bytes: i64,
    pub modified_ts: i64,
    /// Locally allocated, permanent identity for this item (DBSYNC-99).
    ///
    /// Dropbox cannot name an item it has never seen, and the gap between a local
    /// creation and a successful upload is unbounded when uploads keep failing — so this
    /// is not a mirror of Dropbox's id. It is a monotonic integer minted at first index
    /// and **never derived from the path**: Apple's SDK header warns an identifier may be
    /// recorded in system logs, and a path is user data.
    ///
    /// `Option` covers rows written before the column existed; they are back-filled the
    /// next time the path is indexed.
    pub item_id: Option<i64>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteFileIndexRow {
    pub relative_path: String,
    pub content_hash: String,
    pub rev: String,
    pub modified_ts: i64,
    /// Dropbox's stable identifier, e.g. `id:eTyPGjL6NDAAAAAAAAABwg` (DBSYNC-99).
    /// Joined to the local row's `item_id` by `relative_path`; slice 3 rewrites both
    /// sides together on a move, so the join survives a rename.
    pub dropbox_id: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncJobRow {
    pub id: i64,
    pub job_type: String,
    pub source_path: Option<String>,
    pub target_path: Option<String>,
    pub status: String,
    pub attempt_count: i64,
    pub next_retry_at: Option<String>,
    pub updated_at: String,
    pub last_error: Option<String>,
    pub delete_parent_rev: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictRow {
    pub id: i64,
    pub local_path: String,
    pub remote_path: String,
    pub reason: String,
    /// Sibling copy holding the *local* content the auto-resolve preserved
    /// (`<name> (conflicted copy <ts>).<ext>`), relative to the sync root. `None`
    /// for the remote-deleted scenario, where only the local primary survives.
    pub conflicted_copy_path: Option<String>,
    /// True when the conflict is "remote deleted while local diverged" — there is
    /// no remote content to fall back to, so `Use Remote` means discarding local.
    pub remote_deleted: bool,
    pub created_at: String,
}

/// Separate read/write connections plus WAL so the UI can query without blocking on sync writes.
pub struct Db {
    write: Mutex<Connection>,
    read: Mutex<Connection>,
    /// The directory this database lives in, and therefore the directory every other
    /// per-instance artefact belongs in (DBSYNC-75).
    ///
    /// Recorded so that writers of sibling files do not have to reach for the global
    /// [`app_data_dir`]. `overlay_state.json` did exactly that, which meant `cargo test` —
    /// whose `AppState` is built against a `tempdir()` — overwrote the **running user's**
    /// real overlay file with a sync folder that the test then deleted. The Finder Sync
    /// extension reads that file every two seconds, so the user's badges stopped rendering
    /// until it was repaired by hand.
    ///
    /// `new_at`'s own doc already promised that "running `cargo test` never touches a
    /// user's real DB". This extends the same promise to everything written beside it.
    data_dir: PathBuf,
}

/// Debug-only contract check: every index key reaching the storage layer is already
/// `/`-canonical.
///
/// Local keys get that from [`crate::path_util::relpath_under`], the single local
/// producer. Remote keys come from Dropbox `path_display` stripped of its leading `/`,
/// which is `/`-canonical by construction and never passes through `relpath_under`
/// (review L1 corrected an earlier comment that claimed one source for both).
///
/// This layer used to rewrite `\` to `/` itself, in twelve accessors (DBSYNC-45) — a
/// second, independent normalization boundary. On macOS, where `\` is a legal byte in
/// a filename, that silently merged two distinct files onto one `TEXT PRIMARY KEY`:
/// a root file named `a\b.txt` and the genuine `a/b.txt` inside folder `a` shared a
/// row, each re-uploaded over the other, and deleting folder `a` destroyed the remote
/// copy of the other file. Gating those twelve rewrites would have left the next
/// missed one re-aliasing in silence, so the rewrites are gone and the invariant moved
/// to the producer (DBSYNC-104).
///
/// Windows-only: there `\` cannot occur in a name, so its presence means a caller
/// bypassed `relpath_under`. On Unix a backslash is legitimate data and passes through.
///
/// `debug_assert!` compiles out in release. This is a development guard that makes a
/// bypassing caller fail loudly in tests and dev builds — not a runtime enforcement.
///
/// And it is a **no-op on Unix**, where a backslash is legitimate data: it only has teeth
/// in the `rust (windows-latest)` CI job, not on the macOS dev machine (review round 2).
/// It earned that keep immediately — it caught three of this branch's own tests on its
/// first Windows run.
///
/// `enqueue_job`, `enqueue_delete_job` and `record_refused_move` also take index keys and
/// are not asserted. Pre-existing, and left as it is rather than widened silently.
#[inline]
fn debug_assert_canonical_key(_relative_path: &str) {
    #[cfg(windows)]
    debug_assert!(
        !_relative_path.contains('\\'),
        "index key {_relative_path:?} is not '/'-canonical: \
         build it with path_util::relpath_under"
    );
}

impl Db {
    pub fn new() -> AppResult<Self> {
        Self::new_at(&db_path()?)
    }

    /// Open a database at an explicit path. Used by tests to stay fully isolated
    /// from the production database (which `db_path()` resolves via OS-specific
    /// app-data dirs), so running `cargo test` never touches a user's real DB.
    pub fn new_at(path: &std::path::Path) -> AppResult<Self> {
        let mut write = Connection::open(path)?;
        write.execute_batch(
            "
                PRAGMA foreign_keys = ON;
                PRAGMA journal_mode = WAL;
                PRAGMA synchronous = NORMAL;
                ",
        )?;
        migrate(&mut write)?;

        let read = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;

        Ok(Self {
            write: Mutex::new(write),
            read: Mutex::new(read),
            // `parent()` is not the same as "the directory it is in": for a bare filename
            // it returns `Some("")`, not `None` (measured, not assumed). An empty data dir
            // would silently place sibling files in the process's working directory, so the
            // empty case is folded in with the `None` case and both become `.` — the same
            // directory, said out loud. Neither is reachable from the two call sites, which
            // pass absolute paths; this exists so that a future third one cannot be subtly
            // wrong.
            data_dir: match path.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
                _ => PathBuf::from("."),
            },
        })
    }

    /// The directory this database lives in. See the field's documentation for why sibling
    /// files must be resolved from here rather than from [`app_data_dir`] (DBSYNC-75).
    pub(crate) fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
    }

    pub fn set_sync_folder(&self, folder: &str) -> AppResult<()> {
        let now = Utc::now().to_rfc3339();
        let conn = self
            .write
            .lock()
            .map_err(|_| AppError::Storage("db write lock poisoned".into()))?;
        conn.execute(
            "
                INSERT INTO app_config (key, value, updated_at)
                VALUES ('sync_folder', ?1, ?2)
                ON CONFLICT(key) DO UPDATE SET
                  value=excluded.value,
                  updated_at=excluded.updated_at
                ",
            params![folder, now],
        )?;
        Ok(())
    }

    /// Clears every trace of the previous sync folder, **atomically** (DBSYNC-40).
    ///
    /// The transaction is the point. These six deletions used to run as six independent
    /// statements, and a crash, lock error or disk failure between any two left a state
    /// neither half of the sync engine expects: clear `local_file_index` but not
    /// `remote_file_index`, and the next scan walks a folder full of files with no index
    /// rows while the remote index still claims to know them. This is not a hypothetical
    /// path — it runs whenever the user changes their sync folder — and on a client that
    /// carries a mass-delete circuit breaker because bulk operations here destroy data, a
    /// half-cleared index is not a tidiness problem.
    ///
    /// `rusqlite::Transaction` rather than literal `BEGIN`/`COMMIT`: it rolls back when
    /// dropped, so an early `?` return between the statements cannot leave a transaction
    /// open — which would be this function's own failure mode, one level up.
    ///
    /// **The atomic boundary is this function, not the folder switch.** `commands.rs` sets
    /// the new sync folder in its own transaction and *then* calls this one, so a failure
    /// here still leaves `app_config` pointing at the new folder while the index describes
    /// the old one. That is better than the torn index this replaces — coherent-but-stale
    /// beats half-cleared — but it is not the same as the whole operation being atomic, and
    /// this comment should not be read as claiming it is. Closing it means one method doing
    /// the config write and these deletions together: **DBSYNC-94**.
    pub fn reset_sync_state(&self) -> AppResult<()> {
        let mut conn = self
            .write
            .lock()
            .map_err(|_| AppError::Storage("db write lock poisoned".into()))?;
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM local_file_index", [])?;
        tx.execute("DELETE FROM remote_file_index", [])?;
        tx.execute("DELETE FROM sync_jobs", [])?;
        // DBSYNC-99's table. A surviving refusal names relative paths, so after a folder
        // change a colliding pair suppresses a legitimate rename correlation in the NEW
        // folder. Added here when the table was added; the next table needs the same line.
        tx.execute("DELETE FROM refused_moves", [])?;
        tx.execute("DELETE FROM sync_conflicts", [])?;
        tx.execute("DELETE FROM known_folders", [])?;
        // Drop the cursor-delta cursor so remote change detection re-seeds
        // against the new folder (DBSYNC-30); other app_config keys are kept.
        tx.execute(
            "DELETE FROM app_config WHERE key = 'remote_delta_cursor'",
            [],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Records that `relative_path` is a currently-materialized (real, on-disk)
    /// folder under the sync root, so a later scan can detect it being deleted
    /// locally even though folders themselves have no content to diff.
    ///
    /// Carries no Dropbox identifier: on the path this is called from the folder exists
    /// on disk and Dropbox has not named it yet. The identifier is filled in later by
    /// [`Self::set_known_folder_dropbox_id`].
    pub fn upsert_known_folder(&self, relative_path: &str) -> AppResult<()> {
        debug_assert_canonical_key(relative_path);
        let now = Utc::now().to_rfc3339();
        let conn = self
            .write
            .lock()
            .map_err(|_| AppError::Storage("db write lock poisoned".into()))?;
        conn.execute(
            "
                INSERT INTO known_folders(relative_path, updated_at)
                VALUES(?1, ?2)
                ON CONFLICT(relative_path) DO UPDATE SET
                  updated_at=excluded.updated_at
                ",
            params![relative_path, now],
        )?;
        Ok(())
    }

    pub fn list_known_folders(&self) -> AppResult<Vec<String>> {
        let conn = self
            .read
            .lock()
            .map_err(|_| AppError::Storage("db read lock poisoned".into()))?;
        let mut stmt =
            conn.prepare("SELECT relative_path FROM known_folders ORDER BY relative_path")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Fill in a folder's Dropbox identifier without disturbing anything else (DBSYNC-99).
    ///
    /// Deliberately an UPDATE and not an upsert. On macOS `known_folders` is populated
    /// entirely from local discovery, where Dropbox has not named the folder yet, so the
    /// identifier arrives later from the remote sweep; inserting there instead would
    /// create rows for remote folders that do not exist locally, and `known_folders`
    /// drives local deletion detection. The `dropbox_id IS NULL` guard makes it
    /// idempotent and keeps a stored identifier authoritative.
    ///
    /// Returns whether a row was updated, so a caller can tell "filled it in" from
    /// "no such folder locally" — which is not an error.
    ///
    /// # Written, and deliberately not read yet
    ///
    /// **Nothing in the crate reads `known_folders.dropbox_id`.** That is on purpose, not an
    /// oversight, and it is recorded here so the next reader does not have to guess
    /// (DBSYNC-106). Captured as groundwork for:
    ///
    /// - **DBSYNC-95** (macOS File Provider), which addresses items by stable identifier.
    ///   ADR-0003 already counts capturing the id as work done.
    /// - **DBSYNC-102** (remote→local drift). A repair pass comparing recorded ids against
    ///   current paths has real value *in that direction*, where both sides have an id.
    ///
    /// It does **not** help local→remote rename correlation, and DBSYNC-106 considered and
    /// rejected it for that: correlation pairs a local path that vanished with a local path
    /// that appeared, and the appeared path has never existed on Dropbox, so there is no
    /// second id to match. `correlate_renames` documents the same thing for files.
    ///
    /// # Two coverage holes, for whoever writes the first reader
    ///
    /// A reader must not assume the column is populated:
    ///
    /// 1. **The delta path never fills it.** `delta_action_from_entry` ignores every folder
    ///    entry, so identifiers arrive only from the periodic full recursive sweep.
    /// 2. **A fresh install has none.** The sweep early-returns when `local_file_index` is
    ///    empty, so no folder gets an identifier until at least one file is indexed.
    pub fn set_known_folder_dropbox_id(
        &self,
        relative_path: &str,
        dropbox_id: &str,
    ) -> AppResult<bool> {
        debug_assert_canonical_key(relative_path);
        let conn = self
            .write
            .lock()
            .map_err(|_| AppError::Storage("db write lock poisoned".into()))?;
        let updated = conn.execute(
            "UPDATE known_folders SET dropbox_id = ?2 WHERE relative_path = ?1 AND dropbox_id IS NULL",
            params![relative_path, dropbox_id],
        )?;
        Ok(updated > 0)
    }

    pub fn remove_known_folder(&self, relative_path: &str) -> AppResult<()> {
        debug_assert_canonical_key(relative_path);
        let conn = self
            .write
            .lock()
            .map_err(|_| AppError::Storage("db write lock poisoned".into()))?;
        conn.execute(
            "DELETE FROM known_folders WHERE relative_path = ?1",
            params![relative_path],
        )?;
        Ok(())
    }

    /// Removes the `sync_folder` app_config key (DBSYNC-36 disconnect). Local
    /// preferences (selective-sync prefixes, ignore globs) are deliberately left
    /// untouched — only `reset_sync_state` + this together represent "sign out".
    pub fn clear_sync_folder(&self) -> AppResult<()> {
        let conn = self
            .write
            .lock()
            .map_err(|_| AppError::Storage("db write lock poisoned".into()))?;
        conn.execute("DELETE FROM app_config WHERE key = 'sync_folder'", [])?;
        Ok(())
    }

    pub fn get_sync_folder(&self) -> AppResult<Option<String>> {
        let conn = self
            .read
            .lock()
            .map_err(|_| AppError::Storage("db read lock poisoned".into()))?;
        let mut stmt =
            conn.prepare("SELECT value FROM app_config WHERE key = 'sync_folder' LIMIT 1")?;
        let mut rows = stmt.query([])?;
        if let Some(row) = rows.next()? {
            let value: String = row.get(0)?;
            return Ok(Some(value));
        }
        Ok(None)
    }

    pub fn set_app_config(&self, key: &str, value: &str) -> AppResult<()> {
        let now = Utc::now().to_rfc3339();
        let conn = self
            .write
            .lock()
            .map_err(|_| AppError::Storage("db write lock poisoned".into()))?;
        conn.execute(
            "
            INSERT INTO app_config (key, value, updated_at)
            VALUES(?1, ?2, ?3)
            ON CONFLICT(key) DO UPDATE SET
              value=excluded.value,
              updated_at=excluded.updated_at
            ",
            params![key, value, now],
        )?;
        Ok(())
    }

    pub fn get_app_config(&self, key: &str) -> AppResult<Option<String>> {
        let conn = self
            .read
            .lock()
            .map_err(|_| AppError::Storage("db read lock poisoned".into()))?;
        let mut stmt = conn.prepare("SELECT value FROM app_config WHERE key = ?1 LIMIT 1")?;
        let mut rows = stmt.query(params![key])?;
        if let Some(row) = rows.next()? {
            let value: String = row.get(0)?;
            return Ok(Some(value));
        }
        Ok(None)
    }

    // Selective sync (prefix-based). CSV of prefixes without leading '/' (e.g. "Fotos,Videos/2024").
    pub fn set_include_prefixes_csv(&self, csv: &str) -> AppResult<()> {
        self.set_app_config("include_prefixes_csv", csv)
    }

    pub fn get_include_prefixes_csv(&self) -> AppResult<Option<String>> {
        self.get_app_config("include_prefixes_csv")
    }

    pub fn set_exclude_prefixes_csv(&self, csv: &str) -> AppResult<()> {
        self.set_app_config("exclude_prefixes_csv", csv)
    }

    pub fn get_exclude_prefixes_csv(&self) -> AppResult<Option<String>> {
        self.get_app_config("exclude_prefixes_csv")
    }

    // User-defined local ignore globs (DBSYNC-36). CSV of basename / `*.ext` /
    // relative-path patterns (e.g. "Thumbs.db,*.log,Notes/scratch.txt").
    pub fn set_ignore_globs_csv(&self, csv: &str) -> AppResult<()> {
        self.set_app_config("ignore_globs_csv", csv)
    }

    pub fn get_ignore_globs_csv(&self) -> AppResult<Option<String>> {
        self.get_app_config("ignore_globs_csv")
    }

    pub fn list_local_files(&self) -> AppResult<Vec<FileIndexRow>> {
        let conn = self
            .read
            .lock()
            .map_err(|_| AppError::Storage("db read lock poisoned".into()))?;
        let mut stmt = conn
            .prepare(
                "SELECT relative_path, hash, size_bytes, modified_ts, item_id FROM local_file_index ORDER BY relative_path",
            )
            ?;

        let rows = stmt.query_map([], |row| {
            Ok(FileIndexRow {
                relative_path: row.get(0)?,
                hash: row.get(1)?,
                size_bytes: row.get(2)?,
                modified_ts: row.get(3)?,
                item_id: row.get(4)?,
            })
        })?;

        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn get_local_file(&self, relative_path: &str) -> AppResult<Option<FileIndexRow>> {
        // Canonicalize path separators to '/' so local (OS-native '\' on Windows)
        // and remote (Dropbox '/') keys match — DBSYNC-45.
        debug_assert_canonical_key(relative_path);
        let conn = self
            .read
            .lock()
            .map_err(|_| AppError::Storage("db read lock poisoned".into()))?;
        let mut stmt = conn
            .prepare(
                "SELECT relative_path, hash, size_bytes, modified_ts, item_id FROM local_file_index WHERE relative_path = ?1 LIMIT 1",
            )
            ?;
        let mut rows = stmt.query(params![relative_path])?;
        if let Some(row) = rows.next()? {
            return Ok(Some(FileIndexRow {
                relative_path: row.get(0)?,
                hash: row.get(1)?,
                size_bytes: row.get(2)?,
                modified_ts: row.get(3)?,
                item_id: row.get(4)?,
            }));
        }
        Ok(None)
    }

    /// A `local_file_index.hash` deliberately made unusable, meaning **"re-detect this
    /// path on the next scan"** (DBSYNC-56).
    ///
    /// Change detection asks `prev.hash != hash`, so a row carrying this value always
    /// compares as changed and the file is re-hashed and re-uploaded. It exists because an
    /// upload can be cancelled after the index row was already optimistically advanced: the
    /// file vanishes mid-flight, the job no-ops to avoid the phantom error DBSYNC-55 fixed,
    /// and the file returns byte-identical. Index and disk then agree on content the remote
    /// has never seen, and **nothing** re-detects it — the local scan compares index against
    /// disk, and `reconcile_remote_present` only fires when the remote moves.
    ///
    /// The empty string, rather than a new nullable column, because `hash` is `TEXT NOT
    /// NULL` and this project has no migration system yet (DBSYNC-40).
    ///
    /// **Why nothing collides with it, stated precisely** — a first version of this comment
    /// claimed "every value written here comes from `hash_file`", which is false:
    /// `cloudsc_ops::materialize_remote_only_file_if_absent` writes a hash taken from
    /// Dropbox's `content_hash`. It is safe anyway, but for a reason worth naming rather
    /// than assuming: `hash_file` returns hex and never `""`, even for a zero-byte file,
    /// and the remote-sourced path early-returns on an empty `content_hash` before it can
    /// reach this column. The `debug_assert!` in [`Self::upsert_local_file`] enforces that
    /// where it is claimed, so a future writer cannot quietly break it.
    ///
    /// **It widens this column's contract** from "a content hash" to "a content hash, or
    /// this marker", so every reader has to know. The rule, and it must be the same rule
    /// everywhere — the first version of this change answered it two opposite ways in two
    /// files and would have lost bytes:
    ///
    /// > **The marker means: there is unuploaded local content, and we do not have a
    /// > trustworthy record of what it is.**
    ///
    /// So a reader deciding whether to *destroy* local bytes must treat it as a conflict
    /// and preserve them ([`download_would_conflict`]). A reader that needs the content to
    /// decide at all must defer until the next scan supplies a real hash
    /// (`reconcile_remote_absent`). And a reader asking "was there a NEW edit?" must answer
    /// no — the marker is bookkeeping, not an observation (`process_local_file_change`'s
    /// pending-job arm).
    ///
    /// When DBSYNC-40 lands, a nullable column expresses this properly and this constant
    /// should go.
    pub const HASH_NEEDS_RESCAN: &'static str = "";

    /// Marks an existing row for rescan, preserving its size and mtime (DBSYNC-56).
    ///
    /// The **only** way [`Self::HASH_NEEDS_RESCAN`] enters the column. That matters more
    /// than it looks: the marker is the empty string, so a `debug_assert!` inside
    /// `upsert_local_file` could never tell a deliberate marking from an accidentally-blank
    /// hash — the two are the same value, and the assert would be incapable of failing.
    /// Routing intent through a separate method is what makes the assert there meaningful.
    ///
    /// No-op when the row is absent: there is nothing to preserve, and creating one here
    /// would invent a tracked file out of a cancelled upload.
    pub fn mark_local_file_for_rescan(&self, relative_path: &str) -> AppResult<()> {
        let Some(row) = self.get_local_file(relative_path)? else {
            return Ok(());
        };
        self.write_local_file_row(
            relative_path,
            Self::HASH_NEEDS_RESCAN,
            row.size_bytes,
            row.modified_ts,
        )
    }

    pub fn upsert_local_file(
        &self,
        relative_path: &str,
        hash: &str,
        size_bytes: i64,
        modified_ts: i64,
    ) -> AppResult<()> {
        // The empty string is reserved for [`Self::HASH_NEEDS_RESCAN`] and every reader of
        // this column branches on it (DBSYNC-56). A caller writing an accidentally-blank
        // hash — a remote `content_hash` that came back empty, say — would silently mark the
        // row for rescan instead of recording a hash. Deliberate marking goes through
        // [`Self::mark_local_file_for_rescan`], so reaching here with an empty hash is
        // always a mistake.
        //
        // Debug-only: in release the consequence is a redundant re-upload, not data loss,
        // and panicking inside the sync loop would be the worse failure.
        debug_assert!(
            !hash.is_empty(),
            "empty local hash written for {relative_path}: use mark_local_file_for_rescan"
        );
        self.write_local_file_row(relative_path, hash, size_bytes, modified_ts)
    }

    fn write_local_file_row(
        &self,
        relative_path: &str,
        hash: &str,
        size_bytes: i64,
        modified_ts: i64,
    ) -> AppResult<()> {
        debug_assert_canonical_key(relative_path);
        let now = Utc::now().to_rfc3339();
        let conn = self
            .write
            .lock()
            .map_err(|_| AppError::Storage("db write lock poisoned".into()))?;
        // DBSYNC-99: mint an `item_id` on first index, and never mint a second one for a
        // path that already has one — that is what makes it an identity rather than a
        // version. Both halves are load-bearing, and so is where the number comes from.
        //
        // It is NOT `MAX(item_id) + 1`. Rows are deleted here routinely (every local file
        // deletion), so the maximum goes back down and the next file would be handed the
        // identity of a file that no longer exists. Anything still holding the old
        // identifier — File Provider's enumeration, most of all — would then resolve it to
        // the wrong item. `item_id_seq` is an AUTOINCREMENT table, which SQLite guarantees
        // never reuses a rowid even after its rows are gone: the high-water mark lives in
        // `sqlite_sequence`, so the row is deleted again immediately and the table stays
        // empty while the counter keeps climbing.
        let existing: Option<i64> = conn
            .query_row(
                "SELECT item_id FROM local_file_index WHERE relative_path = ?1",
                params![relative_path],
                |row| row.get::<_, Option<i64>>(0),
            )
            .unwrap_or(None);
        let item_id = match existing {
            Some(id) => id,
            None => {
                conn.execute("INSERT INTO item_id_seq DEFAULT VALUES", [])?;
                let id = conn.last_insert_rowid();
                conn.execute("DELETE FROM item_id_seq", [])?;
                id
            }
        };
        conn
            .execute(
                "
                INSERT INTO local_file_index(relative_path, hash, size_bytes, modified_ts, updated_at, item_id)
                VALUES(?1, ?2, ?3, ?4, ?5, ?6)
                ON CONFLICT(relative_path) DO UPDATE SET
                  hash=excluded.hash,
                  size_bytes=excluded.size_bytes,
                  modified_ts=excluded.modified_ts,
                  updated_at=excluded.updated_at,
                  item_id=COALESCE(local_file_index.item_id, excluded.item_id)
                ",
                params![relative_path, hash, size_bytes, modified_ts, now, item_id],
            )
            ?;
        Ok(())
    }

    /// Move every record of an item from one path to another (DBSYNC-99).
    ///
    /// The point of the ticket in one method: the row **travels** instead of being dropped
    /// and a fresh one created. Identity, content hash, rev and Dropbox id all come along,
    /// so a renamed item keeps its history rather than looking like a file that appeared
    /// from nowhere.
    ///
    /// Four tables, because four tables address an item by where it is: the local and
    /// remote indexes, `sync_conflicts` (three paths per row) and any still-active
    /// `sync_jobs`. One transaction, so a rename is either wholly recorded or not at all —
    /// a half-moved item would be worse than an unmoved one.
    ///
    /// Active jobs are moved with `OR IGNORE`. A partial-unique index allows only one
    /// active job per `(job_type, target_path)`, and the destination may already have one;
    /// dropping the redundant duplicate is correct, because both describe the same work on
    /// the same path.
    pub fn move_index_row(&self, old_path: &str, new_path: &str) -> AppResult<()> {
        debug_assert_canonical_key(old_path);
        debug_assert_canonical_key(new_path);
        let now = Utc::now().to_rfc3339();
        let mut conn = self
            .write
            .lock()
            .map_err(|_| AppError::Storage("db write lock poisoned".into()))?;
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE local_file_index SET relative_path = ?2, updated_at = ?3 WHERE relative_path = ?1",
            params![old_path, new_path, now],
        )?;
        tx.execute(
            "UPDATE remote_file_index SET relative_path = ?2, updated_at = ?3 WHERE relative_path = ?1",
            params![old_path, new_path, now],
        )?;
        tx.execute(
            "UPDATE sync_conflicts SET local_path = ?2 WHERE local_path = ?1",
            params![old_path, new_path],
        )?;
        tx.execute(
            "UPDATE sync_conflicts SET remote_path = ?2 WHERE remote_path = ?1",
            params![old_path, new_path],
        )?;
        // `failed` is in the status set deliberately. It looks transient, but
        // `requeue_failed_jobs` resets every failed row back to `queued`, so a download that
        // failed before a rename would be resurrected pointing at the pre-rename path — it
        // writes the file back under its old name and the next scan uploads it as untracked,
        // undoing the rename on the server. The partial-unique index only covers active
        // statuses, so widening the set here cannot collide with it.
        //
        // `local_delete` AND `delete` are excluded alongside `move`, for a sharper reason than
        // tidiness: a job whose whole purpose is destruction must never be re-aimed.
        //
        // `delete_local_file_internal` removes the file unconditionally, so a failed
        // `local_delete` naming the OLD path is harmless — that path no longer exists — but
        // retargeted to the new one it deletes the file the user just renamed, the moment they
        // press Retry. `delete` is the same argument one level out: it destroys the REMOTE
        // copy, the one every other device syncs from, and recursively when the target is a
        // folder. An earlier version of this reasoning was applied to `local_delete` only, and
        // that omission is what turned a stale delete of a path Dropbox no longer has — a
        // benign 409 — into a recursive delete of the folder the user had just renamed.
        //
        // `job_type <> 'move'` is not an optimisation. A move job's two paths describe an
        // OPERATION — move this from here to there — not where an item currently lives, so
        // rewriting them corrupts the instruction. Rewriting the `source_path` of the very
        // move being enqueued collapses it to "move X to X", which Dropbox rejects as not
        // applicable and the job is dropped, leaving the local index pointing at a path the
        // server does not have. Measured on a real install, not imagined.
        tx.execute(
            "UPDATE OR IGNORE sync_jobs SET target_path = ?2, updated_at = ?3 \
             WHERE target_path = ?1 AND job_type NOT IN ('move','local_delete','delete') AND status IN ('queued','retry_wait','running','failed')",
            params![old_path, new_path, now],
        )?;
        tx.execute(
            "UPDATE sync_jobs SET source_path = ?2, updated_at = ?3 \
             WHERE source_path = ?1 AND job_type NOT IN ('move','local_delete','delete') AND status IN ('queued','retry_wait','running','failed')",
            params![old_path, new_path, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Move a folder and everything under it from one path to another (DBSYNC-99).
    ///
    /// The subtree equivalent of [`Self::move_index_row`]: one prefix rewrite instead of a
    /// row-by-row loop, so renaming a folder with a thousand files costs one statement per
    /// table rather than a thousand. Every descendant keeps its identity, its content hash
    /// and its Dropbox id.
    ///
    /// Boundary-safe via `substr`, the same idiom as [`Self::remove_remote_subtree`] and
    /// for the same reason: `d` and `d-other` share a prefix, so the match is on `d/` plus
    /// the exact row, never on `d` alone. `substr` compares BINARY, so no `%`/`_`/`!`
    /// escaping is needed and, unlike `LIKE`, it does not disagree with `=` on case — see
    /// the note in the body.
    ///
    /// One transaction. A half-rewritten subtree — some children under the new name, some
    /// under the old — would be worse than one that never moved.
    ///
    /// **Returns the number of rows left behind.** `UPDATE OR IGNORE` cannot fail on a
    /// destination collision, which is what keeps one surprising row from discarding the
    /// whole watcher batch — but it also means the method used to report `Ok(())` while
    /// silently orphaning rows that kept their identity at a path no longer on disk. The
    /// caller decides what a non-zero count means; it is not this method's to swallow.
    pub fn move_index_subtree(&self, old_prefix: &str, new_prefix: &str) -> AppResult<usize> {
        debug_assert_canonical_key(old_prefix);
        debug_assert_canonical_key(new_prefix);
        let now = Utc::now().to_rfc3339();

        // **The offset is computed in SQL, in characters, and it must stay that way.**
        //
        // This used to pass `old_prefix.len() as i64 + 1` — Rust's BYTE length — as the
        // `substr` start, and SQLite's `substr` counts CHARACTERS. For any non-ASCII prefix
        // the overshoot is `bytes - chars`, which eats the `/` separator and, when it exceeds
        // the tail, the whole tail: measured, `artículos/a.txt` became `Papersa.txt` and
        // `我的文档/a.txt` became exactly `Docs` — the child row collapsing onto the prefix.
        //
        // It was silent. `stranded` counts rows LEFT under the old prefix, and a mangled row
        // is not under it, so the count stayed 0 and `apply_confirmed_move`'s `tracing::error!`
        // never fired. The consequences ran from a lost identity and a full re-upload, through
        // the mass-deletion breaker pausing sync, to a collapsed row naming a real directory
        // and being taken for a deleted file — which enqueues a RECURSIVE `delete_v2` of the
        // folder the user just renamed.
        //
        // `LIKE` is gone for the same reason `prune_stale_refused_moves` dropped it: it is
        // ASCII case-insensitive while `=` is BINARY, so the two halves of every statement
        // here disagreed on case. `substr` compares binary and needs no `%`/`_`/`!` escaping.
        const MATCH: &str = "substr({c}, 1, length(?1) + 1) = ?1 || '/'";
        const TAIL: &str = "substr({c}, length(?1) + 1)";
        let m = |c: &str| MATCH.replace("{c}", c);
        let tail = |c: &str| TAIL.replace("{c}", c);

        let mut conn = self
            .write
            .lock()
            .map_err(|_| AppError::Storage("db write lock poisoned".into()))?;
        let tx = conn.transaction()?;

        for table in ["local_file_index", "remote_file_index", "known_folders"] {
            // The folder / file at the prefix itself.
            tx.execute(
                &format!(
                    "UPDATE OR IGNORE {table} SET relative_path = ?2, updated_at = ?3 \
                     WHERE relative_path = ?1"
                ),
                params![old_prefix, new_prefix, now],
            )?;
            // Everything beneath it.
            tx.execute(
                &format!(
                    "UPDATE OR IGNORE {table} SET relative_path = ?2 || {t}, updated_at = ?3 \
                     WHERE {w}",
                    t = tail("relative_path"),
                    w = m("relative_path")
                ),
                params![old_prefix, new_prefix, now],
            )?;
        }

        // See `move_index_row`: a move job's paths are an instruction, not a location, so
        // they are excluded from every rewrite below.
        for (table, column, guard) in [
            ("sync_conflicts", "local_path", ""),
            ("sync_conflicts", "remote_path", ""),
            (
                "sync_jobs",
                "source_path",
                " AND job_type NOT IN ('move','local_delete','delete') \
                  AND status IN ('queued','retry_wait','running','failed')",
            ),
        ] {
            tx.execute(
                &format!("UPDATE OR IGNORE {table} SET {column} = ?2 WHERE {column} = ?1{guard}"),
                params![old_prefix, new_prefix],
            )?;
            tx.execute(
                &format!(
                    "UPDATE OR IGNORE {table} SET {column} = ?2 || {t} WHERE {w}{guard}",
                    t = tail(column),
                    w = m(column)
                ),
                params![old_prefix, new_prefix],
            )?;
        }
        // `sync_jobs.target_path` carries the partial-unique index on
        // `(job_type, target_path)`, so a collision with an existing active job is dropped
        // rather than aborting the rewrite — both rows describe the same work.
        tx.execute(
            "UPDATE OR IGNORE sync_jobs SET target_path = ?2, updated_at = ?3 \
             WHERE target_path = ?1 AND job_type NOT IN ('move','local_delete','delete') AND status IN ('queued','retry_wait','running','failed')",
            params![old_prefix, new_prefix, now],
        )?;
        tx.execute(
            &format!(
                "UPDATE OR IGNORE sync_jobs SET target_path = ?2 || {t}, updated_at = ?3 \
                 WHERE {w} AND job_type NOT IN ('move','local_delete','delete') \
                   AND status IN ('queued','retry_wait','running','failed')",
                t = tail("target_path"),
                w = m("target_path")
            ),
            params![old_prefix, new_prefix, now],
        )?;

        // Anything still under the old prefix could not be moved, because the destination was
        // occupied. Count it across ALL THREE tables the rewrite touches: counting only
        // `local_file_index` was blind to the collision most likely to happen — a destination
        // Dropbox holds that was never downloaded has remote rows and no local ones, so it
        // would strand `remote_file_index` rows and report zero.
        let mut stranded: i64 = 0;
        for table in ["local_file_index", "remote_file_index", "known_folders"] {
            stranded += tx.query_row(
                &format!(
                    "SELECT COUNT(*) FROM {table} WHERE relative_path = ?1 OR {w}",
                    w = m("relative_path")
                ),
                params![old_prefix],
                |row| row.get::<_, i64>(0),
            )?;
        }

        tx.commit()?;
        Ok(stranded.max(0) as usize)
    }

    pub fn remove_local_file(&self, relative_path: &str) -> AppResult<()> {
        debug_assert_canonical_key(relative_path);
        let conn = self
            .write
            .lock()
            .map_err(|_| AppError::Storage("db write lock poisoned".into()))?;
        conn.execute(
            "DELETE FROM local_file_index WHERE relative_path = ?1",
            params![relative_path],
        )?;
        Ok(())
    }

    pub fn get_remote_file(&self, relative_path: &str) -> AppResult<Option<RemoteFileIndexRow>> {
        debug_assert_canonical_key(relative_path);
        let conn = self
            .read
            .lock()
            .map_err(|_| AppError::Storage("db read lock poisoned".into()))?;
        let mut stmt = conn.prepare(
            "
                SELECT relative_path, content_hash, rev, modified_ts, dropbox_id
                FROM remote_file_index
                WHERE relative_path = ?1
                LIMIT 1
                ",
        )?;
        let mut rows = stmt.query(params![relative_path])?;
        if let Some(row) = rows.next()? {
            return Ok(Some(RemoteFileIndexRow {
                relative_path: row.get(0)?,
                content_hash: row.get(1)?,
                rev: row.get(2)?,
                modified_ts: row.get(3)?,
                dropbox_id: row.get(4)?,
            }));
        }
        Ok(None)
    }

    /// Upsert a remote index row, carrying Dropbox's item identifier when the caller
    /// has one (DBSYNC-99).
    ///
    /// `dropbox_id` is a required parameter rather than a second entry point on purpose.
    /// Several paths write this row — the delta loop, `get_metadata`, the Windows
    /// placeholder sweep — and a convenience overload that quietly passed `None` would
    /// let a caller drop an identifier it was holding, without ever saying so.
    ///
    /// **A stored `dropbox_id` is never cleared by a write that lacks one.** Not every
    /// path knows the id, and losing it would make a known item look brand new — the
    /// exact defect this ticket exists to remove — so the update COALESCEs onto the
    /// stored value rather than overwriting it.
    pub fn upsert_remote_file(
        &self,
        relative_path: &str,
        content_hash: &str,
        rev: &str,
        modified_ts: i64,
        dropbox_id: Option<&str>,
    ) -> AppResult<()> {
        debug_assert_canonical_key(relative_path);
        let now = Utc::now().to_rfc3339();
        let conn = self
            .write
            .lock()
            .map_err(|_| AppError::Storage("db write lock poisoned".into()))?;
        conn.execute(
            "
            INSERT INTO remote_file_index(relative_path, content_hash, rev, modified_ts, updated_at, dropbox_id)
            VALUES(?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(relative_path) DO UPDATE SET
              content_hash=excluded.content_hash,
              rev=excluded.rev,
              modified_ts=excluded.modified_ts,
              updated_at=excluded.updated_at,
              dropbox_id=COALESCE(excluded.dropbox_id, remote_file_index.dropbox_id)
            ",
            params![
                relative_path,
                content_hash,
                rev,
                modified_ts,
                now,
                dropbox_id
            ],
        )?;
        Ok(())
    }

    pub fn remove_remote_file(&self, relative_path: &str) -> AppResult<()> {
        debug_assert_canonical_key(relative_path);
        let conn = self
            .write
            .lock()
            .map_err(|_| AppError::Storage("db write lock poisoned".into()))?;
        conn.execute(
            "DELETE FROM remote_file_index WHERE relative_path = ?1",
            params![relative_path],
        )?;
        Ok(())
    }

    /// DBSYNC-66: clear the remote-index row for `prefix` AND every descendant
    /// under it (`prefix/...`). A folder delete on Dropbox is recursive, so its
    /// whole subtree of remote rows must go too — otherwise the materialization
    /// sweep re-creates placeholders for the (now-deleted) descendants, forcing
    /// the "delete a folder twice" behavior. Boundary-safe via `substr`
    /// so a sibling like `prefix-other` is never matched and `%`/`_`/accents in
    /// the path are treated literally. For a plain file `prefix` this is
    /// equivalent to `remove_remote_file` (no `prefix/...` descendants exist).
    pub fn remove_remote_subtree(&self, prefix: &str) -> AppResult<()> {
        debug_assert_canonical_key(prefix);
        let conn = self
            .write
            .lock()
            .map_err(|_| AppError::Storage("db write lock poisoned".into()))?;
        // `substr`, not `LIKE`, for the reason `move_index_subtree` and
        // `prune_stale_refused_moves` both carry: `LIKE` is ASCII case-insensitive in SQLite
        // while `=` is BINARY, so the two halves of this statement disagreed on case — this
        // one DELETES, so `remove_remote_subtree("Docs")` also took `docs/...` and `DOCS/...`.
        // `substr` compares binary and needs no `%`/`_`/`!` escaping.
        conn.execute(
            "DELETE FROM remote_file_index \
             WHERE relative_path = ?1 OR substr(relative_path, 1, length(?1) + 1) = ?1 || '/'",
            params![prefix],
        )?;
        Ok(())
    }

    pub fn enqueue_job(
        &self,
        job_type: &str,
        source_path: Option<&str>,
        target_path: Option<&str>,
    ) -> AppResult<()> {
        let now = Utc::now().to_rfc3339();
        let conn = self
            .write
            .lock()
            .map_err(|_| AppError::Storage("db write lock poisoned".into()))?;
        // DBSYNC-31: ON CONFLICT against the partial-unique index — if an ACTIVE job for
        // this (job_type, target_path) already exists, collapse into it (refresh, don't
        // duplicate) instead of piling up a second row. We deliberately do NOT reset its
        // status/attempt_count — a `running` or backing-off `retry_wait` job keeps its
        // lifecycle; the existing job re-reads the current file state when it runs. Rows
        // with a NULL target_path (e.g. hydrate_cloudsc) aren't covered by the partial
        // index (NULLs are distinct) and insert normally, matching prior behaviour.
        conn
            .execute(
                "
                INSERT INTO sync_jobs(job_type, source_path, target_path, status, attempt_count, next_retry_at, created_at, updated_at)
                VALUES(?1, ?2, ?3, 'queued', 0, NULL, ?4, ?4)
                ON CONFLICT(job_type, target_path) WHERE status IN ('queued','retry_wait','running')
                DO UPDATE SET source_path=excluded.source_path, updated_at=excluded.updated_at
                ",
                params![job_type, source_path, target_path, now],
            )
            ?;
        Ok(())
    }

    /// DBSYNC-65 (Slice 1): dedicated `delete` job enqueue that also captures the
    /// Dropbox `rev` of the file being deleted at enqueue time, so a later drain
    /// can detect (Slice 2) whether the remote copy changed since the local
    /// delete was observed. Mirrors `enqueue_job`'s ON CONFLICT collapse, but the
    /// `DO UPDATE SET` additionally refreshes `delete_parent_rev` — re-enqueuing a
    /// delete for an already-queued path must NOT keep a stale captured rev.
    pub fn enqueue_delete_job(&self, target_path: &str, parent_rev: Option<&str>) -> AppResult<()> {
        let now = Utc::now().to_rfc3339();
        let conn = self
            .write
            .lock()
            .map_err(|_| AppError::Storage("db write lock poisoned".into()))?;
        conn.execute(
            "
            INSERT INTO sync_jobs(job_type, source_path, target_path, delete_parent_rev, status, attempt_count, next_retry_at, created_at, updated_at)
            VALUES('delete', ?1, ?1, ?2, 'queued', 0, NULL, ?3, ?3)
            ON CONFLICT(job_type, target_path) WHERE status IN ('queued','retry_wait','running')
            DO UPDATE SET source_path=excluded.source_path, delete_parent_rev=excluded.delete_parent_rev, updated_at=excluded.updated_at
            ",
            params![target_path, parent_rev, now],
        )?;
        Ok(())
    }

    /// DBSYNC-99: enqueue an upload that **owes a deletion** — the source of a move Dropbox
    /// refused. The deletion is carried on the upload row instead of being enqueued next to
    /// it, and that is the whole point.
    ///
    /// Two jobs ordered by id only look safe. `pick_next_due_job` orders by id **among the
    /// jobs that are due**, and a job in `retry_wait` with a future `next_retry_at` is not
    /// due at all — so one transient upload failure (a 429, a 5xx, a file locked by another
    /// process) drops the upload out of the candidate set and the delete, still `queued`,
    /// drains first. The content would then exist at neither path until the upload came back,
    /// which for a five-attempt backoff is minutes.
    ///
    /// Carried here, the delete row does not exist until the upload has actually succeeded —
    /// see the `upload` success arm in `process_sync_queue_internal`, which is the only place
    /// that reads this column. Causation, not ordering.
    ///
    /// The `DO UPDATE` must carry the columns for the same reason `enqueue_delete_job`'s
    /// carries `delete_parent_rev`: if an upload for this destination is already active, the
    /// plain `enqueue_job` collapse would silently discard the owed deletion.
    ///
    /// But it must NOT overwrite a **different** owed deletion. Two refused moves onto the
    /// same destination (A→X refused, later B→X refused) would leave the upload owing only B,
    /// while A's index rows are already gone — an orphan on Dropbox that nothing will ever
    /// delete or index. `COALESCE` keeps the first debt and the caller is told the second was
    /// not taken, because one upload can only settle one source.
    ///
    /// **If the upload never succeeds** — five attempts exhausted, or the app stops first —
    /// the deletion simply never happens and Dropbox keeps the source as a duplicate. That is
    /// the failure this shape is chosen for: never a window with no copy at all. The duplicate
    /// is not self-healing; see the caller for what is logged.
    ///
    /// Returns whether this call's deletion is the one now owed.
    pub fn enqueue_upload_then_delete(
        &self,
        upload_path: &str,
        delete_path: &str,
        delete_parent_rev: Option<&str>,
    ) -> AppResult<bool> {
        let now = Utc::now().to_rfc3339();
        let mut conn = self
            .write
            .lock()
            .map_err(|_| AppError::Storage("db write lock poisoned".into()))?;
        // The INSERT and the read-back that decides `took_the_debt` must be ONE transaction.
        // Apart, a concurrent drain flipping the row to `done` between them makes the SELECT
        // return nothing, the caller concludes "another upload already owes a different
        // source", records a refusal and keeps the source's index rows — while the row it
        // just wrote is the one holding the debt.
        let tx = conn.transaction()?;
        tx.execute(
            "
            INSERT INTO sync_jobs(job_type, source_path, target_path, on_success_delete_path, on_success_delete_rev, status, attempt_count, next_retry_at, created_at, updated_at)
            VALUES('upload', ?1, ?1, ?2, ?3, 'queued', 0, NULL, ?4, ?4)
            ON CONFLICT(job_type, target_path) WHERE status IN ('queued','retry_wait','running')
            DO UPDATE SET
                source_path=excluded.source_path,
                on_success_delete_path=COALESCE(sync_jobs.on_success_delete_path, excluded.on_success_delete_path),
                on_success_delete_rev=CASE
                    WHEN sync_jobs.on_success_delete_path IS NULL THEN excluded.on_success_delete_rev
                    ELSE sync_jobs.on_success_delete_rev END,
                updated_at=excluded.updated_at
            ",
            params![upload_path, delete_path, delete_parent_rev, now],
        )?;
        let owed: Option<String> = tx
            .query_row(
                "SELECT on_success_delete_path FROM sync_jobs WHERE job_type='upload' AND target_path=?1 AND status IN ('queued','retry_wait','running')",
                params![upload_path],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let took = owed.as_deref() == Some(delete_path);
        tx.commit()?;
        Ok(took)
    }

    /// Read the deletion an upload job owes its source, **without** clearing it.
    ///
    /// Read and clear used to be one step, which looked like "act on it at most once" and was
    /// really "destroy it on the first look". The caller checks a condition after reading, and
    /// every way that check can decline — the destination does not hold the bytes, a storage
    /// error, the enqueue itself failing — returned with the record already gone and the job
    /// marked `done` immediately after. A single transient error permanently discarded a
    /// deletion for an upload that had succeeded. Clearing is now
    /// [`Self::clear_deferred_source_delete`], called only once the delete job exists.
    ///
    /// Restricted to `upload` rows: the column has no meaning on any other job type, and a
    /// query that would honour it there is a query that could be made to delete a path by
    /// setting a field on the wrong row.
    pub fn peek_deferred_source_delete(
        &self,
        job_id: i64,
    ) -> AppResult<Option<(String, Option<String>)>> {
        let conn = self
            .read
            .lock()
            .map_err(|_| AppError::Storage("db read lock poisoned".into()))?;
        let owed: Option<(Option<String>, Option<String>)> = conn
            .query_row(
                "SELECT on_success_delete_path, on_success_delete_rev FROM sync_jobs WHERE id = ?1 AND job_type = 'upload'",
                params![job_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((Some(path), rev)) = owed else {
            return Ok(None);
        };
        Ok(Some((path, rev)))
    }

    /// Clear a settled debt, so the same deletion cannot be enqueued twice. Call only after
    /// the delete job exists — see [`Self::peek_deferred_source_delete`].
    pub fn clear_deferred_source_delete(&self, job_id: i64) -> AppResult<()> {
        let conn = self
            .write
            .lock()
            .map_err(|_| AppError::Storage("db write lock poisoned".into()))?;
        conn.execute(
            "UPDATE sync_jobs SET on_success_delete_path=NULL, on_success_delete_rev=NULL WHERE id = ?1 AND job_type = 'upload'",
            params![job_id],
        )?;
        Ok(())
    }

    /// DBSYNC-99: a source deletion still owed by an upload that will never run again.
    ///
    /// The debt is cleared only when the delete job exists, so a debt still sitting on a
    /// **terminal** upload row means the deletion never happened and never will: either the
    /// upload died after five attempts, or it landed and the destination could not be
    /// confirmed to hold the bytes. Either way Dropbox keeps the old name beside the new one
    /// and nothing heals it.
    ///
    /// Returning it from the jobs table rather than a flag is deliberate. `refresh_queue_
    /// depth_internal` recomputes `last_error` from durable sources on every tick and clears
    /// anything else, so a bare `set_last_error` would vanish a second later — its own comment
    /// says so. This is durable for exactly as long as the condition holds, and self-clearing:
    /// settling the debt or clearing the job row removes it, with no new user action to build.
    /// Returns `(source, destination)`. The message built from this **must not claim more than
    /// was checked** — see the caller, which asks about the destination rather than guessing.
    ///
    /// A third element carrying the job's `status` was carried through this whole call chain
    /// to decide nothing: `latest_failed_error` fires whenever any job row is `failed` and now
    /// outranks this advisory, so a `failed` row never reaches the message at all.
    ///
    /// The source must still be in `remote_file_index`, and that clause is the exit. The
    /// notice used to have none: nothing clears the column on a terminal row, `sync_jobs` is
    /// never pruned of `done`/`failed` rows, and `requeue_failed_jobs` touches only `failed`.
    /// So it returned `Some` forever, and — ranked above `latest_failed_error` — it masked
    /// every real failure after it: an expired token, a full disk, a rejected upload. Tying it
    /// to the remote row makes it self-falsifying: when the user (or the sweep) removes the
    /// old name from Dropbox, the condition stops holding and the notice goes.
    ///
    /// **The clause is a filter, not evidence, and an earlier version of this doc claimed
    /// otherwise.** It said the row appears only once the sweep has re-indexed the path, so
    /// its presence meant Dropbox had been *observed* to still hold it. That was false twice
    /// over: the sweep cannot re-index a path with no local row, and `rederive_refused_move`
    /// now writes the row itself. What the clause does is exclude paths already known to be
    /// gone, and give the notice an exit — when the row goes, so does the notice.
    pub fn unsettled_source_deletion(&self) -> AppResult<Option<(String, String)>> {
        let conn = self
            .read
            .lock()
            .map_err(|_| AppError::Storage("db read lock poisoned".into()))?;
        let row = conn
            .query_row(
                "SELECT on_success_delete_path, source_path FROM sync_jobs
                 WHERE job_type = 'upload' AND status IN ('done','failed')
                   AND on_success_delete_path IS NOT NULL
                   AND on_success_delete_path IN (SELECT relative_path FROM remote_file_index)
                 ORDER BY updated_at DESC LIMIT 1",
                [],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
            )
            .optional()?;
        // A NULL `source_path` cannot happen — `enqueue_upload_then_delete` always writes it —
        // but rendering "renamed to ''" would be worse than saying nothing, so decline.
        Ok(row.and_then(|(source, destination)| destination.map(|d| (source, d))))
    }

    /// DBSYNC-99: remember a folder move Dropbox permanently refused, so the correlator stops
    /// proposing it. See the `refused_moves` table comment for why not remembering it means
    /// the rename never reaches Dropbox at all.
    pub fn record_refused_move(&self, from_path: &str, to_path: &str) -> AppResult<()> {
        let conn = self
            .write
            .lock()
            .map_err(|_| AppError::Storage("db write lock poisoned".into()))?;
        conn.execute(
            "INSERT INTO refused_moves(from_path, to_path, refused_at) VALUES(?1, ?2, ?3)
             ON CONFLICT(from_path, to_path) DO UPDATE SET refused_at=excluded.refused_at",
            params![from_path, to_path, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn list_refused_moves(&self) -> AppResult<HashSet<(String, String)>> {
        let conn = self
            .read
            .lock()
            .map_err(|_| AppError::Storage("db read lock poisoned".into()))?;
        let mut stmt = conn.prepare("SELECT from_path, to_path FROM refused_moves")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<Result<HashSet<_>, _>>()?)
    }

    /// Drop refusals whose source the index no longer tracks **and** which no job still names.
    ///
    /// An entry exists to stop a pair being re-proposed, and a pair can only be proposed while
    /// `from_path` is still tracked — as a folder for the directory correlator, as a local file
    /// row for the file one. Both are checked: an earlier version tested `known_folders` alone
    /// and so forgot a file refusal on the next tick, which is one of the two halves that let
    /// a refused file move loop.
    ///
    /// The active-job clause is the other half, and it is about timing. Being untracked means
    /// the fallback has **started**, not that it has finished: at that moment its deletes and
    /// its upload are still queued and can still fail, be dropped by
    /// `delete_suppressed_by_dehydration`, or exhaust their attempts. If anything then puts
    /// the source back — the remote sweep re-seeding a folder Dropbox still holds because the
    /// delete failed — the whole cycle re-arms: correlate, live `move_v2`, refusal, record.
    /// So the refusal outlives the work it caused.
    pub fn prune_stale_refused_moves(&self) -> AppResult<usize> {
        let conn = self
            .write
            .lock()
            .map_err(|_| AppError::Storage("db write lock poisoned".into()))?;
        // Prefix-aware, because `rederive_refused_move` decides "this is a directory" by
        // finding index rows UNDER the path. Matching only exact rows meant a directory found
        // that way — one with no `known_folders` row, the case the structural test was added
        // for — satisfied every clause and had its refusal deleted on the very next tick. The
        // entry the code calls "not bookkeeping, the whole fix" lived for one scan.
        //
        // The same applies to the in-flight clause: a folder's fallback enqueues jobs for its
        // CHILDREN, so an exact match only covered the window in which a delete for the folder
        // path itself happened to be queued — and `process_known_folder_deletion` is skipped
        // outright when a `.cloudsc` placeholder exists.
        //
        // `NOT EXISTS` rather than `NOT IN`: `relative_path` is a TEXT PRIMARY KEY, which
        // SQLite does not imply NOT NULL, and one NULL row would make `NOT IN` evaluate to
        // NULL for every candidate and silently prune nothing for ever.
        //
        // Prefix matching by `substr`, not `LIKE`.
        //
        // `LIKE` is ASCII case-insensitive in SQLite unless `case_sensitive_like` is set, while
        // `=` is BINARY — so the exact half and the prefix half of each clause disagreed on
        // case, and an unrelated `docs/other.txt` retained a refusal recorded for `Docs`. That
        // direction is safe (over-retention), but a retained refusal pushes a genuine later
        // rename back onto delete-plus-full-upload, which is the regression this ticket exists
        // to remove. `COLLATE BINARY` does NOT fix it: collations are ignored by `LIKE` —
        // measured, after reaching for it first.
        //
        // `substr(path, 1, length(prefix) + 1) = prefix || '/'` compares binary, says exactly
        // what is meant, and needs **no escaping at all**: `%`, `_` and `!` are ordinary
        // characters here. That removes the triple-`replace` and the bug class it guarded.
        let under = |col: &str| {
            format!("substr({col}, 1, length(refused_moves.from_path) + 1) = refused_moves.from_path || '/'")
        };
        let (kf, lfi, sp, tp) = (
            under("relative_path"),
            under("relative_path"),
            under("source_path"),
            under("target_path"),
        );
        let sql = format!(
            "DELETE FROM refused_moves
             WHERE NOT EXISTS (SELECT 1 FROM known_folders
                                WHERE relative_path = refused_moves.from_path OR {kf})
               AND NOT EXISTS (SELECT 1 FROM local_file_index
                                WHERE relative_path = refused_moves.from_path OR {lfi})
               AND NOT EXISTS (SELECT 1 FROM sync_jobs
                                WHERE status IN ('queued','retry_wait','running')
                                  AND (source_path = refused_moves.from_path
                                    OR target_path = refused_moves.from_path
                                    OR {sp} OR {tp}))"
        );
        let removed = conn.execute(&sql, [])?;
        Ok(removed)
    }

    pub fn count_active_jobs(&self) -> AppResult<usize> {
        let conn = self
            .read
            .lock()
            .map_err(|_| AppError::Storage("db read lock poisoned".into()))?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sync_jobs WHERE status IN ('queued', 'retry_wait', 'running')",
            [],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    /// The paths an active **move** job names, as source or as target.
    ///
    /// Narrower than [`Self::active_job_paths`] and deliberately so. A deletion must be held
    /// back only when something is about to **relocate** that path — which is what a move
    /// does and what nothing else does. Holding one back for any job in flight was too broad,
    /// and the excess lost deletions outright: the materialization sweep plants a `.cloudsc`
    /// sidecar for any remote child whose local counterpart is absent, consulting no index,
    /// and `process_local_file_deletion` then drops a delete whose path has a placeholder —
    /// removing the index row that remembers it. The file stayed on Dropbox forever.
    ///
    /// Before that over-broad deferral the delete was emitted in the same watcher batch and
    /// drained before any sweep could run, so the window was ~0. This restores that for every
    /// job type except the one with a real claim on the path.
    pub fn active_move_paths(&self) -> AppResult<std::collections::HashSet<String>> {
        let conn = self
            .read
            .lock()
            .map_err(|_| AppError::Storage("db read lock poisoned".into()))?;
        let mut stmt = conn.prepare(
            "
            SELECT target_path FROM sync_jobs
              WHERE job_type = 'move' AND status IN ('queued','retry_wait','running')
                AND target_path IS NOT NULL AND target_path <> ''
            UNION
            SELECT source_path FROM sync_jobs
              WHERE job_type = 'move' AND status IN ('queued','retry_wait','running')
                AND source_path IS NOT NULL AND source_path <> ''
            ",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut out = std::collections::HashSet::new();
        for r in rows {
            out.insert(r?);
        }
        Ok(out)
    }

    /// Distinct `target_path` + `source_path` of every ACTIVE job (`queued`,
    /// `retry_wait`, `running`). DBSYNC-31: replaces the `list_recent_jobs(N)`-based
    /// dedup, which silently missed jobs once the table exceeded N rows. Backed by the
    /// `idx_sync_jobs_status_retry` index. Used to avoid enqueuing duplicate work and to
    /// route a change that races a still-pending job to a conflicted copy.
    ///
    /// The UNION of both columns is load-bearing for DBSYNC-99 and not merely thorough: a
    /// queued `move` names the pre-rename path as its source and the post-rename path as its
    /// target, and the index is not rewritten until Dropbox confirms the move. So between
    /// enqueue and drain the index still describes the old world while the disk describes the
    /// new one, and BOTH paths have to be protected — the old one from being propagated as a
    /// deletion, the new one from being uploaded as a stranger.
    ///
    /// Consumers must ask with `covered_by_active_job`, never `.contains()`: the hazard is
    /// prefix-shaped and this set holds only the two folder paths.
    pub fn active_job_paths(&self) -> AppResult<std::collections::HashSet<String>> {
        let conn = self
            .read
            .lock()
            .map_err(|_| AppError::Storage("db read lock poisoned".into()))?;
        let mut stmt = conn.prepare(
            "
            SELECT target_path FROM sync_jobs
              WHERE status IN ('queued','retry_wait','running') AND target_path IS NOT NULL AND target_path <> ''
            UNION
            SELECT source_path FROM sync_jobs
              WHERE status IN ('queued','retry_wait','running') AND source_path IS NOT NULL AND source_path <> ''
            ",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut out = std::collections::HashSet::new();
        for r in rows {
            out.insert(r?);
        }
        Ok(out)
    }

    pub fn list_recent_jobs(&self, limit: i64) -> AppResult<Vec<SyncJobRow>> {
        let conn = self
            .read
            .lock()
            .map_err(|_| AppError::Storage("db read lock poisoned".into()))?;
        let mut stmt = conn
            .prepare(
                "
                SELECT id, job_type, source_path, target_path, status, attempt_count, next_retry_at, updated_at, last_error, delete_parent_rev
                FROM sync_jobs
                ORDER BY id DESC
                LIMIT ?1
                ",
            )
            ?;

        let rows = stmt.query_map(params![limit], |row| {
            Ok(SyncJobRow {
                id: row.get(0)?,
                job_type: row.get(1)?,
                source_path: row.get(2)?,
                target_path: row.get(3)?,
                status: row.get(4)?,
                attempt_count: row.get(5)?,
                next_retry_at: row.get(6)?,
                updated_at: row.get(7)?,
                last_error: row.get(8)?,
                delete_parent_rev: row.get(9)?,
            })
        })?;

        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn pick_next_due_job(&self) -> AppResult<Option<SyncJobRow>> {
        let now = Utc::now().to_rfc3339();
        let conn = self
            .write
            .lock()
            .map_err(|_| AppError::Storage("db write lock poisoned".into()))?;
        let job_opt: Option<SyncJobRow> = {
            let mut stmt = conn
                .prepare(
                    "
                SELECT id, job_type, source_path, target_path, status, attempt_count, next_retry_at, updated_at, last_error, delete_parent_rev
                FROM sync_jobs
                WHERE status = 'queued' OR (status = 'retry_wait' AND (next_retry_at IS NULL OR next_retry_at <= ?1))
                ORDER BY id ASC
                LIMIT 1
                ",
                )
                ?;

            let mut rows = stmt.query(params![now])?;
            if let Some(row) = rows.next()? {
                Some(SyncJobRow {
                    id: row.get(0)?,
                    job_type: row.get(1)?,
                    source_path: row.get(2)?,
                    target_path: row.get(3)?,
                    status: row.get(4)?,
                    attempt_count: row.get(5)?,
                    next_retry_at: row.get(6)?,
                    updated_at: row.get(7)?,
                    last_error: row.get(8)?,
                    delete_parent_rev: row.get(9)?,
                })
            } else {
                None
            }
        };

        if let Some(ref job) = job_opt {
            conn.execute(
                "UPDATE sync_jobs SET status='running', updated_at=?2 WHERE id=?1",
                params![job.id, Utc::now().to_rfc3339()],
            )?;
        }
        Ok(job_opt)
    }

    pub fn mark_job_completed(&self, id: i64) -> AppResult<()> {
        let conn = self
            .write
            .lock()
            .map_err(|_| AppError::Storage("db write lock poisoned".into()))?;
        conn.execute(
            "UPDATE sync_jobs SET status='done', last_error=NULL, updated_at=?2 WHERE id=?1",
            params![id, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn mark_job_retry_wait(
        &self,
        id: i64,
        attempt_count: i64,
        next_retry_at: &str,
        last_error: Option<&str>,
    ) -> AppResult<()> {
        let conn = self
            .write
            .lock()
            .map_err(|_| AppError::Storage("db write lock poisoned".into()))?;
        conn
            .execute(
                "
                UPDATE sync_jobs
                SET status='retry_wait', attempt_count=?2, next_retry_at=?3, last_error=?4, updated_at=?5
                WHERE id=?1
                ",
                params![id, attempt_count, next_retry_at, last_error, Utc::now().to_rfc3339()],
            )
            ?;
        Ok(())
    }

    pub fn mark_job_failed(
        &self,
        id: i64,
        attempt_count: i64,
        last_error: Option<&str>,
    ) -> AppResult<()> {
        let conn = self
            .write
            .lock()
            .map_err(|_| AppError::Storage("db write lock poisoned".into()))?;
        conn.execute(
            "
                UPDATE sync_jobs
                SET status='failed', attempt_count=?2, last_error=?3, updated_at=?4,
                    upload_session_id=NULL, upload_session_offset=NULL,
                    upload_session_file_len=NULL, upload_session_file_mtime=NULL
                WHERE id=?1
                ",
            params![id, attempt_count, last_error, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// Resets jobs stuck in `running` (e.g. the app was killed mid-upload) back to
    /// `queued` with a clean attempt count, since an interruption is not a genuine
    /// failed attempt. Deliberately leaves `upload_session_id`/`upload_session_offset`
    /// untouched so an interrupted large-file upload resumes from its last checkpoint
    /// instead of restarting from byte 0. Returns the number of rows recovered.
    pub fn recover_running_jobs(&self) -> AppResult<usize> {
        let conn = self
            .write
            .lock()
            .map_err(|_| AppError::Storage("db write lock poisoned".into()))?;
        let n = conn.execute(
            "
                UPDATE sync_jobs
                SET status='queued', attempt_count=0, next_retry_at=NULL, updated_at=?1
                WHERE status='running'
                ",
            params![Utc::now().to_rfc3339()],
        )?;
        Ok(n)
    }

    /// Persists the in-progress Dropbox upload-session checkpoint for `job_id` so a
    /// restart (or a retried attempt) can resume the chunked upload instead of
    /// starting over from byte 0. `file_len`/`file_mtime` record the identity of
    /// the local file at the time of the checkpoint, so a later resume attempt can
    /// detect whether the file changed underneath the job (see `get_upload_checkpoint`
    /// and the resume guard in `dropbox_transfer::upload_via_session`) and refuse to
    /// silently append new content onto a stale session.
    pub fn save_upload_checkpoint(
        &self,
        job_id: i64,
        session_id: &str,
        offset: u64,
        file_len: u64,
        file_mtime: i64,
    ) -> AppResult<()> {
        let conn = self
            .write
            .lock()
            .map_err(|_| AppError::Storage("db write lock poisoned".into()))?;
        conn.execute(
            "
                UPDATE sync_jobs
                SET upload_session_id=?2, upload_session_offset=?3,
                    upload_session_file_len=?4, upload_session_file_mtime=?5, updated_at=?6
                WHERE id=?1
                ",
            params![
                job_id,
                session_id,
                offset as i64,
                file_len as i64,
                file_mtime,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    /// Returns the saved upload-session checkpoint for `job_id`, if any, as
    /// `(session_id, offset, file_len, file_mtime)`. `file_len`/`file_mtime` default
    /// to 0 when NULL (checkpoints saved before this column existed). Callers must
    /// compare `file_len`/`file_mtime` against the file currently being uploaded
    /// before resuming — this method only round-trips the stored values, it does
    /// not itself validate identity.
    pub fn get_upload_checkpoint(&self, job_id: i64) -> AppResult<Option<(String, u64, u64, i64)>> {
        let conn = self
            .read
            .lock()
            .map_err(|_| AppError::Storage("db read lock poisoned".into()))?;
        let mut stmt = conn.prepare(
            "
                SELECT upload_session_id, upload_session_offset,
                       upload_session_file_len, upload_session_file_mtime
                FROM sync_jobs WHERE id=?1
                ",
        )?;
        let mut rows = stmt.query(params![job_id])?;
        if let Some(row) = rows.next()? {
            let session_id: Option<String> = row.get(0)?;
            let offset: Option<i64> = row.get(1)?;
            let file_len: Option<i64> = row.get(2)?;
            let file_mtime: Option<i64> = row.get(3)?;
            if let Some(session_id) = session_id {
                return Ok(Some((
                    session_id,
                    offset.unwrap_or(0) as u64,
                    file_len.unwrap_or(0) as u64,
                    file_mtime.unwrap_or(0),
                )));
            }
        }
        Ok(None)
    }

    /// Clears the upload-session checkpoint for `job_id` (called once the upload
    /// finishes successfully, or when the job is abandoned).
    pub fn clear_upload_checkpoint(&self, job_id: i64) -> AppResult<()> {
        let conn = self
            .write
            .lock()
            .map_err(|_| AppError::Storage("db write lock poisoned".into()))?;
        conn.execute(
            "
                UPDATE sync_jobs
                SET upload_session_id=NULL, upload_session_offset=NULL,
                    upload_session_file_len=NULL, upload_session_file_mtime=NULL, updated_at=?2
                WHERE id=?1
                ",
            params![job_id, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// The most recent failed job's error message, or `None` if no jobs are failed.
    /// Drives the dashboard's global error/health so a later unrelated success
    /// doesn't mask that failures are still present.
    pub fn latest_failed_error(&self) -> AppResult<Option<String>> {
        let conn = self
            .read
            .lock()
            .map_err(|_| AppError::Storage("db read lock poisoned".into()))?;
        let mut stmt = conn.prepare(
            "
                SELECT last_error FROM sync_jobs
                WHERE status='failed'
                ORDER BY id DESC
                LIMIT 1
                ",
        )?;
        let mut rows = stmt.query([])?;
        if let Some(row) = rows.next()? {
            let msg: Option<String> = row.get(0)?;
            return Ok(Some(msg.unwrap_or_else(|| "job failed".to_string())));
        }
        Ok(None)
    }

    /// Resets all `failed` jobs back to `queued` so they are retried. Returns the count.
    pub fn requeue_failed_jobs(&self) -> AppResult<usize> {
        let conn = self
            .write
            .lock()
            .map_err(|_| AppError::Storage("db write lock poisoned".into()))?;
        let n = conn
            .execute(
                "
                UPDATE sync_jobs
                SET status='queued', attempt_count=0, next_retry_at=NULL, last_error=NULL, updated_at=?1
                WHERE status='failed'
                ",
                params![Utc::now().to_rfc3339()],
            )
            ?;
        Ok(n)
    }

    pub fn add_conflict(
        &self,
        local_path: &str,
        remote_path: &str,
        reason: &str,
        conflicted_copy_path: Option<&str>,
        remote_deleted: bool,
    ) -> AppResult<()> {
        let conn = self
            .write
            .lock()
            .map_err(|_| AppError::Storage("db write lock poisoned".into()))?;
        conn.execute(
            "
                INSERT INTO sync_conflicts
                    (local_path, remote_path, reason, conflicted_copy_path, remote_deleted, resolved, created_at)
                VALUES(?1, ?2, ?3, ?4, ?5, 0, ?6)
                ",
            params![
                local_path,
                remote_path,
                reason,
                conflicted_copy_path,
                remote_deleted as i64,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    /// Only UNRESOLVED conflicts — this backs the actionable list in the UI. Once a
    /// conflict is resolved (`mark_conflict_resolved`) it drops off here and the
    /// overlay stops flagging its path (`list_unresolved_conflict_local_paths`).
    pub fn list_recent_conflicts(&self, limit: i64) -> AppResult<Vec<ConflictRow>> {
        let conn = self
            .read
            .lock()
            .map_err(|_| AppError::Storage("db read lock poisoned".into()))?;
        let mut stmt = conn.prepare(
            "
                SELECT id, local_path, remote_path, reason, conflicted_copy_path,
                       remote_deleted, created_at
                FROM sync_conflicts
                WHERE resolved = 0
                ORDER BY id DESC
                LIMIT ?1
                ",
        )?;

        let rows = stmt.query_map(params![limit], |row| {
            Ok(ConflictRow {
                id: row.get(0)?,
                local_path: row.get(1)?,
                remote_path: row.get(2)?,
                reason: row.get(3)?,
                conflicted_copy_path: row.get(4)?,
                remote_deleted: row.get::<_, i64>(5)? != 0,
                created_at: row.get(6)?,
            })
        })?;

        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Fetch a single unresolved conflict by id (for the resolver). Returns `None`
    /// if it doesn't exist or was already resolved — so a double-click resolves once.
    pub fn get_unresolved_conflict(&self, id: i64) -> AppResult<Option<ConflictRow>> {
        let conn = self
            .read
            .lock()
            .map_err(|_| AppError::Storage("db read lock poisoned".into()))?;
        let mut stmt = conn.prepare(
            "
                SELECT id, local_path, remote_path, reason, conflicted_copy_path,
                       remote_deleted, created_at
                FROM sync_conflicts
                WHERE id = ?1 AND resolved = 0
                ",
        )?;
        let mut rows = stmt.query(params![id])?;
        if let Some(row) = rows.next()? {
            return Ok(Some(ConflictRow {
                id: row.get(0)?,
                local_path: row.get(1)?,
                remote_path: row.get(2)?,
                reason: row.get(3)?,
                conflicted_copy_path: row.get(4)?,
                remote_deleted: row.get::<_, i64>(5)? != 0,
                created_at: row.get(6)?,
            }));
        }
        Ok(None)
    }

    /// Marks a conflict row resolved. Idempotent (a no-op if already resolved).
    pub fn mark_conflict_resolved(&self, id: i64) -> AppResult<()> {
        let conn = self
            .write
            .lock()
            .map_err(|_| AppError::Storage("db write lock poisoned".into()))?;
        conn.execute(
            "UPDATE sync_conflicts SET resolved = 1 WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    pub fn list_unresolved_conflict_local_paths(&self) -> AppResult<Vec<String>> {
        let conn = self
            .read
            .lock()
            .map_err(|_| AppError::Storage("db read lock poisoned".into()))?;
        let mut stmt = conn.prepare(
            "
                SELECT DISTINCT local_path
                FROM sync_conflicts
                WHERE resolved = 0
                ORDER BY local_path
                ",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

/// Brings any database up to the schema this build expects, **atomically** (DBSYNC-40).
///
/// ## Why there is no version counter, and what to use if that changes
///
/// This is deliberately **declarative and self-converging**: every step states a desired
/// end state — `CREATE TABLE IF NOT EXISTS`, `add_column_if_missing`, a guarded rebuild —
/// so a database reaches it from wherever it happens to be. A `schema_version` counter
/// would describe a *sequence* instead, and a counter that is wrong (a hand-edited file, a
/// restored backup, a half-applied migration from before this function was transactional)
/// silently skips the very steps that would have repaired it. Convergence degrades better
/// than sequencing.
///
/// The cost, stated so the trade is visible: startup re-inspects the schema on every run,
/// and a failure remains a silent retry rather than a detectable stop. Both are small today
/// and grow slowly. Revisit if this function gets materially longer, or if some migration
/// ever genuinely cannot be written idempotently.
///
/// **If it is revisited, the mechanism is `PRAGMA user_version`, not a row in `app_config`.**
/// That table is created by this very function, so reading a version out of it before
/// migrating needs its own bootstrap step on a fresh database — solvable in a line, but one
/// more thing to get right for no benefit. The real argument is the other one:
/// `user_version` lives in the file header and participates in the transaction below.
///
/// ## The transaction
///
/// One transaction for the whole sequence, not one per step: a half-migrated schema is
/// exactly what must not survive, and committing between steps would preserve it.
///
/// `PRAGMA journal_mode` and `foreign_keys` are set by the caller **before** this runs and
/// must stay there — `journal_mode` cannot be changed inside a transaction, and moving it
/// in would be a silent regression no test here would catch. The only PRAGMA reached from
/// inside is `table_info`, a read, which is safe.
fn migrate(conn: &mut Connection) -> AppResult<()> {
    let tx = conn.transaction()?;
    migrate_in_tx(&tx)?;
    tx.commit()?;
    Ok(())
}

/// The migration steps themselves. Split out so [`migrate`] owns the transaction and this
/// owns the schema — and so a failure anywhere below unwinds through one `?` to a rollback.
fn migrate_in_tx(conn: &rusqlite::Transaction<'_>) -> AppResult<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS app_config (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS sync_jobs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            job_type TEXT NOT NULL CHECK (job_type IN ('upload','download','delete','local_delete','hydrate_cloudsc','move')),
            source_path TEXT,
            target_path TEXT,
            status TEXT NOT NULL CHECK (status IN ('queued','running','retry_wait','done','failed')),
            attempt_count INTEGER NOT NULL DEFAULT 0,
            next_retry_at TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS sync_conflicts (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            local_path TEXT NOT NULL,
            remote_path TEXT NOT NULL,
            reason TEXT NOT NULL,
            conflicted_copy_path TEXT,
            remote_deleted INTEGER NOT NULL DEFAULT 0,
            resolved INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS local_file_index (
            relative_path TEXT PRIMARY KEY,
            hash TEXT NOT NULL,
            size_bytes INTEGER NOT NULL,
            modified_ts INTEGER NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS remote_file_index (
            relative_path TEXT PRIMARY KEY,
            content_hash TEXT NOT NULL,
            rev TEXT NOT NULL,
            modified_ts INTEGER NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS known_folders (
            relative_path TEXT PRIMARY KEY,
            updated_at TEXT NOT NULL
        );

        -- DBSYNC-99: folder renames Dropbox has permanently refused.
        --
        -- Without this the refusal does not converge. `rederive_refused_move` declines to
        -- recover a folder (see DBSYNC-100), but declining writes nothing, so the correlator
        -- sees byte-for-byte identical inputs on the next scan and makes the same pair again
        -- — one live `files/move_v2` per tick, forever. Worse, the pair itself suppresses the
        -- ordinary delete-plus-upload fallback via `under_moved_dir`, so the rename never
        -- reaches Dropbox and edits under the renamed folder stop being uploaded entirely.
        --
        -- Remembering the refusal lets the correlator step aside so that fallback can run.
        CREATE TABLE IF NOT EXISTS refused_moves (
            from_path TEXT NOT NULL,
            to_path TEXT NOT NULL,
            refused_at TEXT NOT NULL,
            PRIMARY KEY (from_path, to_path)
        );

        -- DBSYNC-99: the allocator behind `local_file_index.item_id`. AUTOINCREMENT is
        -- the point: SQLite keeps the high-water mark in `sqlite_sequence` and never
        -- reuses a rowid, so an identity belonging to a deleted file is never handed to
        -- a new one. The table itself stays empty — a row is inserted to claim a number
        -- and deleted in the same breath.
        CREATE TABLE IF NOT EXISTS item_id_seq (
            id INTEGER PRIMARY KEY AUTOINCREMENT
        );
        ",
    )?;

    // Additive migrations for databases created before a column existed.
    //
    // **These six run BEFORE the `sync_jobs` rebuild below, and that placement is a
    // precondition, not a preference.** The rebuild's `INSERT ... SELECT` names each of them
    // by hand, so they have to exist on the old table before it runs. Anything added to
    // `sync_jobs` AFTER the rebuild was written belongs in the second block, further down —
    // see the comment there for what happens when it lands here instead.
    add_column_if_missing(conn, "sync_jobs", "last_error", "TEXT")?;
    add_column_if_missing(conn, "sync_jobs", "upload_session_id", "TEXT")?;
    add_column_if_missing(conn, "sync_jobs", "upload_session_offset", "INTEGER")?;
    add_column_if_missing(conn, "sync_jobs", "upload_session_file_len", "INTEGER")?;
    add_column_if_missing(conn, "sync_jobs", "upload_session_file_mtime", "INTEGER")?;
    add_column_if_missing(conn, "sync_jobs", "delete_parent_rev", "TEXT")?;

    // DBSYNC-99: stable item identity. Three additive, nullable columns — no rebuild, so
    // nothing here can fail on an existing database. `remote_file_index.dropbox_id` and
    // `known_folders.dropbox_id` hold what Dropbox calls the item; `local_file_index.item_id`
    // holds what we call it, which is the only name an item has before its first successful
    // upload. Back-fill is free: `seed_remote_delta_cursor` already re-snapshots, and legacy
    // rows pick up an `item_id` the next time their path is indexed.
    add_column_if_missing(conn, "remote_file_index", "dropbox_id", "TEXT")?;
    add_column_if_missing(conn, "known_folders", "dropbox_id", "TEXT")?;
    add_column_if_missing(conn, "local_file_index", "item_id", "INTEGER")?;

    // ...and rows that predate the column get their identity here, rather than waiting to
    // be re-indexed.
    //
    // The writer mints an `item_id` when it touches a path, which covers every new or
    // changed file and nothing else. A file that simply sits there unchanged is never
    // written, so on a real database every pre-existing row stayed NULL — measured, not
    // supposed: seven of seven on the maintainer's own install after the first launch
    // carrying this schema. `dropbox_id` has the remote sweep as its trigger and this had
    // no equivalent, which would have left DBSYNC-95 an empty substrate to enumerate.
    //
    // Inside the migration transaction, so it is atomic with the column that makes it
    // possible, and idempotent: once every row has an identity the SELECT returns nothing
    // and this costs one scan of a small table per startup.
    let unidentified: Vec<String> = {
        let mut stmt =
            conn.prepare("SELECT relative_path FROM local_file_index WHERE item_id IS NULL")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    if !unidentified.is_empty() {
        for relative_path in &unidentified {
            // Same allocator as the writer, for the same reason: an identity belonging to a
            // deleted file must never be reissued, so it comes from the AUTOINCREMENT
            // sequence and never from `MAX + 1`.
            conn.execute("INSERT INTO item_id_seq DEFAULT VALUES", [])?;
            let item_id = conn.last_insert_rowid();
            conn.execute(
                "UPDATE local_file_index SET item_id = ?2 WHERE relative_path = ?1",
                params![relative_path, item_id],
            )?;
        }
        conn.execute("DELETE FROM item_id_seq", [])?;
        tracing::info!(
            count = unidentified.len(),
            "back-filled local item identities for rows that predate the column"
        );
    }

    // DBSYNC-35: structured fields for conflict resolution — the sibling copy holding
    // the preserved local content, and a flag for the remote-deleted scenario. Both
    // have constant defaults, so ADD COLUMN is safe on existing rows.
    add_column_if_missing(conn, "sync_conflicts", "conflicted_copy_path", "TEXT")?;
    add_column_if_missing(
        conn,
        "sync_conflicts",
        "remote_deleted",
        "INTEGER NOT NULL DEFAULT 0",
    )?;

    // DBSYNC-45: canonicalize path separators in the index tables to '/'. Rows
    // written from the local scan used OS-native '\' on Windows while remote/
    // Dropbox rows used '/', so cross-table lookups (e.g. get_remote_file with a
    // local key) missed and remote deletions of hydrated files never propagated.
    // For each table: drop any stale '\'-row whose normalized form already exists
    // (avoids a PRIMARY KEY collision on the UPDATE; the next sync tick re-upserts
    // it), then normalize the remaining '\'-rows. Idempotent.
    // NOTE: a literal '\' inside a SQL string is parsed unreliably by SQLite here,
    // so the backslash is referenced as char(92) throughout.
    for table in ["local_file_index", "remote_file_index", "known_folders"] {
        conn.execute(
            &format!(
                "DELETE FROM {table} WHERE instr(relative_path, char(92)) > 0 \
                 AND replace(relative_path, char(92), '/') IN \
                 (SELECT relative_path FROM {table} WHERE instr(relative_path, char(92)) = 0)"
            ),
            [],
        )?;
        conn.execute(
            &format!(
                "UPDATE {table} SET relative_path = replace(relative_path, char(92), '/') \
                 WHERE instr(relative_path, char(92)) > 0"
            ),
            [],
        )?;
    }

    // DBSYNC-31 (AC4): CHECK constraints on sync_jobs(job_type, status). SQLite can't
    // ALTER ... ADD CONSTRAINT, so rebuild the table once. Fresh DBs already get the
    // CHECKs from the CREATE TABLE above; a pre-existing table is rebuilt here. Guarded +
    // idempotent. Any row with an out-of-set value (shouldn't exist — both columns are
    // code-controlled) is dropped rather than aborting the copy. Runs BEFORE the index
    // creation below so the indexes land on the rebuilt table. Job rows are transient
    // (re-derived by the scan), so this is safe.
    //
    // DBSYNC-99 widened the guard, and the reason is worth keeping. It used to ask "does
    // the stored schema contain a CHECK at all", which was right exactly once: every
    // database in existence now has one, so adding `move` to the permitted set would have
    // been a silent no-op on every upgrade and `enqueue_job("move", ..)` would have failed
    // a constraint at runtime with the migration reporting success. The guard has to name
    // the member being added, not the mechanism. The next job type will need the same
    // treatment — the test is `migrate_widens_a_narrower_job_type_check`.
    let sync_jobs_check_admits_move = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='sync_jobs'",
            [],
            |r| r.get::<_, String>(0),
        )
        .map(|sql| sql.contains("CHECK") && sql.contains("'move'"))
        .unwrap_or(true);
    if !sync_jobs_check_admits_move {
        conn.execute_batch(
            "
            CREATE TABLE sync_jobs_new (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                job_type TEXT NOT NULL CHECK (job_type IN ('upload','download','delete','local_delete','hydrate_cloudsc','move')),
                source_path TEXT,
                target_path TEXT,
                status TEXT NOT NULL CHECK (status IN ('queued','running','retry_wait','done','failed')),
                attempt_count INTEGER NOT NULL DEFAULT 0,
                next_retry_at TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                last_error TEXT,
                upload_session_id TEXT,
                upload_session_offset INTEGER,
                upload_session_file_len INTEGER,
                upload_session_file_mtime INTEGER,
                delete_parent_rev TEXT
            );
            INSERT INTO sync_jobs_new
                SELECT id, job_type, source_path, target_path, status, attempt_count, next_retry_at,
                       created_at, updated_at, last_error, upload_session_id, upload_session_offset,
                       upload_session_file_len, upload_session_file_mtime, delete_parent_rev
                FROM sync_jobs
                WHERE status IN ('queued','running','retry_wait','done','failed')
                  AND job_type IN ('upload','download','delete','local_delete','hydrate_cloudsc','move');
            DROP TABLE sync_jobs;
            ALTER TABLE sync_jobs_new RENAME TO sync_jobs;
            ",
        )?;
    }

    // `sync_jobs` columns added AFTER the rebuild above was written, and therefore added
    // AFTER it runs.
    //
    // The rebuild's column list is frozen at DBSYNC-31. A column declared in the first
    // additive block is created, then dropped again by `DROP TABLE sync_jobs` on any legacy
    // database that still needs rebuilding — and the code that uses it fails at runtime with
    // `no such column` while the migration reports success. DBSYNC-99 put
    // `on_success_delete_path` there and this is where it came back from; the catch was
    // `migrate_rebuilds_a_legacy_sync_jobs_table_inside_the_transaction`, whose second-migrate
    // idempotence check saw the two runs disagree.
    //
    // Declaring them here instead of widening the rebuild removes the class rather than this
    // instance: the rebuild copies what it was written to copy, and everything since is
    // re-applied on top of whatever table survives. The next `sync_jobs` column goes here.
    //
    // DBSYNC-99: the source deletion an upload owes once its bytes have landed. Set only on
    // `upload` rows, and only by `enqueue_upload_then_delete`. See that function for why the
    // deletion is carried on the upload row rather than enqueued alongside it.
    add_column_if_missing(conn, "sync_jobs", "on_success_delete_path", "TEXT")?;
    add_column_if_missing(conn, "sync_jobs", "on_success_delete_rev", "TEXT")?;

    // DBSYNC-31: indexes for the hot job/conflict queries (previously full scans) and a
    // partial-unique guard so a path can never have two ACTIVE jobs of the same type.
    //
    // `sync_jobs(status, next_retry_at)` serves the drain query (WHERE status='queued'
    // OR (status='retry_wait' AND next_retry_at<=?)) and the active-job lookups.
    // `sync_conflicts(resolved)` serves the unresolved-conflict query.
    conn.execute_batch(
        "
        CREATE INDEX IF NOT EXISTS idx_sync_jobs_status_retry ON sync_jobs(status, next_retry_at);
        CREATE INDEX IF NOT EXISTS idx_sync_conflicts_resolved ON sync_conflicts(resolved);
        ",
    )?;

    // Dedup existing ACTIVE jobs before adding the unique index (a pre-existing
    // duplicate would make CREATE UNIQUE INDEX fail). Keep the lowest id per
    // (job_type, target_path); NULL target_path jobs (e.g. hydrate_cloudsc) are left
    // untouched — NULLs are distinct in a SQLite unique index, so they never conflict.
    conn.execute(
        "
        DELETE FROM sync_jobs
        WHERE status IN ('queued','retry_wait','running')
          AND target_path IS NOT NULL
          AND id NOT IN (
              SELECT MIN(id) FROM sync_jobs
              WHERE status IN ('queued','retry_wait','running') AND target_path IS NOT NULL
              GROUP BY job_type, target_path
          )
        ",
        [],
    )?;

    // DBSYNC-99: let the database enforce what the allocator is careful about. Two rows
    // sharing an `item_id` would make an identifier ambiguous, and DBSYNC-95 will resolve
    // identifiers to items. Partial, so the rows that predate the column and have not been
    // back-filled yet do not all collide on NULL.
    conn.execute(
        "
        CREATE UNIQUE INDEX IF NOT EXISTS idx_local_item_id
          ON local_file_index(item_id)
          WHERE item_id IS NOT NULL
        ",
        [],
    )?;

    // Only ONE active job per (job_type, target_path); DONE/failed history is exempt
    // (partial index), so `enqueue_job`'s ON CONFLICT collapses re-enqueues of the same
    // pending work instead of piling up duplicates.
    conn.execute(
        "
        CREATE UNIQUE INDEX IF NOT EXISTS idx_sync_jobs_active_unique
          ON sync_jobs(job_type, target_path)
          WHERE status IN ('queued','retry_wait','running')
        ",
        [],
    )?;

    Ok(())
}

/// Adds `column` to `table` if it isn't already present. Idempotent so it can run
/// on every startup without failing on databases that already have the column.
fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    decl: &str,
) -> AppResult<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut exists = false;
    let names = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for name in names {
        if name? == column {
            exists = true;
            break;
        }
    }
    if !exists {
        conn.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"),
            [],
        )?;
    }
    Ok(())
}

/// Platform whose data-dir convention to follow. Split out from the real OS so the
/// path logic below is deterministic and unit-testable on any build target.
///
/// Every variant is constructed on some platform (or in tests), but on any single
/// build target `current_data_dir_os()` only builds one of them, so `dead_code`
/// would otherwise flag the others as never-constructed.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DataDirOs {
    Windows,
    Macos,
    Unix,
}

fn current_data_dir_os() -> DataDirOs {
    #[cfg(target_os = "windows")]
    {
        DataDirOs::Windows
    }
    #[cfg(target_os = "macos")]
    {
        DataDirOs::Macos
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        DataDirOs::Unix
    }
}

/// Pure resolver for the app data directory, given the relevant env vars. Kept free
/// of env/FS access so every platform branch can be exercised in unit tests.
///
/// - Windows: `%LOCALAPPDATA%\DropboxSyncDesktop`
/// - macOS: `~/Library/Application Support/DropboxSyncDesktop`
/// - Linux/other Unix: `$XDG_DATA_HOME/DropboxSyncDesktop`, else `~/.local/share/DropboxSyncDesktop`
fn data_dir_for(
    os: DataDirOs,
    localappdata: Option<&str>,
    xdg_data_home: Option<&str>,
    home: Option<&str>,
) -> AppResult<PathBuf> {
    match os {
        DataDirOs::Windows => {
            let base = localappdata
                .filter(|s| !s.is_empty())
                .ok_or_else(|| AppError::Io("LOCALAPPDATA env var not found".into()))?;
            Ok(PathBuf::from(base).join("DropboxSyncDesktop"))
        }
        DataDirOs::Macos => {
            let home = home
                .filter(|s| !s.is_empty())
                .ok_or_else(|| AppError::Io("HOME env var not found".into()))?;
            Ok(PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("DropboxSyncDesktop"))
        }
        DataDirOs::Unix => {
            let base = match xdg_data_home.filter(|s| !s.is_empty()) {
                Some(v) => PathBuf::from(v),
                None => {
                    let home = home
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| AppError::Io("HOME env var not found".into()))?;
                    PathBuf::from(home).join(".local").join("share")
                }
            };
            Ok(base.join("DropboxSyncDesktop"))
        }
    }
}

fn resolve_app_data_dir() -> AppResult<PathBuf> {
    let localappdata = std::env::var("LOCALAPPDATA").ok();
    let xdg_data_home = std::env::var("XDG_DATA_HOME").ok();
    let home = std::env::var("HOME").ok();
    data_dir_for(
        current_data_dir_os(),
        localappdata.as_deref(),
        xdg_data_home.as_deref(),
        home.as_deref(),
    )
}

/// Shared app data directory (SQLite DB, overlay_state.json for shell extensions).
pub fn app_data_dir() -> AppResult<PathBuf> {
    let path = resolve_app_data_dir()?;
    std::fs::create_dir_all(&path)?;
    Ok(path)
}

#[cfg(test)]
mod data_dir_tests {
    use super::Db;

    /// `data_dir` must never be the empty path (DBSYNC-75). `Path::parent()` returns
    /// `Some("")` for a bare filename rather than `None` — verified, because the first
    /// version of this code assumed `None` and its fallback was therefore dead. An empty
    /// data dir would put `overlay_state.json` in the process's working directory, which is
    /// a quieter version of the very bug this ticket fixes.
    #[test]
    fn a_bare_filename_still_yields_a_usable_data_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Opened through a relative name, with the cwd irrelevant to the assertion: what
        // matters is that the recorded directory is usable, never "".
        let db = Db::new_at(&tmp.path().join("app.db")).expect("db");
        assert!(!db.data_dir().as_os_str().is_empty());
        assert_eq!(db.data_dir(), tmp.path());
    }
}

#[cfg(test)]
mod app_data_dir_tests {
    use super::{data_dir_for, DataDirOs};
    use std::path::PathBuf;

    #[test]
    fn windows_uses_localappdata() {
        let got = data_dir_for(
            DataDirOs::Windows,
            Some("C:\\Users\\u\\AppData\\Local"),
            None,
            None,
        )
        .expect("windows path");
        assert_eq!(
            got,
            PathBuf::from("C:\\Users\\u\\AppData\\Local").join("DropboxSyncDesktop")
        );
    }

    #[test]
    fn macos_uses_application_support_not_applications() {
        let got = data_dir_for(DataDirOs::Macos, None, None, Some("/Users/u")).expect("macos path");
        assert_eq!(
            got,
            PathBuf::from("/Users/u")
                .join("Library")
                .join("Application Support")
                .join("DropboxSyncDesktop")
        );
    }

    #[test]
    fn linux_prefers_xdg_data_home() {
        let got = data_dir_for(DataDirOs::Unix, None, Some("/custom/xdg"), Some("/home/u"))
            .expect("linux xdg path");
        assert_eq!(got, PathBuf::from("/custom/xdg").join("DropboxSyncDesktop"));
    }

    #[test]
    fn linux_falls_back_to_local_share() {
        // Missing and empty XDG_DATA_HOME both fall back to ~/.local/share.
        let expected = PathBuf::from("/home/u")
            .join(".local")
            .join("share")
            .join("DropboxSyncDesktop");
        assert_eq!(
            data_dir_for(DataDirOs::Unix, None, None, Some("/home/u")).unwrap(),
            expected
        );
        assert_eq!(
            data_dir_for(DataDirOs::Unix, None, Some(""), Some("/home/u")).unwrap(),
            expected
        );
    }

    #[test]
    fn missing_required_env_is_an_error() {
        assert!(data_dir_for(DataDirOs::Windows, None, None, None).is_err());
        assert!(data_dir_for(DataDirOs::Macos, None, None, None).is_err());
        assert!(data_dir_for(DataDirOs::Unix, None, None, None).is_err());
    }
}

fn db_path() -> AppResult<PathBuf> {
    let mut path = app_data_dir()?;
    path.push("app.db");
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::Db;
    use rusqlite::Connection;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// The whole schema as comparable rows — every object, its name, its **rootpage** and
    /// its DDL. Used to assert that a second `migrate` changes nothing, which is what
    /// "idempotent" means and what a bare `expect` on the second call does not check.
    ///
    /// `rootpage` is in there deliberately. Without it, making the `sync_jobs` rebuild
    /// unconditional — so every startup drops and recreates the table — left both
    /// idempotency tests green, because the recreated table has identical DDL. A recreated
    /// table gets a new rootpage, so including it turns "the schema looks the same" into
    /// "the schema IS the same objects".
    fn schema_rows(c: &Connection) -> Vec<String> {
        let mut stmt = c
            .prepare(
                "SELECT type || ' ' || name || ' ' || rootpage || ' ' || COALESCE(sql, '') \
                 FROM sqlite_master ORDER BY type, name",
            )
            .expect("prepare");
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .expect("query");
        rows.collect::<Result<Vec<_>, _>>().expect("collect")
    }

    // ── DBSYNC-104: a backslash is data, not a separator ──────────────────────

    /// The storage layer used to rewrite `\` to `/` in twelve accessors, so a key
    /// carrying a backslash never survived a round trip. Every producer now hands it
    /// an already-canonical key and it stores the bytes it is given.
    /// Unix-only: the premise is a filename containing a backslash, which cannot exist
    /// on Windows. There `debug_assert_canonical_key` correctly rejects such a key —
    /// it caught this very test on the first CI run, which is the guard working.
    #[cfg(not(windows))]
    #[test]
    fn a_key_containing_a_backslash_survives_a_round_trip() {
        let db = Db::new_at(&unique_db_path()).expect("db init");

        db.upsert_local_file("a\\b.txt", "H", 3, 0).expect("upsert");

        let row = db
            .get_local_file("a\\b.txt")
            .expect("query")
            .expect("the row must be readable under the key it was written with");
        assert_eq!(row.relative_path, "a\\b.txt");

        assert!(
            db.get_local_file("a/b.txt").expect("query").is_none(),
            "the backslash name must not be reachable under the genuine nested key"
        );
    }

    /// The CRITICAL defect. A root file literally named `a\b.txt` and the genuine
    /// `a/b.txt` inside folder `a` are two different files with two different
    /// contents. They used to collapse onto one `TEXT PRIMARY KEY`, alternating
    /// ownership of the row on every scan and each re-uploading over the other —
    /// silent content loss for whichever lost the race.
    /// Unix-only: the premise is a filename containing a backslash, which cannot exist
    /// on Windows. There `debug_assert_canonical_key` correctly rejects such a key —
    /// it caught this very test on the first CI run, which is the guard working.
    #[cfg(not(windows))]
    #[test]
    fn two_files_that_differ_only_by_a_backslash_are_two_rows() {
        let db = Db::new_at(&unique_db_path()).expect("db init");

        db.upsert_local_file("a\\b.txt", "HASH_WEIRD", 3, 0)
            .expect("upsert");
        db.upsert_local_file("a/b.txt", "HASH_GENUINE", 5, 0)
            .expect("upsert");

        let rows = db.list_local_files().expect("list");
        assert_eq!(rows.len(), 2, "two distinct files must be two rows");

        assert_eq!(
            db.get_local_file("a\\b.txt").unwrap().unwrap().hash,
            "HASH_WEIRD"
        );
        assert_eq!(
            db.get_local_file("a/b.txt").unwrap().unwrap().hash,
            "HASH_GENUINE",
            "neither file may overwrite the other's content"
        );
    }

    /// The destructive consequence, and the most valuable assertion in the slice.
    /// Deleting the unrelated folder `a` issues a recursive removal of the `a/`
    /// subtree. The root file named `a\b.txt` is not in that subtree — it only looked
    /// like it was, because the rewrite turned its backslash into a separator.
    /// Unix-only: the premise is a filename containing a backslash, which cannot exist
    /// on Windows. There `debug_assert_canonical_key` correctly rejects such a key —
    /// it caught this very test on the first CI run, which is the guard working.
    #[cfg(not(windows))]
    #[test]
    fn removing_a_subtree_spares_a_sibling_whose_name_merely_contains_a_backslash() {
        let db = Db::new_at(&unique_db_path()).expect("db init");

        db.upsert_remote_file("a\\b.txt", "HASH_WEIRD", "rev1", 0, None)
            .expect("upsert");
        db.upsert_remote_file("a/b.txt", "HASH_GENUINE", "rev2", 0, None)
            .expect("upsert");

        db.remove_remote_subtree("a").expect("remove subtree");

        assert!(
            db.get_remote_file("a/b.txt").unwrap().is_none(),
            "the genuine child of folder `a` is removed"
        );
        assert!(
            db.get_remote_file("a\\b.txt").unwrap().is_some(),
            "the root file named `a\\\\b.txt` is NOT inside folder `a` and must survive"
        );
    }

    /// Regression guard. `move_index_subtree` carries a history — its byte-vs-character
    /// `substr` offset silently mangled non-ASCII child paths (DBSYNC-99 round 13) — so
    /// prove the ordinary subtree move still works after the rewrite was removed.
    #[test]
    fn moving_a_subtree_still_moves_every_child() {
        let db = Db::new_at(&unique_db_path()).expect("db init");

        db.upsert_local_file("d/one.txt", "H1", 3, 0)
            .expect("upsert");
        db.upsert_local_file("d/sub/two.txt", "H2", 3, 0)
            .expect("upsert");
        // A sibling sharing the prefix must NOT be swept along.
        db.upsert_local_file("d-other/three.txt", "H3", 3, 0)
            .expect("upsert");

        let stranded = db.move_index_subtree("d", "e").expect("move subtree");
        assert_eq!(
            stranded, 0,
            "no row may be left behind under the old prefix"
        );

        assert!(db.get_local_file("e/one.txt").unwrap().is_some());
        assert!(db.get_local_file("e/sub/two.txt").unwrap().is_some());
        assert!(db.get_local_file("d/one.txt").unwrap().is_none());
        assert!(
            db.get_local_file("d-other/three.txt").unwrap().is_some(),
            "a prefix-sharing sibling must not be moved"
        );
    }

    /// A unique temp DB file path so tests never touch the production database.
    ///
    /// The counter is not decoration. `as_nanos()` has **microsecond** resolution on macOS
    /// — measured: 192393 of 200000 consecutive readings were duplicates, smallest non-zero
    /// gap 1000ns — so two tests entering this function in the same microsecond got the
    /// same directory and shared one database file. Most callers survived that because
    /// `Db::new_at` is all `CREATE TABLE IF NOT EXISTS`; `migrate_rebuilds_a_legacy_...`
    /// plants a bare `CREATE TABLE` and dies on the collision. The bug is older than that
    /// test — the test is just the first caller intolerant enough to expose it, at roughly
    /// one failed run in ten under the default parallel harness, and none single-threaded.
    fn unique_db_path() -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("dropbox-sync-test-{ts}-{n}"));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir.join("app.db")
    }

    #[test]
    fn persists_local_file_index_and_jobs() {
        let db = Db::new_at(&unique_db_path()).expect("db init");
        db.set_sync_folder("/tmp/folder").expect("set folder");
        db.upsert_local_file("a.txt", "abc123", 10, 123)
            .expect("upsert");
        db.enqueue_job("upload", Some("a.txt"), Some("a.txt"))
            .expect("enqueue");

        let files = db.list_local_files().expect("files");
        let jobs = db.list_recent_jobs(10).expect("jobs");
        let active = db.count_active_jobs().expect("count");

        assert_eq!(files.len(), 1);
        assert_eq!(jobs.len(), 1);
        assert_eq!(active, 1);
        assert_eq!(files[0].relative_path, "a.txt");
        assert_eq!(jobs[0].status, "queued");
    }

    #[test]
    fn enqueue_dedups_active_jobs_but_not_history() {
        let db = Db::new_at(&unique_db_path()).expect("db init");

        // DBSYNC-31: two enqueues of the same (job_type, target_path) collapse into ONE
        // active job (partial-unique index + ON CONFLICT), instead of two rows.
        db.enqueue_job("upload", Some("a.txt"), Some("a.txt"))
            .unwrap();
        db.enqueue_job("upload", Some("a.txt"), Some("a.txt"))
            .unwrap();
        assert_eq!(
            db.count_active_jobs().unwrap(),
            1,
            "duplicate active upload collapsed"
        );

        // A different job_type for the same path is a distinct active job.
        db.enqueue_job("delete", Some("a.txt"), Some("a.txt"))
            .unwrap();
        assert_eq!(db.count_active_jobs().unwrap(), 2);

        // active_job_paths reports the path (used for dedup / conflict routing).
        assert!(db.active_job_paths().unwrap().contains("a.txt"));

        // DONE jobs are exempt from the partial index: completing the upload lets a fresh
        // upload for the same path be enqueued (history is preserved, not overwritten).
        let upload_id = db
            .list_recent_jobs(50)
            .unwrap()
            .into_iter()
            .find(|j| j.job_type == "upload")
            .unwrap()
            .id;
        db.mark_job_completed(upload_id).unwrap();
        db.enqueue_job("upload", Some("a.txt"), Some("a.txt"))
            .unwrap();
        let uploads = db
            .list_recent_jobs(50)
            .unwrap()
            .into_iter()
            .filter(|j| j.job_type == "upload")
            .count();
        assert_eq!(
            uploads, 2,
            "a new active upload coexists with the completed one"
        );
    }

    /// DBSYNC-99. `peek_deferred_source_delete` restricts itself to `upload` rows, and that
    /// restriction has to be a guard rather than a comment.
    ///
    /// Nothing writes the column onto another job type today — `enqueue_upload_then_delete`
    /// is the only writer and its `ON CONFLICT` target is the partial-unique index on
    /// `(job_type, target_path)`, so a collapse is necessarily with another `upload`. That is
    /// exactly why this needs a test: the guard is unreachable through the public API, so
    /// removing it broke nothing and survived a full mutation run. What it defends against is
    /// a future writer, a migration, or a bug putting a path there — and then a `delete` job
    /// completing would enqueue a deletion of whatever that field named.
    #[test]
    fn a_deferred_deletion_on_a_non_upload_row_is_not_honoured() {
        let path = unique_db_path();
        let db = Db::new_at(&path).expect("db init");
        db.enqueue_delete_job("victim-carrier.txt", None)
            .expect("enqueue");
        let job_id = db.list_recent_jobs(10).expect("jobs")[0].id;

        // Plant the column on a `delete` row, which no code path does — that is the point.
        Connection::open(&path)
            .expect("raw open")
            .execute(
                "UPDATE sync_jobs SET on_success_delete_path = ?2 WHERE id = ?1",
                rusqlite::params![job_id, "innocent.txt"],
            )
            .expect("plant");

        assert_eq!(
            db.peek_deferred_source_delete(job_id).expect("peek"),
            None,
            "only an upload can owe a deletion; honouring this row would delete a path \
             because a field was set on the wrong job"
        );
    }

    /// `reset_sync_state` must clear every table the index owns, including the ones added
    /// later. A surviving refusal names relative paths, so after a folder change a colliding
    /// pair suppresses a legitimate rename correlation in the NEW folder.
    #[test]
    fn reset_sync_state_clears_refused_moves_too() {
        let db = Db::new_at(&unique_db_path()).expect("db init");
        db.set_sync_folder("/tmp/whatever").unwrap();
        db.record_refused_move("Docs", "Papers").unwrap();
        db.upsert_local_file("a.txt", "H", 1, 0).unwrap();

        db.reset_sync_state().expect("reset");

        assert!(
            db.list_refused_moves().unwrap().is_empty(),
            "a table added after this function was written is exactly the one that gets \
             forgotten here"
        );
        assert!(db.list_local_files().unwrap().is_empty());
    }

    /// A non-ASCII folder rename must not mangle its subtree.
    ///
    /// The offset used to be `old_prefix.len()` — Rust BYTES — fed to SQLite's `substr`, which
    /// counts CHARACTERS. Two shapes, both silent, because `stranded` counts rows left UNDER
    /// the old prefix and a mangled row is not under it:
    ///
    /// - overshoot smaller than the tail: the `/` is eaten (`artículos/a.txt` → `Papersa.txt`)
    /// - overshoot larger than the tail: `substr` returns `''` and the child row COLLAPSES onto
    ///   the prefix (`我的文档/a.txt` → `Docs`), which then names a real directory and is taken
    ///   for a deleted file by the next full scan — a RECURSIVE `delete_v2` of the folder just
    ///   renamed.
    ///
    /// Both cases are here on purpose: the accented one alone cannot see the collapse.
    #[test]
    fn move_index_subtree_is_character_safe_for_multibyte_prefixes() {
        for (old, new, child) in [
            ("artículos", "Papers", "artículos/a.txt"),
            ("我的文档", "Docs", "我的文档/a.txt"),
            ("Ñoño", "Plain", "Ñoño/deep/b.txt"),
        ] {
            let db = Db::new_at(&unique_db_path()).expect("db init");
            db.upsert_known_folder(old).unwrap();
            db.upsert_local_file(child, "H", 1, 0).unwrap();
            db.upsert_remote_file(child, "H", "rev", 0, None).unwrap();

            let stranded = db.move_index_subtree(old, new).expect("move subtree");

            let tail = &child[old.len() + 1..];
            let expected = format!("{new}/{tail}");
            let locals: Vec<String> = db
                .list_local_files()
                .unwrap()
                .into_iter()
                .map(|r| r.relative_path)
                .collect();
            assert_eq!(
                locals,
                vec![expected.clone()],
                "prefix {old:?}: the child must land at {expected:?}, not be mangled"
            );
            assert!(
                db.get_remote_file(&expected).unwrap().is_some(),
                "prefix {old:?}: the remote row must travel too"
            );
            assert_eq!(stranded, 0, "prefix {old:?}: nothing may be left behind");
        }
    }

    /// The subtree match is BINARY: `LIKE` is ASCII case-insensitive and this one DELETES.
    #[test]
    fn remove_remote_subtree_does_not_match_a_different_case() {
        let db = Db::new_at(&unique_db_path()).expect("db init");
        db.upsert_remote_file("Docs/keep.txt", "H", "rev", 0, None)
            .unwrap();
        db.upsert_remote_file("docs/other.txt", "H", "rev", 0, None)
            .unwrap();

        db.remove_remote_subtree("Docs").unwrap();

        assert!(db.get_remote_file("Docs/keep.txt").unwrap().is_none());
        assert!(
            db.get_remote_file("docs/other.txt").unwrap().is_some(),
            "a differently-cased sibling subtree must survive — this statement deletes"
        );
    }

    #[test]
    fn enqueue_delete_job_persists_parent_rev() {
        let db = Db::new_at(&unique_db_path()).expect("db init");
        db.enqueue_delete_job("a.txt", Some("rev123"))
            .expect("enqueue");

        let jobs = db.list_recent_jobs(10).expect("jobs");
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].job_type, "delete");
        assert_eq!(jobs[0].target_path.as_deref(), Some("a.txt"));
        assert_eq!(jobs[0].delete_parent_rev.as_deref(), Some("rev123"));
    }

    #[test]
    fn enqueue_delete_job_on_conflict_updates_rev_not_just_source_path() {
        let db = Db::new_at(&unique_db_path()).expect("db init");
        // DBSYNC-65 (Slice 1) crux regression: re-enqueuing an already-active delete
        // for the same target must refresh `delete_parent_rev`, not keep the stale
        // value captured by the first enqueue.
        db.enqueue_delete_job("a.txt", Some("old_rev"))
            .expect("first enqueue");
        db.enqueue_delete_job("a.txt", Some("new_rev"))
            .expect("second enqueue");

        assert_eq!(
            db.count_active_jobs().unwrap(),
            1,
            "re-enqueuing the same delete target must collapse into one active job"
        );
        let jobs = db.list_recent_jobs(10).expect("jobs");
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].delete_parent_rev.as_deref(), Some("new_rev"));
    }

    #[test]
    fn check_constraint_rejects_invalid_job_type() {
        let db = Db::new_at(&unique_db_path()).expect("db init");
        // DBSYNC-31 AC4: the CHECK constraint rejects an unknown job_type at the DB layer.
        assert!(
            db.enqueue_job("bogus_type", Some("a.txt"), Some("a.txt"))
                .is_err(),
            "an out-of-set job_type must be rejected"
        );
        // A valid job_type still enqueues.
        db.enqueue_job("upload", Some("a.txt"), Some("a.txt"))
            .unwrap();
        assert_eq!(db.count_active_jobs().unwrap(), 1);
    }

    #[test]
    fn recover_running_jobs_resets_to_queued_with_clean_attempts() {
        let db = Db::new_at(&unique_db_path()).expect("db init");
        db.enqueue_job("upload", Some("big.bin"), Some("big.bin"))
            .expect("enqueue");

        let job = db.pick_next_due_job().expect("pick").expect("some job");
        assert_eq!(job.status, "queued"); // status pre-update snapshot returned by pick

        let jobs_before = db.list_recent_jobs(10).expect("jobs");
        assert_eq!(jobs_before[0].status, "running");

        let recovered = db.recover_running_jobs().expect("recover");
        assert_eq!(recovered, 1);

        let jobs_after = db.list_recent_jobs(10).expect("jobs");
        assert_eq!(jobs_after[0].status, "queued");
        assert_eq!(jobs_after[0].attempt_count, 0);
        assert!(jobs_after[0].next_retry_at.is_none());
    }

    #[test]
    fn recover_running_jobs_preserves_upload_checkpoint() {
        let db = Db::new_at(&unique_db_path()).expect("db init");
        db.enqueue_job("upload", Some("big.bin"), Some("big.bin"))
            .expect("enqueue");
        let job = db.pick_next_due_job().expect("pick").expect("some job");

        db.save_upload_checkpoint(job.id, "sess-abc", 123456, 999_999_999, 1_700_000_000)
            .expect("save checkpoint");

        db.recover_running_jobs().expect("recover");

        let checkpoint = db.get_upload_checkpoint(job.id).expect("get checkpoint");
        assert_eq!(
            checkpoint,
            Some(("sess-abc".to_string(), 123456, 999_999_999, 1_700_000_000))
        );
    }

    #[test]
    fn upload_checkpoint_round_trip() {
        let db = Db::new_at(&unique_db_path()).expect("db init");
        db.enqueue_job("upload", Some("big.bin"), Some("big.bin"))
            .expect("enqueue");
        let job = db.pick_next_due_job().expect("pick").expect("some job");

        assert_eq!(db.get_upload_checkpoint(job.id).expect("get"), None);

        db.save_upload_checkpoint(
            job.id,
            "sess-1",
            8 * 1024 * 1024,
            10 * 1024 * 1024,
            1_700_000_000,
        )
        .expect("save");
        assert_eq!(
            db.get_upload_checkpoint(job.id).expect("get"),
            Some((
                "sess-1".to_string(),
                8 * 1024 * 1024,
                10 * 1024 * 1024,
                1_700_000_000
            ))
        );

        db.clear_upload_checkpoint(job.id).expect("clear");
        assert_eq!(db.get_upload_checkpoint(job.id).expect("get"), None);
    }

    #[test]
    fn upload_checkpoint_round_trips_file_identity_for_resume_guard() {
        // This is a pure DB round-trip test: the actual "refuse to resume when
        // identity differs" guard lives in `dropbox_transfer::upload_via_session`
        // (it compares the returned file_len/file_mtime against the file currently
        // being uploaded). Here we just verify the stored identity values are
        // exactly what was saved, including when they differ between two saves,
        // since that's what the resume guard depends on being accurate.
        let db = Db::new_at(&unique_db_path()).expect("db init");
        db.enqueue_job("upload", Some("big.bin"), Some("big.bin"))
            .expect("enqueue");
        let job = db.pick_next_due_job().expect("pick").expect("some job");

        db.save_upload_checkpoint(job.id, "sess-a", 100, 5_000, 1_700_000_000)
            .expect("save first");
        let first = db.get_upload_checkpoint(job.id).expect("get first");
        assert_eq!(
            first,
            Some(("sess-a".to_string(), 100, 5_000, 1_700_000_000))
        );

        // Simulate the file changing underneath the job (different len and mtime):
        // a fresh session checkpoint overwrites the old identity entirely.
        db.save_upload_checkpoint(job.id, "sess-b", 0, 6_000, 1_800_000_000)
            .expect("save second");
        let second = db.get_upload_checkpoint(job.id).expect("get second");
        assert_eq!(
            second,
            Some(("sess-b".to_string(), 0, 6_000, 1_800_000_000))
        );
        assert_ne!(first, second);
    }

    #[test]
    fn known_folders_upsert_list_remove_round_trip() {
        let db = Db::new_at(&unique_db_path()).expect("db init");
        db.upsert_known_folder("Cocina/Test").expect("upsert 1");
        db.upsert_known_folder("Cocina/Otra").expect("upsert 2");

        let mut folders = db.list_known_folders().expect("list");
        folders.sort();
        assert_eq!(
            folders,
            vec!["Cocina/Otra".to_string(), "Cocina/Test".to_string()]
        );

        db.remove_known_folder("Cocina/Otra").expect("remove");
        let folders = db.list_known_folders().expect("list after remove");
        assert_eq!(folders, vec!["Cocina/Test".to_string()]);
    }

    #[test]
    fn remove_remote_subtree_clears_prefix_and_descendants_boundary_safe() {
        let db = Db::new_at(&unique_db_path()).expect("db init");
        // The folder itself, a descendant (with an accent + a space), and two
        // rows that must NOT be touched: a boundary-collision sibling and an
        // unrelated tree.
        db.upsert_remote_file("UNET", "h", "r", 0, None)
            .expect("u1");
        db.upsert_remote_file("UNET/Ascensos/artículos/a b.pdf", "h", "r", 0, None)
            .expect("u2");
        db.upsert_remote_file("UNET-other/keep.txt", "h", "r", 0, None)
            .expect("u3");
        db.upsert_remote_file("Otra/keep.txt", "h", "r", 0, None)
            .expect("u4");

        db.remove_remote_subtree("UNET").expect("prune subtree");

        assert!(db.get_remote_file("UNET").expect("g1").is_none());
        assert!(db
            .get_remote_file("UNET/Ascensos/artículos/a b.pdf")
            .expect("g2")
            .is_none());
        // Boundary-collision sibling and unrelated tree survive.
        assert!(db
            .get_remote_file("UNET-other/keep.txt")
            .expect("g3")
            .is_some());
        assert!(db.get_remote_file("Otra/keep.txt").expect("g4").is_some());
    }

    #[test]
    fn reset_sync_state_clears_known_folders() {
        let db = Db::new_at(&unique_db_path()).expect("db init");
        db.upsert_known_folder("Cocina/Test").expect("upsert");
        assert_eq!(db.list_known_folders().expect("list").len(), 1);

        db.reset_sync_state().expect("reset");

        assert!(db
            .list_known_folders()
            .expect("list after reset")
            .is_empty());
    }

    #[test]
    fn disconnect_clears_sync_folder_but_keeps_local_prefs() {
        let db = Db::new_at(&unique_db_path()).expect("db init");
        db.set_sync_folder("/tmp/folder").expect("set folder");
        db.set_include_prefixes_csv("Fotos,Videos/2024")
            .expect("set include prefixes");

        // Mirror the `disconnect_dropbox` command's DB-side clears.
        db.reset_sync_state().expect("reset");
        db.clear_sync_folder().expect("clear sync folder");

        assert_eq!(db.get_sync_folder().expect("get after clear"), None);
        assert_eq!(
            db.get_include_prefixes_csv()
                .expect("get prefixes after clear"),
            Some("Fotos,Videos/2024".to_string()),
            "local prefs must survive disconnect"
        );
    }

    /// DBSYNC-40, and this is the whole evidence for the ticket: a failure partway through
    /// `reset_sync_state` must leave the database untouched.
    ///
    /// The lever is `DROP TABLE known_folders` — the **fifth** of six deletions, so the
    /// first four certainly execute before the failure. Without the transaction,
    /// `local_file_index` is empty when this returns and the assertion below fails. A test
    /// that only checked the happy path would pass either way, which is the definition of a
    /// check that cannot fail.
    #[test]
    fn reset_sync_state_rolls_back_when_a_deletion_fails_partway() {
        let db = Db::new_at(&unique_db_path()).expect("db init");
        db.upsert_local_file("a.txt", "H", 1, 0)
            .expect("seed local");
        db.upsert_remote_file("a.txt", "H", "rev", 0, None)
            .expect("seed remote");
        db.upsert_known_folder("sub").expect("seed folder");

        // The lever fires ONLY IF an earlier deletion has already run inside the
        // transaction, and that is the whole point of using a trigger rather than the
        // obvious `DROP TABLE known_folders`.
        //
        // A dropped table cannot tell "deleted then rolled back" from "never executed".
        // Reorder the `known_folders` deletion to the front of `reset_sync_state` — a
        // plausible edit, nothing about the function forbids it — and the surviving-row
        // assertions below become trivially true, so the test passes with no rollback
        // exercised at all. Worse, measured: with that reordering the mutation that removes
        // the transaction ALSO stops reddening. The test the ticket calls its whole
        // evidence, and the mutation that validates it, are disarmed together by moving one
        // line inside the function under test.
        //
        // This trigger aborts only once `local_file_index` is empty, so the abort IS the
        // proof that deletion 1 ran. The assertions then prove it was undone.
        {
            let conn = db.write.lock().expect("lock");
            conn.execute_batch(
                "CREATE TRIGGER abort_once_local_is_cleared \
                 BEFORE DELETE ON known_folders BEGIN \
                   SELECT RAISE(ABORT, 'local_file_index was already cleared') \
                   WHERE (SELECT COUNT(*) FROM local_file_index) = 0; \
                 END",
            )
            .expect("install the ordering-sensitive lever");
        }

        let err = db
            .reset_sync_state()
            .expect_err("the reset must fail, not silently skip");
        assert!(
            err.to_string()
                .contains("local_file_index was already cleared"),
            "the failure must come from the ordering-sensitive lever, which fires only \
             after an earlier deletion ran: {err}"
        );
        assert!(
            db.get_local_file("a.txt").expect("query").is_some(),
            "the FIRST deletion must have been rolled back — a half-cleared index is what \
             this ticket exists to prevent"
        );
        assert!(
            db.get_remote_file("a.txt").expect("query").is_some(),
            "and so must the second"
        );
    }

    /// DBSYNC-40. `reset_sync_state` must clear **every** table it names, and this test
    /// exists because a systematic sweep found that three of its six deletions could be
    /// deleted outright with the whole suite still green — including `remote_file_index`,
    /// the one this function's own doc comment names as the disaster case:
    ///
    /// > *"clear `local_file_index` but not `remote_file_index`, and the next scan walks a
    /// > folder full of files with no index rows while the remote index still claims to
    /// > know them"*
    ///
    /// The rollback test next door reads like it covers this — it asserts the remote row
    /// survives a failed reset — but that assertion passes just as happily when the remote
    /// deletion never runs at all. "Rolled back" and "never executed" look identical from
    /// the outside, which is the same confusion the trigger lever exists to resolve.
    #[test]
    fn reset_sync_state_clears_every_table_it_names() {
        let db = Db::new_at(&unique_db_path()).expect("db init");

        // Seeded through raw SQL, and deliberately including rows the application's own
        // readers cannot see: a `done` and a `failed` job (`count_active_jobs` counts only
        // queued/retry_wait/running), a resolved conflict (`list_recent_conflicts` filters
        // `resolved = 0`), and a rescan-marked index row (DBSYNC-56 stores `hash = ''`, and
        // `get_local_file` is queried here by a different path).
        //
        // That is the whole point. A first version of this test asserted through those
        // readers, and every deletion could then be narrowed to exactly what its reader
        // shows — `DELETE FROM sync_jobs WHERE status IN ('queued','retry_wait','running')`
        // left the suite green, and so did the equivalents for conflicts and the index.
        // Each is a plausible edit someone makes on purpose ("keep the history"), and the
        // assertion messages would still have read "jobs", "conflicts", "local".
        {
            let conn = db.write.lock().expect("lock");
            conn.execute_batch(
                // Columns are named rather than positional: a bare `VALUES (...)` binds by
                // ordinal, so adding a column to any of these tables breaks the seed with a
                // count mismatch that has nothing to do with what the test asserts.
                "INSERT INTO local_file_index (relative_path,hash,size_bytes,modified_ts,updated_at)
                     VALUES ('a.txt','H',1,0,'t'), ('marked.txt','',1,0,'t');
                 INSERT INTO remote_file_index (relative_path,content_hash,rev,modified_ts,updated_at)
                     VALUES ('a.txt','H','rev',0,'t');
                 INSERT INTO sync_jobs (job_type,status,created_at,updated_at)
                     VALUES ('upload','queued','t','t'),
                            ('upload','done','t','t'),
                            ('upload','failed','t','t');
                 INSERT INTO sync_conflicts (local_path,remote_path,reason,resolved,created_at)
                     VALUES ('a.txt','a.txt','unresolved',0,'t'),
                            ('b.txt','b.txt','resolved',1,'t');
                 INSERT INTO known_folders (relative_path,updated_at) VALUES ('sub','t');",
            )
            .expect("seed");
        }
        db.set_app_config(crate::remote_index::REMOTE_DELTA_CURSOR_KEY, "cursor")
            .expect("seed cursor");

        db.reset_sync_state().expect("reset");

        // Raw COUNT(*) per table, for the same reason: a filtered reader would let a
        // narrowed deletion pass.
        let conn = db.write.lock().expect("lock");
        for table in [
            "local_file_index",
            "remote_file_index",
            "sync_jobs",
            "sync_conflicts",
            "known_folders",
        ] {
            let n: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .expect("count");
            assert_eq!(n, 0, "{table} must be empty after a reset");
        }
        // The cursor goes; other app_config keys stay — that distinction is deliberate in
        // the production code, so assert both halves.
        let cursor: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM app_config WHERE key = ?1",
                [crate::remote_index::REMOTE_DELTA_CURSOR_KEY],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(cursor, 0, "the delta cursor must be dropped");
    }

    /// DBSYNC-40. Same requirement for the schema: a failure partway leaves nothing behind.
    ///
    /// The lever is a **table** named `idx_sync_jobs_status_retry`, colliding with an index
    /// `migrate` creates near the end — SQLite reports "there is already a table named …"
    /// even for `CREATE INDEX IF NOT EXISTS`. `app_config` is created by the very first
    /// statement, so its absence afterwards is what proves the whole batch unwound.
    ///
    /// **That last step rests on an ordering this test cannot check**, and saying so is the
    /// point: it assumes `CREATE TABLE app_config` runs before the failure. Move it after
    /// the index block and the assertion passes vacuously. The reset test solves the same
    /// problem with a trigger that fires only after an earlier statement ran, but SQLite has
    /// no DDL trigger hook, and the failing statement here is a `CREATE INDEX` — so there is
    /// no way *in SQL* to make this failure conditional on `app_config` already existing —
    /// a `sqlite3_set_authorizer` hook could, at far more cost than this residual is worth.
    /// The
    /// ordering is verified by reading `migrate`, not by this test. Accepted knowingly:
    /// it would take reordering a `CREATE TABLE` behind an index creation to break it.
    #[test]
    fn migrate_rolls_back_when_a_step_fails_partway() {
        let path = unique_db_path();
        let mut conn = Connection::open(&path).expect("open");
        conn.execute_batch("CREATE TABLE idx_sync_jobs_status_retry (x)")
            .expect("plant the collision");

        let err = super::migrate(&mut conn).expect_err("the collision must fail the migration");

        // Pin WHERE it failed. Without this the test passes vacuously: if some future edit
        // makes an EARLIER step fail — a new first statement, a reordering, a typo in the
        // opening batch — then `app_config` was never created, its absence is trivially
        // true, and this becomes a check that cannot fail while looking green. Verified by
        // forcing exactly that (a stray VIEW named `sync_jobs` breaks the first
        // `execute_batch`): both assertions below held with no atomicity involved at all.
        assert!(
            err.to_string().contains("idx_sync_jobs_status_retry"),
            "the migration must fail at the planted collision, not earlier: {err}"
        );
        let app_config_exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='app_config'",
                [],
                |r| r.get(0),
            )
            .expect("query");
        assert_eq!(
            app_config_exists, 0,
            "app_config is created by the first statement; if it survives a later failure \
             the migration was not atomic"
        );
    }

    /// DBSYNC-40. Running `migrate` twice is the normal case on every restart, so a
    /// transaction that accidentally broke a step's re-runnability would break every
    /// existing installation. Compares the WHOLE schema, not one table.
    #[test]
    fn migrate_is_idempotent_and_leaves_an_identical_schema() {
        let path = unique_db_path();
        let mut conn = Connection::open(&path).expect("open");

        super::migrate(&mut conn).expect("first migrate");
        let after_first = schema_rows(&conn);
        super::migrate(&mut conn).expect("second migrate must not fail");
        let after_second = schema_rows(&conn);

        assert_eq!(after_first, after_second);
        assert!(
            after_first.iter().any(|s| s.contains("app_config")),
            "sanity: the schema comparison must be comparing something"
        );
    }

    /// DBSYNC-99. The first version of the allocator was `MAX(item_id) + 1`, which is
    /// wrong in a way the other identity tests cannot see: rows are deleted here on every
    /// local file deletion, so the maximum falls back and the next file is handed the
    /// identity of a file that no longer exists. Anything still holding the old
    /// identifier would resolve it to the wrong item. This is the test that catches it.
    #[test]
    fn an_identity_is_never_handed_to_a_second_file() {
        let db = Db::new_at(&unique_db_path()).expect("db init");

        db.upsert_local_file("first.txt", "H", 1, 0).expect("first");
        let first = db
            .get_local_file("first.txt")
            .expect("get")
            .unwrap()
            .item_id
            .expect("identity");

        // The highest identity in the table now belongs to a file that is about to go.
        db.remove_local_file("first.txt").expect("remove");
        db.upsert_local_file("second.txt", "H", 1, 0)
            .expect("second");

        let second = db
            .get_local_file("second.txt")
            .expect("get")
            .unwrap()
            .item_id
            .expect("identity");
        assert_ne!(
            second, first,
            "a deleted file's identity must never be reissued"
        );
    }

    /// DBSYNC-99. `item_id` is minted by the writer rather than by a back-fill migration,
    /// so a row that predates the column must pick one up the next time its path is
    /// indexed. Seeded through raw SQL because no public writer can produce a row without
    /// an identity — which is the property being relied on.
    #[test]
    fn a_row_written_before_item_id_existed_adopts_one_when_reindexed() {
        let db = Db::new_at(&unique_db_path()).expect("db init");
        {
            let conn = db.write.lock().expect("lock");
            conn.execute_batch(
                "INSERT INTO local_file_index (relative_path,hash,size_bytes,modified_ts,updated_at)
                     VALUES ('legacy.txt','H',1,0,'t');",
            )
            .expect("seed");
        }
        assert_eq!(
            db.get_local_file("legacy.txt")
                .expect("get")
                .unwrap()
                .item_id,
            None,
            "the seeded row must start without an identity, or this proves nothing"
        );

        db.upsert_local_file("legacy.txt", "H2", 2, 3)
            .expect("reindex");

        let row = db.get_local_file("legacy.txt").expect("get").unwrap();
        assert!(row.item_id.is_some(), "reindexing must mint an identity");
        assert_eq!(row.hash, "H2", "and must still record the content change");

        // A second path must not be handed the same number.
        db.upsert_local_file("other.txt", "H", 1, 0).expect("other");
        assert_ne!(
            db.get_local_file("other.txt")
                .expect("get")
                .unwrap()
                .item_id,
            row.item_id
        );
    }

    /// DBSYNC-99. `d` and `d-other` share a prefix, so a subtree rewrite that matches on the
    /// bare prefix drags the sibling along with it. `remove_remote_subtree` already carries
    /// a test for exactly this shape; the rewrite needs its own, because it corrupts paths
    /// rather than deleting rows, which is quieter and therefore worse.
    #[test]
    fn move_index_subtree_rewrites_the_subtree_and_leaves_siblings_alone() {
        let db = Db::new_at(&unique_db_path()).expect("db init");

        db.upsert_known_folder("d").expect("folder");
        db.upsert_local_file("d", "H", 0, 0).expect("d itself");
        db.upsert_local_file("d/one.txt", "H1", 1, 0).expect("one");
        db.upsert_local_file("d/sub/two.txt", "H2", 1, 0)
            .expect("two");
        db.upsert_remote_file("d/one.txt", "H1", "rev", 0, Some("id:ONE"))
            .expect("remote");
        // Siblings that must not move: a prefix neighbour and an unrelated path.
        db.upsert_local_file("d-other/keep.txt", "HK", 1, 0)
            .expect("sibling");
        db.upsert_local_file("elsewhere.txt", "HE", 1, 0)
            .expect("other");
        let identity = db
            .get_local_file("d/sub/two.txt")
            .expect("get")
            .unwrap()
            .item_id
            .expect("identity");

        db.move_index_subtree("d", "e").expect("move subtree");

        let paths: Vec<String> = db
            .list_local_files()
            .expect("list")
            .into_iter()
            .map(|r| r.relative_path)
            .collect();
        assert_eq!(
            paths,
            vec![
                "d-other/keep.txt".to_string(),
                "e".to_string(),
                "e/one.txt".to_string(),
                "e/sub/two.txt".to_string(),
                "elsewhere.txt".to_string(),
            ],
            "the subtree moves whole, and only the subtree"
        );
        assert_eq!(
            db.get_local_file("e/sub/two.txt")
                .expect("get")
                .unwrap()
                .item_id,
            Some(identity),
            "identity travels with every descendant, however deep"
        );
        assert_eq!(
            db.get_remote_file("e/one.txt")
                .expect("get")
                .unwrap()
                .dropbox_id
                .as_deref(),
            Some("id:ONE"),
            "and so does the Dropbox identifier"
        );
        assert_eq!(db.list_known_folders().expect("folders"), vec!["e"]);
    }

    /// DBSYNC-99. The writer mints an identity when it touches a path, which covers every
    /// new or changed file and nothing else — a file that simply sits there unchanged is
    /// never written. On a real install that left **every** pre-existing row without one,
    /// so `dropbox_id` (which has the remote sweep as its trigger) filled in while
    /// `item_id` did not. Migration is the trigger it was missing.
    #[test]
    fn migrate_gives_an_identity_to_rows_that_predate_the_column() {
        let path = unique_db_path();
        let db = Db::new_at(&path).expect("db init");
        {
            // Raw SQL because no public writer can produce a row without an identity —
            // which is the property this back-fill exists to repair.
            let conn = db.write.lock().expect("lock");
            conn.execute_batch(
                "INSERT INTO local_file_index (relative_path,hash,size_bytes,modified_ts,updated_at)
                     VALUES ('a.txt','H',1,0,'t'), ('b.txt','H',1,0,'t');",
            )
            .expect("seed");
        }
        for rel in ["a.txt", "b.txt"] {
            assert!(
                db.get_local_file(rel)
                    .expect("get")
                    .unwrap()
                    .item_id
                    .is_none(),
                "the seeded rows must start without one, or this proves nothing"
            );
        }
        drop(db);

        // Re-open: `Db::new_at` runs the migration, which is where the repair happens.
        let db = Db::new_at(&path).expect("reopen");

        let a = db.get_local_file("a.txt").expect("get").unwrap().item_id;
        let b = db.get_local_file("b.txt").expect("get").unwrap().item_id;
        assert!(
            a.is_some() && b.is_some(),
            "every row must end up identified"
        );
        assert_ne!(a, b, "and no two rows may share an identity");

        // Idempotent: a third open must not re-mint what is already there.
        drop(db);
        let db = Db::new_at(&path).expect("reopen again");
        assert_eq!(
            db.get_local_file("a.txt").expect("get").unwrap().item_id,
            a,
            "an identity, once given, is not reissued on the next startup"
        );
    }

    /// W4-bis. Widening the path rewrite to `failed` rows was justified by `download`, but
    /// the clause caught every job type. `delete_local_file_internal` removes the file
    /// unconditionally, so a failed `local_delete` retargeted to the new name deletes the
    /// file the user just renamed the moment they press Retry. Naming the OLD path, it was a
    /// harmless no-op — the path no longer exists.
    #[test]
    fn a_failed_local_delete_does_not_follow_a_rename() {
        let db = Db::new_at(&unique_db_path()).expect("db init");
        db.upsert_local_file("old.txt", "H", 1, 0).expect("seed");
        {
            let conn = db.write.lock().expect("lock");
            conn.execute(
                "INSERT INTO sync_jobs (job_type,status,source_path,target_path,created_at,updated_at) \
                 VALUES ('local_delete','failed','old.txt','old.txt','t','t'), \
                        ('download','failed','old.txt','old.txt','t','t')",
                [],
            )
            .expect("seed jobs");
        }

        db.move_index_row("old.txt", "new.txt").expect("move");

        let rows: Vec<(String, Option<String>)> = {
            let conn = db.read.lock().expect("lock");
            let mut stmt = conn
                .prepare("SELECT job_type, target_path FROM sync_jobs ORDER BY job_type")
                .expect("prepare");
            let mapped = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .expect("query");
            mapped.collect::<Result<Vec<_>, _>>().expect("rows")
        };
        assert_eq!(
            rows,
            vec![
                ("download".to_string(), Some("new.txt".to_string())),
                ("local_delete".to_string(), Some("old.txt".to_string())),
            ],
            "the download follows the rename; the local_delete must be left behind"
        );
    }

    #[test]
    fn migrate_widens_a_narrower_job_type_check() {
        let path = unique_db_path();
        let mut conn = Connection::open(&path).expect("open");
        // A database from before `move` existed: it HAS a CHECK, just not one that admits
        // the new job type. This is the case the original guard missed — it asked whether
        // a CHECK was present at all, which every database now satisfies, so the widening
        // would have been skipped in silence on every real upgrade.
        conn.execute_batch(
            "CREATE TABLE sync_jobs (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 job_type TEXT NOT NULL CHECK (job_type IN ('upload','download','delete','local_delete','hydrate_cloudsc')),
                 source_path TEXT,
                 target_path TEXT,
                 status TEXT NOT NULL CHECK (status IN ('queued','running','retry_wait','done','failed')),
                 attempt_count INTEGER NOT NULL DEFAULT 0,
                 next_retry_at TEXT,
                 created_at TEXT NOT NULL,
                 updated_at TEXT NOT NULL
             );
             INSERT INTO sync_jobs (job_type, status, target_path, created_at, updated_at)
                 VALUES ('upload', 'queued', 'keep.txt', 't', 't');",
        )
        .expect("plant a narrower table");

        super::migrate(&mut conn).expect("migrate");

        // Ask the database, not the stored SQL. The production guard decides by grepping
        // that same text for the same word, so an assertion that greps it shares an oracle
        // with the code under test and cannot catch the guard being wrong.
        conn.execute(
            "INSERT INTO sync_jobs (job_type, status, source_path, target_path, created_at, updated_at) \
             VALUES ('move', 'queued', 'old.txt', 'new.txt', 't', 't')",
            [],
        )
        .expect("the widened table must accept a move job");

        // Widening must not become permissiveness: everything outside the set still fails.
        conn.execute(
            "INSERT INTO sync_jobs (job_type, status, created_at, updated_at) \
             VALUES ('bogus_type', 'queued', 't', 't')",
            [],
        )
        .expect_err("and must still reject a job type that is not in the set");

        // The rebuild copies rows rather than starting fresh — it now runs on databases
        // that hold real queued work, which it did not before this change.
        let kept: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sync_jobs WHERE target_path = 'keep.txt'",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(kept, 1, "existing queued work must survive the rebuild");
    }

    /// DBSYNC-40. The `sync_jobs` rebuild is the one migration step that only ever runs on
    /// **existing installations** — a fresh database gets the CHECK constraints from the
    /// `CREATE TABLE`, so the guard is satisfied and the DROP/RENAME never executes.
    ///
    /// That means the other migration tests, which all start from an empty file, never
    /// touch it. The branch most likely to break someone's database was the one with no
    /// coverage, which is the wrong way round — and it is now the branch running inside a
    /// transaction for the first time.
    ///
    /// (Restored: inserting a DBSYNC-99 test above this one detached this comment from the
    /// test it describes, leaving it documenting an unrelated one. Caught in review.)
    #[test]
    fn migrate_rebuilds_a_legacy_sync_jobs_table_inside_the_transaction() {
        let path = unique_db_path();
        let mut conn = Connection::open(&path).expect("open");
        // A pre-DBSYNC-31 `sync_jobs`: no CHECK constraints, and carrying rows whose values
        // are outside the sets the CHECKs will impose.
        conn.execute_batch(
            "CREATE TABLE sync_jobs (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 job_type TEXT NOT NULL,
                 status TEXT NOT NULL,
                 source_path TEXT,
                 target_path TEXT,
                 attempt_count INTEGER NOT NULL DEFAULT 0,
                 next_retry_at TEXT,
                 created_at TEXT NOT NULL,
                 updated_at TEXT NOT NULL
             );
             INSERT INTO sync_jobs (job_type, status, created_at, updated_at)
                 VALUES ('upload', 'queued', 't', 't'),
                        ('bogus_type', 'queued', 't', 't');",
        )
        .expect("plant a legacy table");

        super::migrate(&mut conn).expect("migrate must rebuild it");

        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='sync_jobs'",
                [],
                |r| r.get(0),
            )
            .expect("query");
        assert!(
            sql.contains("CHECK"),
            "the rebuild must add the constraints"
        );

        // ...and the constraint must actually BITE. The assertion above shares its oracle
        // with the production code: `sync_jobs_has_check` decides whether to rebuild by
        // grepping the same stored SQL for the same word, so a test using that heuristic
        // cannot detect the heuristic being wrong. A legacy table with `-- CHECK` in a
        // comment satisfies both and skips the rebuild entirely. This asks the database.
        let violated = conn
            .execute(
                "INSERT INTO sync_jobs (job_type, status, created_at, updated_at) \
                 VALUES ('bogus_type', 'queued', 't', 't')",
                [],
            )
            .expect_err("the rebuilt table must reject an out-of-set job_type");
        // Pin WHY it failed. A bare `is_err()` was the fourth assertion on this PR to claim
        // "must" about something it did not constrain: dropping the job_type CHECK from the
        // rebuild copy AND removing `DEFAULT 0` from attempt_count makes this insert fail on
        // NOT NULL instead, and the whole suite stayed green with the constraint gone —
        // measured. That is the realistic shape of the bug this test exists for: a rebuild
        // that silently produces a WEAKER schema than a fresh install, where every other
        // test still passes because fresh databases get the CHECK from `CREATE TABLE`.
        assert!(
            violated.to_string().contains("job_type"),
            "the INSERT must fail on the job_type CHECK, not another constraint: {violated}"
        );

        // The valid row survives; the out-of-set one is dropped rather than aborting the
        // copy, which is what the migration's own comment promises.
        let surviving: i64 = conn
            .query_row("SELECT COUNT(*) FROM sync_jobs", [], |r| r.get(0))
            .expect("count");
        assert_eq!(surviving, 1);

        // Indexes must land on the REBUILT table: `DROP TABLE` takes the old ones with it,
        // so the `CREATE INDEX` block has to run after the rebuild, not before.
        //
        // Scoped to `tbl_name` and an exact count, both deliberately. A first version
        // counted every `idx_%` in the schema with `>= 2`, and that was blind to the exact
        // bug this comment names: moving the index block before the rebuild destroys
        // `idx_sync_jobs_status_retry`, but the unscoped count still reached 2 via
        // `idx_sync_conflicts_resolved` (a different table, untouched) and
        // `idx_sync_jobs_active_unique` (created later, so it survives the reordering).
        // The whole suite stayed green with that bug present — measured, not supposed.
        let indexes: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' \
                 AND tbl_name='sync_jobs' AND name LIKE 'idx_%'",
                [],
                |r| r.get(0),
            )
            .expect("count indexes");
        assert_eq!(
            indexes, 2,
            "both sync_jobs indexes must land on the REBUILT table"
        );

        // Columns added after the rebuild's frozen column list must survive the rebuild.
        //
        // The idempotence check below already catches this — it is how the defect was found
        // — but it catches it as "the two runs disagree", which names the symptom. This names
        // the property: a legacy database that gets rebuilt must come out of ONE migrate with
        // every column the code expects, because the app starts using them immediately and
        // does not get a second migrate first.
        let rebuilt_sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='sync_jobs'",
                [],
                |r| r.get(0),
            )
            .expect("query");
        for column in ["on_success_delete_path", "on_success_delete_rev"] {
            assert!(
                rebuilt_sql.contains(column),
                "`{column}` must survive the rebuild — declare it in the additive block AFTER \
                 the rebuild, not before, or `DROP TABLE sync_jobs` takes it with the old \
                 table: {rebuilt_sql}"
            );
        }

        // And it is still idempotent over the rebuilt shape. `expect` alone would prove
        // only that the second run does not error, which is not what "no-op" means.
        let before = schema_rows(&conn);
        super::migrate(&mut conn).expect("second migrate must not fail");
        assert_eq!(
            schema_rows(&conn),
            before,
            "a second migrate over a rebuilt table must change nothing"
        );
    }

    /// DBSYNC-56. The marker is the empty string, so an accidentally-blank hash written
    /// through the ordinary path would silently mark a row for rescan instead of recording
    /// content. The guard against that is `upsert_local_file`'s `debug_assert!`, and an
    /// assert nobody exercises is not a guard — this is what makes it one.
    #[test]
    #[should_panic(expected = "use mark_local_file_for_rescan")]
    fn upsert_local_file_rejects_an_empty_hash_in_debug() {
        let db = Db::new_at(&unique_db_path()).expect("db init");
        db.upsert_local_file("a.txt", "", 1, 0).expect("upsert");
    }

    /// The deliberate route in, which must keep working and must preserve size/mtime —
    /// those are what the row still knows truthfully.
    #[test]
    fn mark_local_file_for_rescan_sets_the_marker_and_keeps_size_and_mtime() {
        let db = Db::new_at(&unique_db_path()).expect("db init");
        db.upsert_local_file("a.txt", "H2", 42, 7).expect("seed");

        db.mark_local_file_for_rescan("a.txt").expect("mark");

        let row = db.get_local_file("a.txt").expect("get").expect("row");
        assert_eq!(row.hash, Db::HASH_NEEDS_RESCAN);
        assert_eq!(row.size_bytes, 42);
        assert_eq!(row.modified_ts, 7);
    }

    /// Marking an absent row must not invent one: a cancelled upload for a path we do not
    /// track is not a reason to start tracking it.
    #[test]
    fn mark_local_file_for_rescan_is_a_noop_when_the_row_is_absent() {
        let db = Db::new_at(&unique_db_path()).expect("db init");
        db.mark_local_file_for_rescan("ghost.txt").expect("mark");
        assert!(db.get_local_file("ghost.txt").expect("get").is_none());
    }

    #[test]
    fn ignore_globs_csv_round_trip() {
        let db = Db::new_at(&unique_db_path()).expect("db init");
        assert_eq!(db.get_ignore_globs_csv().expect("get before set"), None);

        db.set_ignore_globs_csv("Thumbs.db,*.log")
            .expect("set ignore globs");
        assert_eq!(
            db.get_ignore_globs_csv().expect("get after set"),
            Some("Thumbs.db,*.log".to_string())
        );
    }
}
