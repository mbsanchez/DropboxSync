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
    pub fn new() -> Result<Self, String> {
        Self::new_at(&db_path()?)
    }

    /// Open a database at an explicit path. Used by tests to stay fully isolated
    /// from the production database (which `db_path()` resolves via OS-specific
    /// app-data dirs), so running `cargo test` never touches a user's real DB.
    pub fn new_at(path: &std::path::Path) -> Result<Self, String> {
        let write = Connection::open(path).map_err(|e| e.to_string())?;
        write
            .execute_batch(
                "
                PRAGMA foreign_keys = ON;
                PRAGMA journal_mode = WAL;
                PRAGMA synchronous = NORMAL;
                ",
            )
            .map_err(|e| e.to_string())?;
        migrate(&write)?;

        let read = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| e.to_string())?;

        Ok(Self {
            write: Mutex::new(write),
            read: Mutex::new(read),
        })
    }

    pub fn set_sync_folder(&self, folder: &str) -> Result<(), String> {
        let now = Utc::now().to_rfc3339();
        let conn = self.write.lock().map_err(|_| "db write lock poisoned".to_string())?;
        conn
            .execute(
                "
                INSERT INTO app_config (key, value, updated_at)
                VALUES ('sync_folder', ?1, ?2)
                ON CONFLICT(key) DO UPDATE SET
                  value=excluded.value,
                  updated_at=excluded.updated_at
                ",
                params![folder, now],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Clears local sync state to avoid stale jobs when the sync folder changes.
    pub fn reset_sync_state(&self) -> Result<(), String> {
        let conn = self.write.lock().map_err(|_| "db write lock poisoned".to_string())?;
        conn.execute("DELETE FROM local_file_index", []).map_err(|e| e.to_string())?;
        conn.execute("DELETE FROM remote_file_index", []).map_err(|e| e.to_string())?;
        conn.execute("DELETE FROM sync_jobs", []).map_err(|e| e.to_string())?;
        conn.execute("DELETE FROM sync_conflicts", []).map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn get_sync_folder(&self) -> Result<Option<String>, String> {
        let conn = self.read.lock().map_err(|_| "db read lock poisoned".to_string())?;
        let mut stmt = conn
            .prepare("SELECT value FROM app_config WHERE key = 'sync_folder' LIMIT 1")
            .map_err(|e| e.to_string())?;
        let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
        if let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let value: String = row.get(0).map_err(|e| e.to_string())?;
            return Ok(Some(value));
        }
        Ok(None)
    }

    pub fn set_app_config(&self, key: &str, value: &str) -> Result<(), String> {
        let now = Utc::now().to_rfc3339();
        let conn = self.write.lock().map_err(|_| "db write lock poisoned".to_string())?;
        conn.execute(
            "
            INSERT INTO app_config (key, value, updated_at)
            VALUES(?1, ?2, ?3)
            ON CONFLICT(key) DO UPDATE SET
              value=excluded.value,
              updated_at=excluded.updated_at
            ",
            params![key, value, now],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn get_app_config(&self, key: &str) -> Result<Option<String>, String> {
        let conn = self.read.lock().map_err(|_| "db read lock poisoned".to_string())?;
        let mut stmt = conn
            .prepare("SELECT value FROM app_config WHERE key = ?1 LIMIT 1")
            .map_err(|e| e.to_string())?;
        let mut rows = stmt.query(params![key]).map_err(|e| e.to_string())?;
        if let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let value: String = row.get(0).map_err(|e| e.to_string())?;
            return Ok(Some(value));
        }
        Ok(None)
    }

    // Selective sync (prefix-based). CSV of prefixes without leading '/' (e.g. "Fotos,Videos/2024").
    pub fn set_include_prefixes_csv(&self, csv: &str) -> Result<(), String> {
        self.set_app_config("include_prefixes_csv", csv)
    }

    pub fn get_include_prefixes_csv(&self) -> Result<Option<String>, String> {
        self.get_app_config("include_prefixes_csv")
    }

    pub fn set_exclude_prefixes_csv(&self, csv: &str) -> Result<(), String> {
        self.set_app_config("exclude_prefixes_csv", csv)
    }

    pub fn get_exclude_prefixes_csv(&self) -> Result<Option<String>, String> {
        self.get_app_config("exclude_prefixes_csv")
    }

    pub fn list_local_files(&self) -> Result<Vec<FileIndexRow>, String> {
        let conn = self.read.lock().map_err(|_| "db read lock poisoned".to_string())?;
        let mut stmt = conn
            .prepare(
                "SELECT relative_path, hash, size_bytes, modified_ts FROM local_file_index ORDER BY relative_path",
            )
            .map_err(|e| e.to_string())?;

        let rows = stmt
            .query_map([], |row| {
                Ok(FileIndexRow {
                    relative_path: row.get(0)?,
                    hash: row.get(1)?,
                    size_bytes: row.get(2)?,
                    modified_ts: row.get(3)?,
                })
            })
            .map_err(|e| e.to_string())?;

        rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
    }

    pub fn get_local_file(&self, relative_path: &str) -> Result<Option<FileIndexRow>, String> {
        let conn = self.read.lock().map_err(|_| "db read lock poisoned".to_string())?;
        let mut stmt = conn
            .prepare(
                "SELECT relative_path, hash, size_bytes, modified_ts FROM local_file_index WHERE relative_path = ?1 LIMIT 1",
            )
            .map_err(|e| e.to_string())?;
        let mut rows = stmt.query(params![relative_path]).map_err(|e| e.to_string())?;
        if let Some(row) = rows.next().map_err(|e| e.to_string())? {
            return Ok(Some(FileIndexRow {
                relative_path: row.get(0).map_err(|e| e.to_string())?,
                hash: row.get(1).map_err(|e| e.to_string())?,
                size_bytes: row.get(2).map_err(|e| e.to_string())?,
                modified_ts: row.get(3).map_err(|e| e.to_string())?,
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
    ) -> Result<(), String> {
        let now = Utc::now().to_rfc3339();
        let conn = self.write.lock().map_err(|_| "db write lock poisoned".to_string())?;
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
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn remove_local_file(&self, relative_path: &str) -> Result<(), String> {
        let conn = self.write.lock().map_err(|_| "db write lock poisoned".to_string())?;
        conn
            .execute(
                "DELETE FROM local_file_index WHERE relative_path = ?1",
                params![relative_path],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn get_remote_file(&self, relative_path: &str) -> Result<Option<RemoteFileIndexRow>, String> {
        let conn = self.read.lock().map_err(|_| "db read lock poisoned".to_string())?;
        let mut stmt = conn
            .prepare(
                "
                SELECT relative_path, content_hash, rev, modified_ts
                FROM remote_file_index
                WHERE relative_path = ?1
                LIMIT 1
                ",
            )
            .map_err(|e| e.to_string())?;
        let mut rows = stmt.query(params![relative_path]).map_err(|e| e.to_string())?;
        if let Some(row) = rows.next().map_err(|e| e.to_string())? {
            return Ok(Some(RemoteFileIndexRow {
                relative_path: row.get(0).map_err(|e| e.to_string())?,
                content_hash: row.get(1).map_err(|e| e.to_string())?,
                rev: row.get(2).map_err(|e| e.to_string())?,
                modified_ts: row.get(3).map_err(|e| e.to_string())?,
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
    ) -> Result<(), String> {
        let now = Utc::now().to_rfc3339();
        let conn = self.write.lock().map_err(|_| "db write lock poisoned".to_string())?;
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
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn enqueue_job(
        &self,
        job_type: &str,
        source_path: Option<&str>,
        target_path: Option<&str>,
    ) -> Result<(), String> {
        let now = Utc::now().to_rfc3339();
        let conn = self.write.lock().map_err(|_| "db write lock poisoned".to_string())?;
        conn
            .execute(
                "
                INSERT INTO sync_jobs(job_type, source_path, target_path, status, attempt_count, next_retry_at, created_at, updated_at)
                VALUES(?1, ?2, ?3, 'queued', 0, NULL, ?4, ?4)
                ",
                params![job_type, source_path, target_path, now],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn count_active_jobs(&self) -> Result<usize, String> {
        let conn = self.read.lock().map_err(|_| "db read lock poisoned".to_string())?;
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sync_jobs WHERE status IN ('queued', 'retry_wait', 'running')",
                [],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        Ok(count as usize)
    }

    pub fn list_recent_jobs(&self, limit: i64) -> Result<Vec<SyncJobRow>, String> {
        let conn = self.read.lock().map_err(|_| "db read lock poisoned".to_string())?;
        let mut stmt = conn
            .prepare(
                "
                SELECT id, job_type, source_path, target_path, status, attempt_count, next_retry_at, updated_at, last_error
                FROM sync_jobs
                ORDER BY id DESC
                LIMIT ?1
                ",
            )
            .map_err(|e| e.to_string())?;

        let rows = stmt
            .query_map(params![limit], |row| {
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
            })
            .map_err(|e| e.to_string())?;

        rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
    }

    pub fn pick_next_due_job(&self) -> Result<Option<SyncJobRow>, String> {
        let now = Utc::now().to_rfc3339();
        let conn = self.write.lock().map_err(|_| "db write lock poisoned".to_string())?;
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
                .map_err(|e| e.to_string())?;

            let mut rows = stmt.query(params![now]).map_err(|e| e.to_string())?;
            if let Some(row) = rows.next().map_err(|e| e.to_string())? {
                Some(SyncJobRow {
                    id: row.get(0).map_err(|e| e.to_string())?,
                    job_type: row.get(1).map_err(|e| e.to_string())?,
                    source_path: row.get(2).map_err(|e| e.to_string())?,
                    target_path: row.get(3).map_err(|e| e.to_string())?,
                    status: row.get(4).map_err(|e| e.to_string())?,
                    attempt_count: row.get(5).map_err(|e| e.to_string())?,
                    next_retry_at: row.get(6).map_err(|e| e.to_string())?,
                    updated_at: row.get(7).map_err(|e| e.to_string())?,
                    last_error: row.get(8).map_err(|e| e.to_string())?,
                })
            } else {
                None
            }
        };

        if let Some(ref job) = job_opt {
            conn.execute(
                "UPDATE sync_jobs SET status='running', updated_at=?2 WHERE id=?1",
                params![job.id, Utc::now().to_rfc3339()],
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(job_opt)
    }

    pub fn mark_job_completed(&self, id: i64) -> Result<(), String> {
        let conn = self.write.lock().map_err(|_| "db write lock poisoned".to_string())?;
        conn
            .execute(
                "UPDATE sync_jobs SET status='done', last_error=NULL, updated_at=?2 WHERE id=?1",
                params![id, Utc::now().to_rfc3339()],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn mark_job_retry_wait(
        &self,
        id: i64,
        attempt_count: i64,
        next_retry_at: &str,
        last_error: Option<&str>,
    ) -> Result<(), String> {
        let conn = self.write.lock().map_err(|_| "db write lock poisoned".to_string())?;
        conn
            .execute(
                "
                UPDATE sync_jobs
                SET status='retry_wait', attempt_count=?2, next_retry_at=?3, last_error=?4, updated_at=?5
                WHERE id=?1
                ",
                params![id, attempt_count, next_retry_at, last_error, Utc::now().to_rfc3339()],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn mark_job_failed(
        &self,
        id: i64,
        attempt_count: i64,
        last_error: Option<&str>,
    ) -> Result<(), String> {
        let conn = self.write.lock().map_err(|_| "db write lock poisoned".to_string())?;
        conn
            .execute(
                "
                UPDATE sync_jobs
                SET status='failed', attempt_count=?2, last_error=?3, updated_at=?4
                WHERE id=?1
                ",
                params![id, attempt_count, last_error, Utc::now().to_rfc3339()],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// The most recent failed job's error message, or `None` if no jobs are failed.
    /// Drives the dashboard's global error/health so a later unrelated success
    /// doesn't mask that failures are still present.
    pub fn latest_failed_error(&self) -> Result<Option<String>, String> {
        let conn = self.read.lock().map_err(|_| "db read lock poisoned".to_string())?;
        let mut stmt = conn
            .prepare(
                "
                SELECT last_error FROM sync_jobs
                WHERE status='failed'
                ORDER BY id DESC
                LIMIT 1
                ",
            )
            .map_err(|e| e.to_string())?;
        let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
        if let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let msg: Option<String> = row.get(0).map_err(|e| e.to_string())?;
            return Ok(Some(msg.unwrap_or_else(|| "job failed".to_string())));
        }
        Ok(None)
    }

    /// Resets all `failed` jobs back to `queued` so they are retried. Returns the count.
    pub fn requeue_failed_jobs(&self) -> Result<usize, String> {
        let conn = self.write.lock().map_err(|_| "db write lock poisoned".to_string())?;
        let n = conn
            .execute(
                "
                UPDATE sync_jobs
                SET status='queued', attempt_count=0, next_retry_at=NULL, last_error=NULL, updated_at=?1
                WHERE status='failed'
                ",
                params![Utc::now().to_rfc3339()],
            )
            .map_err(|e| e.to_string())?;
        Ok(n)
    }

    pub fn add_conflict(&self, local_path: &str, remote_path: &str, reason: &str) -> Result<(), String> {
        let conn = self.write.lock().map_err(|_| "db write lock poisoned".to_string())?;
        conn
            .execute(
                "
                INSERT INTO sync_conflicts(local_path, remote_path, reason, resolved, created_at)
                VALUES(?1, ?2, ?3, 0, ?4)
                ",
                params![local_path, remote_path, reason, Utc::now().to_rfc3339()],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn list_recent_conflicts(&self, limit: i64) -> Result<Vec<ConflictRow>, String> {
        let conn = self.read.lock().map_err(|_| "db read lock poisoned".to_string())?;
        let mut stmt = conn
            .prepare(
                "
                SELECT id, local_path, remote_path, reason, created_at
                FROM sync_conflicts
                ORDER BY id DESC
                LIMIT ?1
                ",
            )
            .map_err(|e| e.to_string())?;

        let rows = stmt
            .query_map(params![limit], |row| {
                Ok(ConflictRow {
                    id: row.get(0)?,
                    local_path: row.get(1)?,
                    remote_path: row.get(2)?,
                    reason: row.get(3)?,
                    created_at: row.get(4)?,
                })
            })
            .map_err(|e| e.to_string())?;

        rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
    }

    pub fn list_unresolved_conflict_local_paths(&self) -> Result<Vec<String>, String> {
        let conn = self.read.lock().map_err(|_| "db read lock poisoned".to_string())?;
        let mut stmt = conn
            .prepare(
                "
                SELECT DISTINCT local_path
                FROM sync_conflicts
                WHERE resolved = 0
                ORDER BY local_path
                ",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
    }
}

fn migrate(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS app_config (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS sync_jobs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            job_type TEXT NOT NULL,
            source_path TEXT,
            target_path TEXT,
            status TEXT NOT NULL,
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
        ",
    )
    .map_err(|e| e.to_string())?;

    // Additive migrations for databases created before a column existed.
    add_column_if_missing(conn, "sync_jobs", "last_error", "TEXT")?;
    Ok(())
}

/// Adds `column` to `table` if it isn't already present. Idempotent so it can run
/// on every startup without failing on databases that already have the column.
fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    decl: &str,
) -> Result<(), String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|e| e.to_string())?;
    let mut exists = false;
    let names = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| e.to_string())?;
    for name in names {
        if name.map_err(|e| e.to_string())? == column {
            exists = true;
            break;
        }
    }
    if !exists {
        conn.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"),
            [],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Shared app data directory (SQLite DB, overlay_state.json for shell extensions).
pub fn app_data_dir() -> Result<PathBuf, String> {
    #[cfg(target_os = "windows")]
    {
        let base =
            std::env::var("LOCALAPPDATA").map_err(|_| "LOCALAPPDATA env var not found".to_string())?;
        let mut path = PathBuf::from(base);
        path.push("DropboxSyncDesktop");
        std::fs::create_dir_all(&path).map_err(|e| e.to_string())?;
        return Ok(path);
    }
    #[cfg(not(target_os = "windows"))]
    {
        let home = std::env::var("HOME").map_err(|_| "HOME env var not found".to_string())?;
        let mut path = PathBuf::from(home);
        // Requested location for local persistent index/queue database.
        path.push("Library");
        path.push("Applications");
        path.push("DropboxSyncDesktop");
        std::fs::create_dir_all(&path).map_err(|e| e.to_string())?;
        Ok(path)
    }
}

fn db_path() -> Result<PathBuf, String> {
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
}
