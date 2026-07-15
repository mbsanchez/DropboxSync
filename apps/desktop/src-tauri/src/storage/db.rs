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
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictRow {
    pub id: i64,
    pub local_path: String,
    pub remote_path: String,
    pub reason: String,
    pub created_at: String,
}

/// Separate read/write connections plus WAL so the UI can query without blocking on sync writes.
pub struct Db {
    write: Mutex<Connection>,
    read: Mutex<Connection>,
}

impl Db {
    pub fn new() -> AppResult<Self> {
        Self::new_at(&db_path()?)
    }

    /// Open a database at an explicit path. Used by tests to stay fully isolated
    /// from the production database (which `db_path()` resolves via OS-specific
    /// app-data dirs), so running `cargo test` never touches a user's real DB.
    pub fn new_at(path: &std::path::Path) -> AppResult<Self> {
        let write = Connection::open(path)?;
        write.execute_batch(
            "
                PRAGMA foreign_keys = ON;
                PRAGMA journal_mode = WAL;
                PRAGMA synchronous = NORMAL;
                ",
        )?;
        migrate(&write)?;

        let read = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;

        Ok(Self {
            write: Mutex::new(write),
            read: Mutex::new(read),
        })
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

    /// Clears local sync state to avoid stale jobs when the sync folder changes.
    pub fn reset_sync_state(&self) -> AppResult<()> {
        let conn = self
            .write
            .lock()
            .map_err(|_| AppError::Storage("db write lock poisoned".into()))?;
        conn.execute("DELETE FROM local_file_index", [])?;
        conn.execute("DELETE FROM remote_file_index", [])?;
        conn.execute("DELETE FROM sync_jobs", [])?;
        conn.execute("DELETE FROM sync_conflicts", [])?;
        conn.execute("DELETE FROM known_folders", [])?;
        // Drop the cursor-delta cursor so remote change detection re-seeds
        // against the new folder (DBSYNC-30); other app_config keys are kept.
        conn.execute(
            "DELETE FROM app_config WHERE key = 'remote_delta_cursor'",
            [],
        )?;
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

    pub fn upsert_local_file(
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
                SELECT id, job_type, source_path, target_path, status, attempt_count, next_retry_at, updated_at, last_error
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
                SELECT id, job_type, source_path, target_path, status, attempt_count, next_retry_at, updated_at, last_error
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

    pub fn add_conflict(&self, local_path: &str, remote_path: &str, reason: &str) -> AppResult<()> {
        let conn = self
            .write
            .lock()
            .map_err(|_| AppError::Storage("db write lock poisoned".into()))?;
        conn.execute(
            "
                INSERT INTO sync_conflicts(local_path, remote_path, reason, resolved, created_at)
                VALUES(?1, ?2, ?3, 0, ?4)
                ",
            params![local_path, remote_path, reason, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn list_recent_conflicts(&self, limit: i64) -> AppResult<Vec<ConflictRow>> {
        let conn = self
            .read
            .lock()
            .map_err(|_| AppError::Storage("db read lock poisoned".into()))?;
        let mut stmt = conn.prepare(
            "
                SELECT id, local_path, remote_path, reason, created_at
                FROM sync_conflicts
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
                created_at: row.get(4)?,
            })
        })?;

        Ok(rows.collect::<Result<Vec<_>, _>>()?)
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

fn migrate(conn: &Connection) -> AppResult<()> {
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
                upload_session_file_mtime INTEGER
            );
            INSERT INTO sync_jobs_new
                SELECT id, job_type, source_path, target_path, status, attempt_count, next_retry_at,
                       created_at, updated_at, last_error, upload_session_id, upload_session_offset,
                       upload_session_file_len, upload_session_file_mtime
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
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A unique temp DB file path so tests never touch the production database.
    fn unique_db_path() -> PathBuf {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("dropbox-sync-test-{ts}"));
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
        db.enqueue_job("upload", Some("a.txt"), Some("a.txt")).unwrap();
        db.enqueue_job("upload", Some("a.txt"), Some("a.txt")).unwrap();
        assert_eq!(db.count_active_jobs().unwrap(), 1, "duplicate active upload collapsed");

        // A different job_type for the same path is a distinct active job.
        db.enqueue_job("delete", Some("a.txt"), Some("a.txt")).unwrap();
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
        db.enqueue_job("upload", Some("a.txt"), Some("a.txt")).unwrap();
        let uploads = db
            .list_recent_jobs(50)
            .unwrap()
            .into_iter()
            .filter(|j| j.job_type == "upload")
            .count();
        assert_eq!(uploads, 2, "a new active upload coexists with the completed one");
    }

    #[test]
    fn check_constraint_rejects_invalid_job_type() {
        let db = Db::new_at(&unique_db_path()).expect("db init");
        // DBSYNC-31 AC4: the CHECK constraint rejects an unknown job_type at the DB layer.
        assert!(
            db.enqueue_job("bogus_type", Some("a.txt"), Some("a.txt")).is_err(),
            "an out-of-set job_type must be rejected"
        );
        // A valid job_type still enqueues.
        db.enqueue_job("upload", Some("a.txt"), Some("a.txt")).unwrap();
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
}
