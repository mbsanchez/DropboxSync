use crate::error::{AppError, AppResult};
use chrono::Utc;
use rusqlite::{params, Connection, OpenFlags};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileIndexRow {
    pub relative_path: String,
    pub hash: String,
    pub size_bytes: i64,
    pub modified_ts: i64,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteFileIndexRow {
    pub relative_path: String,
    pub content_hash: String,
    pub rev: String,
    pub modified_ts: i64,
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
    pub fn upsert_known_folder(&self, relative_path: &str) -> AppResult<()> {
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

    pub fn remove_known_folder(&self, relative_path: &str) -> AppResult<()> {
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
                "SELECT relative_path, hash, size_bytes, modified_ts FROM local_file_index ORDER BY relative_path",
            )
            ?;

        let rows = stmt.query_map([], |row| {
            Ok(FileIndexRow {
                relative_path: row.get(0)?,
                hash: row.get(1)?,
                size_bytes: row.get(2)?,
                modified_ts: row.get(3)?,
            })
        })?;

        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn get_local_file(&self, relative_path: &str) -> AppResult<Option<FileIndexRow>> {
        // Canonicalize path separators to '/' so local (OS-native '\' on Windows)
        // and remote (Dropbox '/') keys match — DBSYNC-45.
        let relative_path = relative_path.replace('\\', "/");
        let conn = self
            .read
            .lock()
            .map_err(|_| AppError::Storage("db read lock poisoned".into()))?;
        let mut stmt = conn
            .prepare(
                "SELECT relative_path, hash, size_bytes, modified_ts FROM local_file_index WHERE relative_path = ?1 LIMIT 1",
            )
            ?;
        let mut rows = stmt.query(params![relative_path])?;
        if let Some(row) = rows.next()? {
            return Ok(Some(FileIndexRow {
                relative_path: row.get(0)?,
                hash: row.get(1)?,
                size_bytes: row.get(2)?,
                modified_ts: row.get(3)?,
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
        let relative_path = relative_path.replace('\\', "/");
        let now = Utc::now().to_rfc3339();
        let conn = self
            .write
            .lock()
            .map_err(|_| AppError::Storage("db write lock poisoned".into()))?;
        conn
            .execute(
                "
                INSERT INTO local_file_index(relative_path, hash, size_bytes, modified_ts, updated_at)
                VALUES(?1, ?2, ?3, ?4, ?5)
                ON CONFLICT(relative_path) DO UPDATE SET
                  hash=excluded.hash,
                  size_bytes=excluded.size_bytes,
                  modified_ts=excluded.modified_ts,
                  updated_at=excluded.updated_at
                ",
                params![relative_path, hash, size_bytes, modified_ts, now],
            )
            ?;
        Ok(())
    }

    pub fn remove_local_file(&self, relative_path: &str) -> AppResult<()> {
        let relative_path = relative_path.replace('\\', "/");
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
        let relative_path = relative_path.replace('\\', "/");
        let conn = self
            .read
            .lock()
            .map_err(|_| AppError::Storage("db read lock poisoned".into()))?;
        let mut stmt = conn.prepare(
            "
                SELECT relative_path, content_hash, rev, modified_ts
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
            }));
        }
        Ok(None)
    }

    pub fn upsert_remote_file(
        &self,
        relative_path: &str,
        content_hash: &str,
        rev: &str,
        modified_ts: i64,
    ) -> AppResult<()> {
        let relative_path = relative_path.replace('\\', "/");
        let now = Utc::now().to_rfc3339();
        let conn = self
            .write
            .lock()
            .map_err(|_| AppError::Storage("db write lock poisoned".into()))?;
        conn.execute(
            "
            INSERT INTO remote_file_index(relative_path, content_hash, rev, modified_ts, updated_at)
            VALUES(?1, ?2, ?3, ?4, ?5)
            ON CONFLICT(relative_path) DO UPDATE SET
              content_hash=excluded.content_hash,
              rev=excluded.rev,
              modified_ts=excluded.modified_ts,
              updated_at=excluded.updated_at
            ",
            params![relative_path, content_hash, rev, modified_ts, now],
        )?;
        Ok(())
    }

    pub fn remove_remote_file(&self, relative_path: &str) -> AppResult<()> {
        let relative_path = relative_path.replace('\\', "/");
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
    /// the "delete a folder twice" behavior. Boundary-safe via `LIKE ... ESCAPE`
    /// so a sibling like `prefix-other` is never matched and `%`/`_`/accents in
    /// the path are treated literally. For a plain file `prefix` this is
    /// equivalent to `remove_remote_file` (no `prefix/...` descendants exist).
    pub fn remove_remote_subtree(&self, prefix: &str) -> AppResult<()> {
        let prefix = prefix.replace('\\', "/");
        let escaped = prefix
            .replace('!', "!!")
            .replace('%', "!%")
            .replace('_', "!_");
        let child_pattern = format!("{escaped}/%");
        let conn = self
            .write
            .lock()
            .map_err(|_| AppError::Storage("db write lock poisoned".into()))?;
        conn.execute(
            "DELETE FROM remote_file_index WHERE relative_path = ?1 OR relative_path LIKE ?2 ESCAPE '!'",
            params![prefix, child_pattern],
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

    /// Distinct `target_path` + `source_path` of every ACTIVE job (`queued`,
    /// `retry_wait`, `running`). DBSYNC-31: replaces the `list_recent_jobs(N)`-based
    /// dedup, which silently missed jobs once the table exceeded N rows. Backed by the
    /// `idx_sync_jobs_status_retry` index. Used to avoid enqueuing duplicate work and to
    /// route a change that races a still-pending job to a conflicted copy.
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
            job_type TEXT NOT NULL CHECK (job_type IN ('upload','download','delete','local_delete','hydrate_cloudsc')),
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
        ",
    )?;

    // Additive migrations for databases created before a column existed.
    add_column_if_missing(conn, "sync_jobs", "last_error", "TEXT")?;
    add_column_if_missing(conn, "sync_jobs", "upload_session_id", "TEXT")?;
    add_column_if_missing(conn, "sync_jobs", "upload_session_offset", "INTEGER")?;
    add_column_if_missing(conn, "sync_jobs", "upload_session_file_len", "INTEGER")?;
    add_column_if_missing(conn, "sync_jobs", "upload_session_file_mtime", "INTEGER")?;
    add_column_if_missing(conn, "sync_jobs", "delete_parent_rev", "TEXT")?;

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
    // CHECKs from the CREATE TABLE above; a pre-existing table (whose stored SQL has no
    // CHECK) is rebuilt here. Guarded + idempotent. Any row with an out-of-set value
    // (shouldn't exist — both columns are code-controlled) is dropped rather than
    // aborting the copy. Runs BEFORE the index creation below so the indexes land on the
    // rebuilt table. Job rows are transient (re-derived by the scan), so this is safe.
    let sync_jobs_has_check = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='sync_jobs'",
            [],
            |r| r.get::<_, String>(0),
        )
        .map(|sql| sql.contains("CHECK"))
        .unwrap_or(true);
    if !sync_jobs_has_check {
        conn.execute_batch(
            "
            CREATE TABLE sync_jobs_new (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                job_type TEXT NOT NULL CHECK (job_type IN ('upload','download','delete','local_delete','hydrate_cloudsc')),
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
                  AND job_type IN ('upload','download','delete','local_delete','hydrate_cloudsc');
            DROP TABLE sync_jobs;
            ALTER TABLE sync_jobs_new RENAME TO sync_jobs;
            ",
        )?;
    }

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
        db.upsert_remote_file("UNET", "h", "r", 0).expect("u1");
        db.upsert_remote_file("UNET/Ascensos/artículos/a b.pdf", "h", "r", 0)
            .expect("u2");
        db.upsert_remote_file("UNET-other/keep.txt", "h", "r", 0)
            .expect("u3");
        db.upsert_remote_file("Otra/keep.txt", "h", "r", 0)
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
        db.upsert_remote_file("a.txt", "H", "rev", 0)
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
                "INSERT INTO local_file_index VALUES ('a.txt','H',1,0,'t');
                 INSERT INTO local_file_index VALUES ('marked.txt','',1,0,'t');
                 INSERT INTO remote_file_index VALUES ('a.txt','H','rev',0,'t');
                 INSERT INTO sync_jobs (job_type,status,created_at,updated_at)
                     VALUES ('upload','queued','t','t'),
                            ('upload','done','t','t'),
                            ('upload','failed','t','t');
                 INSERT INTO sync_conflicts (local_path,remote_path,reason,resolved,created_at)
                     VALUES ('a.txt','a.txt','unresolved',0,'t'),
                            ('b.txt','b.txt','resolved',1,'t');
                 INSERT INTO known_folders VALUES ('sub','t');",
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

    /// DBSYNC-40. The `sync_jobs` rebuild is the one migration step that only ever runs on
    /// **existing installations** — a fresh database gets the CHECK constraints from the
    /// `CREATE TABLE`, so `sync_jobs_has_check` is true and the DROP/RENAME never executes.
    ///
    /// That means the other migration tests, which all start from an empty file, never
    /// touch it. The branch most likely to break someone's database was the one with no
    /// coverage, which is the wrong way round — and it is now the branch running inside a
    /// transaction for the first time.
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
