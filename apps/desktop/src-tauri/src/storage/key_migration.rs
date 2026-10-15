//! Startup migration: re-key every stored path to NFC on macOS (DBSYNC-103, #186).
//!
//! ## Why this exists, and why it runs inside `Db::new_at`
//!
//! Builds before DBSYNC-103 keyed the index with whatever bytes the filesystem produced
//! (NFD for names typed or created by some tools), while Dropbox returns and folds NFC.
//! The new code keys everything NFC. Legacy NFD rows coexisting with the new NFC code is
//! the dangerous window: a comparison that canonicalises one side but not the other turns
//! "the same folder" into "a folder that vanished", and Dropbox folds NFC and NFD into one
//! name, so a `delete_v2` on the legacy spelling deletes the real folder (reproduced by an
//! independent review of the core slices).
//!
//! The window is closed rather than patched comparison by comparison: this migration runs
//! from [`crate::storage::db::Db::new_at`], i.e. **before a `Db` value exists**. Nothing
//! can read or write a key before it has run, because there is no handle to do it with.
//! `Db::new` is called once in production (`lib.rs::run`), before the Tauri builder, the
//! scheduler, the watcher, the longpoll and the overlay exist.
//!
//! ## Guarantees
//!
//! * **macOS only.** Elsewhere [`is_canonical_form`] is always true, so nothing is ever
//!   selected and the function is a no-op (and it returns early anyway).
//! * **Atomic.** One transaction. A failure at any step rolls everything back.
//! * **Backed up.** When some row will change, a fresh `VACUUM INTO` copy is written next to
//!   the database before the transaction, **every time**; it is never reused. (An earlier
//!   design reused a backup while `startup-error.txt` named it. That broke when an older
//!   build, which never clears the file, wrote new rows in between: the new collision losers
//!   were in no backup.) Disk use stays bounded instead by deleting the one backup the
//!   previous failed attempt left: once the fresh copy is written, the file named by
//!   `startup-error.txt` is removed if its name equals one of this database's
//!   `<db>.pre-nfc-rekey-*.bak` files exactly (no substring match, nothing else is ever
//!   touched). A persistently failing migration therefore keeps exactly one backup, and a
//!   success leaves one, unless a launch in between fails before writing its own backup
//!   (for example `VACUUM INTO` on a full disk): its error replaces the marker, so the
//!   earlier backup is no longer named and stays on disk. That costs disk, never data.
//!   A second run finds nothing to do and writes none.
//! * **Serialised with other launches.** The transaction is `IMMEDIATE`: it takes the write
//!   lock up front and so waits for it under `busy_timeout`. A plain (deferred) transaction
//!   reads first and fails at once with SQLITE_BUSY when it later tries to upgrade under WAL,
//!   skipping the timeout. The non-canonical count is repeated inside the lock: if another
//!   instance migrated first there is nothing to do, nothing changes, and the backup this
//!   call just wrote is deleted again so no extra copy is left behind. The backup comes
//!   *before* the lock because `VACUUM INTO` cannot run inside a transaction; taking the
//!   lock first and then leaving it to back up would reopen the same race.
//! * **Never silently lossy.** A collision (two rows for one canonical key) is resolved by
//!   a fixed per-table rule and every dropped or demoted row is logged at `warn` with both
//!   rows' fields. See each [`IndexSpec`] and [`rekey_sync_jobs`].
//! * **Verified.** Before commit every path column is re-scanned; a surviving non-canonical
//!   value is an error and rolls the transaction back.
//! * **Fail closed.** The caller propagates the error out of `Db::new_at`; `lib.rs::run`
//!   `expect`s it, exactly like any other database initialisation failure, and the process
//!   dies before any sync loop exists. `Db::new_at` first writes the reason, synchronously,
//!   to `startup-error.txt` next to the database (the log goes through a non-blocking
//!   appender that a panic can outrun), and removes that file after a successful start.
//!
//! ## Collision rules
//!
//! `Db::new_at` runs the schema `migrate()` first, and that backfills `local_file_index.item_id`
//! on every row, so by the time this module runs every local row has an identity and "has an
//! `item_id`" can never separate two rows. The rules below are the ones that can fire.
//!
//! | table | winner | loser |
//! |---|---|---|
//! | `local_file_index` | newest `updated_at`, then newest `modified_ts` (hash, size and `item_id` all come from that row) | deleted, logged |
//! | `remote_file_index` | newest `updated_at`, then newest `modified_ts`; `rev` and `content_hash` always come from that row. The winner inherits `dropbox_id` from the loser only if it has none | deleted, logged |
//! | `known_folders` | newest `updated_at`; winner inherits `dropbox_id` if it lacks one and `owns_content = max` | deleted, logged |
//! | `sync_jobs` | among all non-`done` jobs of one `(job_type, canonical target)` (when one was re-keyed): the newest active, else the newest failed (`updated_at`, then `id`); `delta_deferred = max`; an upload winner without a source-deletion debt inherits the loser's | **deleted**, logged at `warn` with the full row, unless it owes a different source deletion (then kept as `done`), see [`rekey_sync_jobs`] |
//! | `sync_conflicts` | no uniqueness; every row is re-keyed in place | none |
//! | `refused_moves` | newest `refused_at` | deleted, logged |
//!
//! Every tie ends the same way: the row that was already canonical wins, then the smaller
//! key bytes. The outcome never depends on iteration order.
//!
//! A stale `rev` is worse than a missing `dropbox_id`: a stored `rev` that no longer matches
//! Dropbox turns a later local delete into a `RevConflict`. That is why the remote index
//! never keeps an older row just because it carries an id.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, Transaction, TransactionBehavior};

use crate::error::{AppError, AppResult};
use crate::path_util::{is_canonical_form, to_canonical_form};

/// Every column that holds a path or an index key, per table. Checked against the live
/// schema by a test (`every_text_column_is_classified`): a new TEXT column must be added
/// here or to the test's non-path list, so a new path column cannot be forgotten.
///
/// `app_config` is deliberately absent: `sync_folder` is an absolute OS path, not a key,
/// and `include_prefixes_csv` / `exclude_prefixes_csv` / `ignore_globs_csv` hold the
/// user's typed text. Those are left byte-for-byte as stored: DBSYNC-103 #188
/// canonicalises them when they are *parsed* (`path_util::parse_prefix_csv` and
/// `parse_ignore_globs_csv`), so there is nothing to re-key here.
const PATH_COLUMNS: &[(&str, &[&str])] = &[
    ("local_file_index", &["relative_path"]),
    ("remote_file_index", &["relative_path"]),
    ("known_folders", &["relative_path"]),
    (
        "sync_jobs",
        &["source_path", "target_path", "on_success_delete_path"],
    ),
    (
        "sync_conflicts",
        &["local_path", "remote_path", "conflicted_copy_path"],
    ),
    ("refused_moves", &["from_path", "to_path"]),
];

/// What a run changed. All zero means the database was already canonical.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct RekeyReport {
    /// Rows whose key (or any path column) was rewritten. In the index tables a survivor
    /// that kept its key but absorbed a merged loser is also counted; in `sync_jobs` a
    /// survivor whose only change is an inherited upload debt is not.
    pub rows_rekeyed: usize,
    /// Index/refusal rows deleted because another row held the same canonical key.
    pub collisions_dropped: usize,
    /// `sync_jobs` rows deleted (each logged in full at `warn`): every loser of a group of
    /// non-`done` jobs that fold onto one canonical `(job_type, target)`, whether the loser
    /// was active or `failed`. Losers kept as `done` (different upload debt) are not counted.
    pub jobs_dropped: usize,
    /// `local_delete` jobs newly marked `delta_deferred` so they re-verify against Dropbox
    /// before unlinking anything: re-keyed non-`done` ones, and group winners promoted
    /// because the group held a re-keyed member. Each job is counted once.
    pub local_deletes_deferred: usize,
    /// The `VACUUM INTO` copy written for this run.
    pub backup: Option<PathBuf>,
}

/// Entry point used by `Db::new_at`. See the module docs.
pub(crate) fn rekey_to_canonical_form(
    conn: &mut Connection,
    db_path: &Path,
) -> AppResult<RekeyReport> {
    rekey_with(conn, db_path, &mut |_, _| Ok(()))
}

/// [`rekey_to_canonical_form`] with a hook called after each table's step, inside the
/// transaction. Production passes a no-op; the fail-closed tests inject an error (to prove
/// earlier tables roll back) or a stray NFD value (to prove the final verification fires).
fn rekey_with(
    conn: &mut Connection,
    db_path: &Path,
    after_step: &mut dyn FnMut(&str, &Transaction<'_>) -> AppResult<()>,
) -> AppResult<RekeyReport> {
    if !cfg!(target_os = "macos") {
        return Ok(RekeyReport::default());
    }
    let legacy = count_non_canonical(conn)?;
    if legacy == 0 {
        return Ok(RekeyReport::default());
    }
    tracing::warn!(
        legacy_values = legacy,
        "found path values that are not NFC; re-keying them (DBSYNC-103)"
    );

    // `VACUUM INTO` cannot run inside a transaction, so the backup comes first. If it
    // fails nothing has changed and the error stops startup. It is always a fresh copy; the
    // one a failed earlier attempt left is deleted once this one exists (module docs).
    let backup = backup_path(db_path);
    write_backup(
        db_path,
        &backup,
        |c, to| c.execute("VACUUM INTO ?1", params![to]).map(|_| ()),
        conn,
    )?;
    tracing::info!(backup = %backup.display(), "wrote pre-migration backup");

    // Whatever fails from here on carries the backup's name, so `startup-error.txt` can tell
    // the next launch which backup the failed attempt left (see `remove_superseded_backup`).
    let backup_name = backup
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    rekey_in_transaction(conn, backup, after_step)
        .map_err(|e| AppError::Storage(format!("{e} ({BACKUP_MARKER} {backup_name})")))
}

/// Writes `backup` with `vacuum`, makes it durable, and only then deletes the backup of a
/// failed earlier attempt ([`remove_superseded_backup`]), so there is never a moment with no
/// durable copy. A partial file left by a failed write is removed (best effort), but only if
/// this call created it: a file that was already there is not ours to delete.
fn write_backup(
    db_path: &Path,
    backup: &Path,
    vacuum: impl FnOnce(&Connection, &str) -> rusqlite::Result<()>,
    conn: &Connection,
) -> AppResult<()> {
    let existed = backup.exists();
    let discard = |why: &str| {
        if existed {
            return;
        }
        match std::fs::remove_file(backup) {
            Ok(()) => tracing::warn!(backup = %backup.display(), why, "removed a partial backup"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(error = %e, backup = %backup.display(), why, "could not remove a partial backup");
            }
        }
    };
    let to = backup
        .to_str()
        .ok_or_else(|| AppError::Storage(format!("backup path {backup:?} is not valid UTF-8")))?;
    if let Err(e) = vacuum(conn, to) {
        discard("VACUUM INTO failed");
        return Err(AppError::Storage(format!(
            "could not write pre-migration backup: {e}"
        )));
    }
    if let Err(e) = sync_backup(backup) {
        discard("fsync failed");
        return Err(AppError::Storage(format!(
            "could not make the pre-migration backup durable: {e}"
        )));
    }
    remove_superseded_backup(db_path, backup);
    Ok(())
}

/// fsync the backup file, then its directory (so the new directory entry is durable too).
/// The directory sync is best effort; a failure of the file sync is an error.
fn sync_backup(backup: &Path) -> std::io::Result<()> {
    std::fs::File::open(backup)?.sync_all()?;
    if let Some(dir) = backup.parent().filter(|p| !p.as_os_str().is_empty()) {
        if let Err(e) = std::fs::File::open(dir).and_then(|d| d.sync_all()) {
            tracing::warn!(error = %e, "could not fsync the backup directory");
        }
    }
    Ok(())
}

/// Precedes the backup's file name in a failing migration's error text.
const BACKUP_MARKER: &str = "pre-migration backup:";

fn rekey_in_transaction(
    conn: &mut Connection,
    backup: PathBuf,
    after_step: &mut dyn FnMut(&str, &Transaction<'_>) -> AppResult<()>,
) -> AppResult<RekeyReport> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    // Another instance may have migrated while this one waited for the lock.
    if count_non_canonical(&tx)? == 0 {
        drop(tx);
        match std::fs::remove_file(&backup) {
            Ok(()) => tracing::info!(
                backup = %backup.display(),
                "another instance already migrated; removed the backup this call wrote"
            ),
            Err(e) => {
                tracing::warn!(error = %e, backup = %backup.display(), "could not remove an unneeded backup")
            }
        }
        return Ok(RekeyReport::default());
    }
    let mut report = RekeyReport {
        backup: Some(backup),
        ..RekeyReport::default()
    };
    for spec in INDEX_SPECS {
        rekey_index_table(&tx, spec, &mut report)?;
        after_step(spec.table, &tx)?;
    }
    rekey_sync_jobs(&tx, &mut report)?;
    after_step("sync_jobs", &tx)?;
    rekey_sync_conflicts(&tx, &mut report)?;
    after_step("sync_conflicts", &tx)?;
    rekey_refused_moves(&tx, &mut report)?;
    after_step("refused_moves", &tx)?;

    let left = count_non_canonical(&tx)?;
    if left != 0 {
        return Err(AppError::Storage(format!(
            "NFC re-key left {left} non-canonical path value(s); rolled back"
        )));
    }
    tx.commit()?;
    tracing::info!(
        rows_rekeyed = report.rows_rekeyed,
        collisions_dropped = report.collisions_dropped,
        jobs_dropped = report.jobs_dropped,
        local_deletes_deferred = report.local_deletes_deferred,
        "NFC re-key complete"
    );
    Ok(report)
}

/// File-name prefix and suffix shared by [`backup_path`] and [`existing_backups`].
fn backup_name_prefix(db_path: &Path) -> String {
    let name = db_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "app.db".to_string());
    format!("{name}.pre-nfc-rekey-")
}

/// Deletes the backup a failed earlier attempt left, once `fresh` has been written. The
/// attempt is identified by `startup-error.txt`, whose text ends `(pre-migration backup:
/// <file name>)`. The name must equal, byte for byte, the file name of one of this
/// database's own `<db>.pre-nfc-rekey-*.bak` files other than `fresh`; anything else is left
/// alone. Best effort: a failure to delete only costs disk space.
fn remove_superseded_backup(db_path: &Path, fresh: &Path) {
    let Ok(marker) = std::fs::read_to_string(crate::storage::db::startup_error_path(db_path))
    else {
        return;
    };
    let Some((_, rest)) = marker.rsplit_once(BACKUP_MARKER) else {
        return;
    };
    let named = rest.trim_start().split(')').next().unwrap_or("");
    for b in existing_backups(db_path) {
        if b != fresh && b.file_name().and_then(|n| n.to_str()) == Some(named) {
            match std::fs::remove_file(&b) {
                Ok(()) => {
                    tracing::info!(backup = %b.display(), "removed the backup of the failed attempt");
                }
                Err(e) => {
                    tracing::warn!(error = %e, backup = %b.display(), "could not remove the superseded backup")
                }
            }
        }
    }
}

/// Backups already next to `db_path`, in file-name order. Nothing depends on the order.
fn existing_backups(db_path: &Path) -> Vec<PathBuf> {
    let dir = match db_path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let prefix = backup_name_prefix(db_path);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".bak"))
        })
        .collect();
    found.sort();
    found
}

fn backup_path(db_path: &Path) -> PathBuf {
    backup_path_at(db_path, &Utc::now().format("%Y%m%dT%H%M%S%3fZ").to_string())
}

/// `<db>.pre-nfc-rekey-<stamp>-<pid>.bak`. The pid separates two launches in the same
/// millisecond; if that name still exists (same process, same stamp) a counter is appended,
/// so a name is never reused and `VACUUM INTO` never meets an existing file.
fn backup_path_at(db_path: &Path, stamp: &str) -> PathBuf {
    let prefix = backup_name_prefix(db_path);
    let pid = std::process::id();
    let mut candidate = db_path.with_file_name(format!("{prefix}{stamp}-{pid}.bak"));
    let mut n = 1u32;
    while candidate.exists() {
        candidate = db_path.with_file_name(format!("{prefix}{stamp}-{pid}-{n}.bak"));
        n += 1;
    }
    candidate
}

/// Number of non-NULL path values, over every column in [`PATH_COLUMNS`], that are not in
/// the canonical form.
fn count_non_canonical(conn: &Connection) -> AppResult<usize> {
    let mut n = 0usize;
    for (table, cols) in PATH_COLUMNS {
        for col in *cols {
            let mut stmt = conn.prepare(&format!(
                "SELECT {col} FROM {table} WHERE {col} IS NOT NULL"
            ))?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let v: String = row.get(0)?;
                if !is_canonical_form(&v) {
                    n += 1;
                }
            }
        }
    }
    Ok(n)
}

// ---------------------------------------------------------------------------------------
// Timestamps and small helpers
// ---------------------------------------------------------------------------------------

/// Sort key for an RFC 3339 string; unparsable or missing values sort lowest, then by text.
fn ts_of(v: Option<&Value>) -> (i64, String) {
    match v {
        Some(Value::Text(s)) => (
            DateTime::parse_from_rfc3339(s)
                .ok()
                .and_then(|d| d.timestamp_nanos_opt())
                .unwrap_or(i64::MIN),
            s.clone(),
        ),
        _ => (i64::MIN, String::new()),
    }
}

fn present(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Text(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

fn int_of(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Integer(i)) => *i,
        _ => 0,
    }
}

fn canon_opt(v: &Option<String>) -> Option<String> {
    v.as_deref().map(to_canonical_form)
}

fn non_canonical_opt(v: &Option<String>) -> bool {
    v.as_deref().is_some_and(|s| !is_canonical_form(s))
}

// ---------------------------------------------------------------------------------------
// Index tables: local_file_index, remote_file_index, known_folders
// ---------------------------------------------------------------------------------------

struct IdxRow {
    key: String,
    /// Aligned with [`IndexSpec::cols`].
    vals: Vec<Value>,
}

struct IndexSpec {
    table: &'static str,
    /// Every column except `relative_path`.
    cols: &'static [&'static str],
    /// Fold information the loser holds into the winner; returns whether anything changed.
    merge: Option<fn(&IndexSpec, &mut IdxRow, &IdxRow) -> bool>,
}

impl IndexSpec {
    fn val<'a>(&self, row: &'a IdxRow, col: &str) -> Option<&'a Value> {
        self.cols
            .iter()
            .position(|c| *c == col)
            .and_then(|i| row.vals.get(i))
    }

    fn describe(&self, row: &IdxRow) -> String {
        let mut s = format!("relative_path={:?}", row.key);
        for (c, v) in self.cols.iter().zip(&row.vals) {
            s.push_str(&format!(", {c}={v:?}"));
        }
        s
    }
}

fn newer_then_modified(spec: &IndexSpec, a: &IdxRow, b: &IdxRow) -> Ordering {
    ts_of(spec.val(a, "updated_at"))
        .cmp(&ts_of(spec.val(b, "updated_at")))
        .then_with(|| int_of(spec.val(a, "modified_ts")).cmp(&int_of(spec.val(b, "modified_ts"))))
}

/// A `dropbox_id` (and a folder's `owns_content`) is a fact about the item, not about one
/// spelling of it, so the winner inherits it from the loser instead of losing it. Content
/// fields (`rev`, `content_hash`, timestamps) are never merged: they stay with the newest
/// row, because a stale one is harmful (a stale `rev` turns a later delete into a
/// `RevConflict`).
fn merge_identity(spec: &IndexSpec, winner: &mut IdxRow, loser: &IdxRow) -> bool {
    let mut changed = false;
    if let Some(i) = spec.cols.iter().position(|c| *c == "dropbox_id") {
        if !present(winner.vals.get(i)) && present(loser.vals.get(i)) {
            winner.vals[i] = loser.vals[i].clone();
            changed = true;
        }
    }
    if let Some(i) = spec.cols.iter().position(|c| *c == "owns_content") {
        let m = int_of(winner.vals.get(i)).max(int_of(loser.vals.get(i)));
        if m != int_of(winner.vals.get(i)) {
            winner.vals[i] = Value::Integer(m);
            changed = true;
        }
    }
    changed
}

const INDEX_SPECS: &[IndexSpec] = &[
    IndexSpec {
        table: "local_file_index",
        cols: &["hash", "size_bytes", "modified_ts", "updated_at", "item_id"],
        merge: None,
    },
    IndexSpec {
        table: "remote_file_index",
        cols: &[
            "content_hash",
            "rev",
            "modified_ts",
            "updated_at",
            "dropbox_id",
        ],
        merge: Some(merge_identity),
    },
    IndexSpec {
        table: "known_folders",
        cols: &["updated_at", "dropbox_id", "owns_content"],
        merge: Some(merge_identity),
    },
];

/// Total order used to pick a winner: newest, then "already canonical", then the smaller
/// key. Greater is better.
fn rank(spec: &IndexSpec, a: &IdxRow, b: &IdxRow) -> Ordering {
    newer_then_modified(spec, a, b)
        .then_with(|| is_canonical_form(&a.key).cmp(&is_canonical_form(&b.key)))
        .then_with(|| b.key.cmp(&a.key))
}

fn rekey_index_table(
    tx: &Transaction<'_>,
    spec: &IndexSpec,
    report: &mut RekeyReport,
) -> AppResult<()> {
    let table = spec.table;
    let mut rows: Vec<IdxRow> = Vec::new();
    {
        let mut stmt = tx.prepare(&format!(
            "SELECT relative_path, {} FROM {table}",
            spec.cols.join(", ")
        ))?;
        let mut q = stmt.query([])?;
        while let Some(r) = q.next()? {
            let mut vals = Vec::with_capacity(spec.cols.len());
            for i in 0..spec.cols.len() {
                vals.push(r.get::<_, Value>(i + 1)?);
            }
            rows.push(IdxRow {
                key: r.get(0)?,
                vals,
            });
        }
    }
    let affected: HashSet<String> = rows
        .iter()
        .filter(|r| !is_canonical_form(&r.key))
        .map(|r| to_canonical_form(&r.key))
        .collect();
    if affected.is_empty() {
        return Ok(());
    }
    let mut groups: BTreeMap<String, Vec<IdxRow>> = BTreeMap::new();
    for r in rows {
        let canon = to_canonical_form(&r.key);
        if affected.contains(&canon) {
            groups.entry(canon).or_default().push(r);
        }
    }

    let set_cols = spec
        .cols
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{c} = ?{}", i + 2))
        .collect::<Vec<_>>()
        .join(", ");
    let update_sql = format!(
        "UPDATE {table} SET relative_path = ?1, {set_cols} WHERE relative_path = ?{}",
        spec.cols.len() + 2
    );

    for (canon, mut group) in groups {
        group.sort_by(|a, b| rank(spec, b, a));
        let mut winner = group.remove(0);
        let old_key = winner.key.clone();
        let mut merged = false;
        // Losers go first: the PRIMARY KEY would reject the winner's UPDATE otherwise.
        for loser in &group {
            tracing::warn!(
                table,
                canonical_key = %canon,
                kept = %spec.describe(&winner),
                dropped = %spec.describe(loser),
                "NFC re-key collision: two rows for one path; keeping the first, dropping the second"
            );
            if let Some(m) = spec.merge {
                merged |= m(spec, &mut winner, loser);
            }
            tx.execute(
                &format!("DELETE FROM {table} WHERE relative_path = ?1"),
                params![loser.key],
            )?;
            report.collisions_dropped += 1;
        }
        if old_key != canon || merged {
            if merged {
                tracing::warn!(
                    table,
                    canonical_key = %canon,
                    result = %spec.describe(&winner),
                    "NFC re-key collision: folded the dropped row's facts into the kept row"
                );
            }
            let mut p: Vec<Value> = Vec::with_capacity(spec.cols.len() + 2);
            p.push(Value::Text(canon.clone()));
            p.extend(winner.vals.iter().cloned());
            p.push(Value::Text(old_key));
            tx.execute(&update_sql, params_from_iter(p))?;
            report.rows_rekeyed += 1;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// sync_jobs
// ---------------------------------------------------------------------------------------

struct JobRow {
    id: i64,
    job_type: String,
    source: Option<String>,
    target: Option<String>,
    on_success: Option<String>,
    on_success_rev: Option<String>,
    status: String,
    updated_at: String,
    delta_deferred: i64,
    attempt_count: i64,
    delete_parent_rev: Option<String>,
    last_error: Option<String>,
}

impl JobRow {
    fn active(&self) -> bool {
        matches!(self.status.as_str(), "queued" | "retry_wait" | "running")
    }

    /// Not `done`: still able to run, or able to be revived by Retry.
    fn not_done(&self) -> bool {
        self.status != "done"
    }

    fn needs_rekey(&self) -> bool {
        non_canonical_opt(&self.source)
            || non_canonical_opt(&self.target)
            || non_canonical_opt(&self.on_success)
    }
}

/// ONE grouping rule. Every job that is not `done` is grouped by its canonical
/// `(job_type, target)`. A group with more than one member and at least one member this
/// migration re-keys is collapsed to a single survivor; groups the migration did not create
/// are left alone.
///
/// * **Winner:** the newest *active* job (`queued`, `retry_wait`, `running`); if no member is
///   active, the newest `failed` one. "Newest" is `updated_at`, then highest `id`.
/// * **Losers** (active or failed alike) are **deleted**, logged at `warn` with the full row
///   (the `VACUUM INTO` backup holds it too). A loser is never left `failed`: before DBSYNC-113
///   "Retry failed jobs" (`requeue_failed_jobs`) would put it back to `queued` next to the
///   winner, violating the partial unique index (Retry then errored for every job); today
///   Retry settles such a row as `done` instead, but a left-over `failed` loser would still
///   show its stale error in the tray until then, and, once the winner finished, could be
///   revived, which for a non-deferred `local_delete` is a destructive job running unverified
///   (DBSYNC-102 settled that such a job is dropped, not failed, and never re-aimed).
/// * **Upload debt.** An upload's source-deletion debt (`on_success_delete_path` / `_rev`)
///   follows `enqueue_upload_then_delete` (`COALESCE`: first debt wins, rev follows its path).
///   A winner without a debt inherits the loser's. A loser that owes a **different** deletion
///   cannot be merged (one upload settles one source), so it is not deleted: it is kept as
///   `done` with an explicit `last_error` and its debt intact, logged at `error`. That is the
///   shape `Db::unsettled_source_deletion` already reports ("a debt sitting on a terminal
///   upload row"), so the deletion is surfaced, not lost.
/// * **Defer.** A `local_delete` that is not `done` and that the migration re-keyed, or that
///   wins a group containing a re-keyed member, becomes `delta_deferred` (as does a winner
///   whose loser was deferred). Such a job was created by the bug being fixed (a snapshot
///   sweep that judged an NFD spelling "absent"); nothing proves its verdict. Deferred routes
///   it through `verify_and_delete_local_file_core` (DBSYNC-102): still on Dropbox, it is
///   kept; gone, it is re-hashed and deleted; a lookup or read failure retries.
///   `requeue_failed_jobs` keeps the flag, so a revived failed job verifies too.
///   `pick_next_due_job` runs a `queued` job at once and a `retry_wait` job when its own
///   `next_retry_at` is due, so no timestamp needs setting.
fn rekey_sync_jobs(tx: &Transaction<'_>, report: &mut RekeyReport) -> AppResult<()> {
    let mut jobs: Vec<JobRow> = Vec::new();
    {
        let mut stmt = tx.prepare(
            "SELECT id, job_type, source_path, target_path, on_success_delete_path, status, \
             updated_at, delta_deferred, attempt_count, delete_parent_rev, last_error, \
             on_success_delete_rev \
             FROM sync_jobs",
        )?;
        let mut q = stmt.query([])?;
        while let Some(r) = q.next()? {
            jobs.push(JobRow {
                id: r.get(0)?,
                job_type: r.get(1)?,
                source: r.get(2)?,
                target: r.get(3)?,
                on_success: r.get(4)?,
                status: r.get(5)?,
                updated_at: r.get(6)?,
                delta_deferred: r.get(7)?,
                attempt_count: r.get(8)?,
                delete_parent_rev: r.get(9)?,
                last_error: r.get(10)?,
                on_success_rev: r.get(11)?,
            });
        }
    }
    if !jobs.iter().any(JobRow::needs_rekey) {
        return Ok(());
    }

    let mut groups: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
    for (i, j) in jobs.iter().enumerate() {
        if j.not_done() {
            if let Some(t) = &j.target {
                groups
                    .entry((j.job_type.clone(), to_canonical_form(t)))
                    .or_default()
                    .push(i);
            }
        }
    }
    let mut losers: HashSet<usize> = HashSet::new();
    // Losers that owe a different source deletion than the winner: kept as `done`.
    let mut kept_for_debt: HashSet<usize> = HashSet::new();
    // Survivors whose debt columns changed (inherited from a loser).
    let mut inherited: HashSet<usize> = HashSet::new();
    let mut promote_deferred: Vec<usize> = Vec::new();
    for ((job_type, canon), mut members) in groups {
        if members.len() < 2 || !members.iter().any(|&i| jobs[i].needs_rekey()) {
            continue;
        }
        // Active before failed, then newest, then highest id.
        members.sort_by(|&a, &b| {
            jobs[b]
                .active()
                .cmp(&jobs[a].active())
                .then_with(|| {
                    ts_of(Some(&Value::Text(jobs[b].updated_at.clone())))
                        .cmp(&ts_of(Some(&Value::Text(jobs[a].updated_at.clone()))))
                })
                .then_with(|| jobs[b].id.cmp(&jobs[a].id))
        });
        let winner = members[0];
        let mut any_deferred = jobs[winner].delta_deferred != 0;
        // A re-keyed `local_delete` in the group carries the original bug's unverified
        // verdict; the survivor inherits the need to verify even if it was canonical itself.
        if job_type == "local_delete" {
            any_deferred = true;
        }
        for &l in &members[1..] {
            let d = &jobs[l];
            tracing::warn!(
                job_type = %job_type,
                canonical_target = %canon,
                kept_job = jobs[winner].id,
                kept_status = %jobs[winner].status,
                dropped_job = d.id,
                dropped_type = %d.job_type,
                dropped_source = ?d.source,
                dropped_target = ?d.target,
                dropped_on_success_delete_path = ?d.on_success,
                dropped_status = %d.status,
                dropped_attempts = d.attempt_count,
                dropped_delta_deferred = d.delta_deferred != 0,
                dropped_parent_rev = ?d.delete_parent_rev,
                dropped_last_error = ?d.last_error,
                "NFC re-key collision: two jobs for one path; deleting the older one \
                 (it is in the pre-migration backup)"
            );
            if job_type == "upload" {
                if let Some(owed) = jobs[l].on_success.clone() {
                    match jobs[winner].on_success.clone() {
                        None => {
                            jobs[winner].on_success = Some(owed);
                            jobs[winner].on_success_rev = jobs[l].on_success_rev.clone();
                            inherited.insert(winner);
                        }
                        Some(w) if to_canonical_form(&w) != to_canonical_form(&owed) => {
                            tracing::error!(
                                kept_job = jobs[winner].id,
                                kept_owes = %w,
                                dropped_job = jobs[l].id,
                                dropped_owes = %owed,
                                dropped_owes_rev = ?jobs[l].on_success_rev,
                                "NFC re-key collision: two uploads for one path owe DIFFERENT \
                                 source deletions; keeping the older one as a finished job so \
                                 its debt stays visible instead of deleting it"
                            );
                            kept_for_debt.insert(l);
                            continue;
                        }
                        Some(_) => {}
                    }
                }
            }
            any_deferred |= jobs[l].delta_deferred != 0;
            losers.insert(l);
        }
        if any_deferred && jobs[winner].delta_deferred == 0 {
            promote_deferred.push(winner);
        }
    }

    // Losers first, so their slot in the partial unique index is free before a survivor
    // takes the canonical key.
    let mut loser_idx: Vec<usize> = losers.iter().copied().collect();
    loser_idx.sort_unstable();
    for i in loser_idx {
        tx.execute("DELETE FROM sync_jobs WHERE id = ?1", params![jobs[i].id])?;
        report.jobs_dropped += 1;
    }
    let mut kept_idx: Vec<usize> = kept_for_debt.iter().copied().collect();
    kept_idx.sort_unstable();
    for i in kept_idx {
        tx.execute(
            "UPDATE sync_jobs SET status = 'done', last_error = ?2 WHERE id = ?1",
            params![
                jobs[i].id,
                "superseded by a newer upload of the same path during the NFC re-key; it owes a \
                 different source deletion that was NOT performed (DBSYNC-103)"
            ],
        )?;
    }
    for i in promote_deferred {
        tx.execute(
            "UPDATE sync_jobs SET delta_deferred = 1 WHERE id = ?1",
            params![jobs[i].id],
        )?;
        // Recorded in memory so the re-key loop below does not count this job again.
        jobs[i].delta_deferred = 1;
        if jobs[i].job_type == "local_delete" {
            report.local_deletes_deferred += 1;
        }
    }
    for (i, j) in jobs.iter().enumerate() {
        if losers.contains(&i) || !(j.needs_rekey() || inherited.contains(&i)) {
            continue;
        }
        // `MAX(delta_deferred, ?5)`: a re-keyed `local_delete` that is not `done` becomes
        // deferred; it is never un-deferred.
        let defer = j.job_type == "local_delete" && j.not_done();
        tx.execute(
            "UPDATE sync_jobs SET source_path = ?1, target_path = ?2, \
             on_success_delete_path = ?3, on_success_delete_rev = ?6, \
             delta_deferred = MAX(delta_deferred, ?5) \
             WHERE id = ?4",
            params![
                canon_opt(&j.source),
                canon_opt(&j.target),
                canon_opt(&j.on_success),
                j.id,
                i64::from(defer),
                j.on_success_rev
            ],
        )?;
        if defer && j.delta_deferred == 0 {
            report.local_deletes_deferred += 1;
        }
        if j.needs_rekey() {
            report.rows_rekeyed += 1;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// sync_conflicts
// ---------------------------------------------------------------------------------------

/// A conflict row is a record of something the user may still have to resolve, with no
/// uniqueness constraint. Every row is re-keyed in place and none is merged or dropped.
fn rekey_sync_conflicts(tx: &Transaction<'_>, report: &mut RekeyReport) -> AppResult<()> {
    let mut rows: Vec<(i64, String, String, Option<String>)> = Vec::new();
    {
        let mut stmt = tx.prepare(
            "SELECT id, local_path, remote_path, conflicted_copy_path FROM sync_conflicts",
        )?;
        let mut q = stmt.query([])?;
        while let Some(r) = q.next()? {
            rows.push((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?));
        }
    }
    for (id, local, remote, copy) in rows {
        if is_canonical_form(&local) && is_canonical_form(&remote) && !non_canonical_opt(&copy) {
            continue;
        }
        tx.execute(
            "UPDATE sync_conflicts SET local_path = ?1, remote_path = ?2, \
             conflicted_copy_path = ?3 WHERE id = ?4",
            params![
                to_canonical_form(&local),
                to_canonical_form(&remote),
                canon_opt(&copy),
                id
            ],
        )?;
        report.rows_rekeyed += 1;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// refused_moves
// ---------------------------------------------------------------------------------------

struct RefusedRow {
    from: String,
    to: String,
    refused_at: String,
}

/// `(from_path, to_path)` is the primary key. Two spellings of one refused pair say the
/// same thing, so the newest refusal wins and the other is dropped with a warning.
fn rekey_refused_moves(tx: &Transaction<'_>, report: &mut RekeyReport) -> AppResult<()> {
    let mut rows: Vec<RefusedRow> = Vec::new();
    {
        let mut stmt = tx.prepare("SELECT from_path, to_path, refused_at FROM refused_moves")?;
        let mut q = stmt.query([])?;
        while let Some(r) = q.next()? {
            rows.push(RefusedRow {
                from: r.get(0)?,
                to: r.get(1)?,
                refused_at: r.get(2)?,
            });
        }
    }
    let canon = |r: &RefusedRow| (to_canonical_form(&r.from), to_canonical_form(&r.to));
    let is_canon = |r: &RefusedRow| is_canonical_form(&r.from) && is_canonical_form(&r.to);
    let affected: HashSet<(String, String)> =
        rows.iter().filter(|r| !is_canon(r)).map(canon).collect();
    if affected.is_empty() {
        return Ok(());
    }
    let mut groups: BTreeMap<(String, String), Vec<RefusedRow>> = BTreeMap::new();
    for r in rows {
        let c = canon(&r);
        if affected.contains(&c) {
            groups.entry(c).or_default().push(r);
        }
    }
    for ((from, to), mut group) in groups {
        group.sort_by(|a, b| {
            ts_of(Some(&Value::Text(b.refused_at.clone())))
                .cmp(&ts_of(Some(&Value::Text(a.refused_at.clone()))))
                .then_with(|| is_canon(b).cmp(&is_canon(a)))
                .then_with(|| (&a.from, &a.to).cmp(&(&b.from, &b.to)))
        });
        let winner = group.remove(0);
        for loser in &group {
            tracing::warn!(
                canonical_from = %from,
                canonical_to = %to,
                kept_from = ?winner.from, kept_to = ?winner.to, kept_refused_at = %winner.refused_at,
                dropped_from = ?loser.from, dropped_to = ?loser.to, dropped_refused_at = %loser.refused_at,
                "NFC re-key collision: two refused-move rows for one pair; keeping the newest"
            );
            tx.execute(
                "DELETE FROM refused_moves WHERE from_path = ?1 AND to_path = ?2",
                params![loser.from, loser.to],
            )?;
            report.collisions_dropped += 1;
        }
        if winner.from != from || winner.to != to {
            tx.execute(
                "UPDATE refused_moves SET from_path = ?1, to_path = ?2 \
                 WHERE from_path = ?3 AND to_path = ?4",
                params![from, to, winner.from, winner.to],
            )?;
            report.rows_rekeyed += 1;
        }
    }
    Ok(())
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use crate::storage::db::Db;
    use rusqlite::Connection;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    const NFD: &str = "re\u{301}sume\u{301}";
    const NFC: &str = "r\u{e9}sum\u{e9}";

    fn fresh_path() -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "dbsync103-keymig-{}-{}",
            std::process::id(),
            N.fetch_add(1, AtomicOrdering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("app.db")
    }

    /// Opens through `Db::new_at` (so the schema is the production one), drops the handle,
    /// and returns a raw connection for planting legacy rows. Raw SQL is the only way to
    /// write NFD: the storage assert refuses it, exactly because a pre-103 build wrote it.
    fn legacy_db() -> (PathBuf, Connection) {
        let path = fresh_path();
        drop(Db::new_at(&path).expect("init"));
        let conn = Connection::open(&path).unwrap();
        (path, conn)
    }

    fn migrate(path: &Path) -> AppResult<RekeyReport> {
        let mut conn = Connection::open(path).unwrap();
        rekey_to_canonical_form(&mut conn, path)
    }

    fn backups(path: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.to_string_lossy().contains(".pre-nfc-rekey-"))
            .collect()
    }

    fn col_strings(conn: &Connection, sql: &str) -> Vec<String> {
        let mut stmt = conn.prepare(sql).unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn every_text_column_is_classified() {
        // A new TEXT column must be a path column (add it to PATH_COLUMNS) or be named
        // here as not a path. This is what stops a forgotten column.
        const NOT_PATHS: &[(&str, &str)] = &[
            ("app_config", "key"),
            ("app_config", "value"),
            ("app_config", "updated_at"),
            ("sync_jobs", "job_type"),
            ("sync_jobs", "status"),
            ("sync_jobs", "next_retry_at"),
            ("sync_jobs", "created_at"),
            ("sync_jobs", "updated_at"),
            ("sync_jobs", "last_error"),
            ("sync_jobs", "upload_session_id"),
            ("sync_jobs", "delete_parent_rev"),
            ("sync_jobs", "on_success_delete_rev"),
            ("sync_conflicts", "reason"),
            ("sync_conflicts", "created_at"),
            ("local_file_index", "hash"),
            ("local_file_index", "updated_at"),
            ("remote_file_index", "content_hash"),
            ("remote_file_index", "rev"),
            ("remote_file_index", "updated_at"),
            ("remote_file_index", "dropbox_id"),
            ("known_folders", "updated_at"),
            ("known_folders", "dropbox_id"),
            ("refused_moves", "refused_at"),
        ];
        let (_p, conn) = legacy_db();
        let tables = col_strings(
            &conn,
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
        );
        let mut unclassified = Vec::new();
        for t in tables {
            let mut stmt = conn.prepare(&format!("PRAGMA table_info({t})")).unwrap();
            let cols: Vec<(String, String)> = stmt
                .query_map([], |r| Ok((r.get(1)?, r.get::<_, String>(2)?)))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            for (c, ty) in cols {
                if !ty.eq_ignore_ascii_case("TEXT") {
                    continue;
                }
                let is_path = PATH_COLUMNS
                    .iter()
                    .any(|(pt, pc)| *pt == t && pc.contains(&c.as_str()))
                    || (t == "known_folders"
                        || t == "local_file_index"
                        || t == "remote_file_index")
                        && c == "relative_path";
                let is_not = NOT_PATHS.iter().any(|(nt, nc)| *nt == t && *nc == c);
                if !is_path && !is_not {
                    unclassified.push(format!("{t}.{c}"));
                }
            }
        }
        assert!(
            unclassified.is_empty(),
            "unclassified TEXT columns (path or not?): {unclassified:?}"
        );
    }

    #[test]
    fn an_already_canonical_database_changes_nothing_and_writes_no_backup() {
        let (path, conn) = legacy_db();
        conn.execute(
            "INSERT INTO known_folders(relative_path, updated_at) VALUES(?1, 'x')",
            [NFC],
        )
        .unwrap();
        let report = migrate(&path).unwrap();
        assert_eq!(report, RekeyReport::default());
        assert!(backups(&path).is_empty());
    }

    #[test]
    fn migrates_every_path_column_and_is_idempotent() {
        let (path, conn) = legacy_db();
        let f = format!("{NFD}/a.txt");
        conn.execute_batch(&format!(
            "INSERT INTO local_file_index(relative_path, hash, size_bytes, modified_ts, updated_at, item_id) VALUES('{f}', 'h', 1, 1, '2026-01-01T00:00:00+00:00', 7);
             INSERT INTO remote_file_index(relative_path, content_hash, rev, modified_ts, updated_at, dropbox_id) VALUES('{f}', 'h', 'r', 1, '2026-01-01T00:00:00+00:00', 'id:1');
             INSERT INTO known_folders(relative_path, updated_at) VALUES('{NFD}', 'x');
             INSERT INTO sync_jobs(job_type, source_path, target_path, status, created_at, updated_at, on_success_delete_path) VALUES('move', '{NFD}', '{NFD}2', 'retry_wait', 'c', 'u', '{NFD}/del');
             INSERT INTO sync_conflicts(local_path, remote_path, reason, conflicted_copy_path, created_at) VALUES('{f}', '{f}', 'r', '{NFD}/copy', 'c');
             INSERT INTO refused_moves(from_path, to_path, refused_at) VALUES('{NFD}', '{NFD}x', 't');"
        ))
        .unwrap();
        assert!(count_non_canonical(&conn).unwrap() > 0);

        let report = migrate(&path).unwrap();
        assert_eq!(report.collisions_dropped, 0);
        assert_eq!(report.rows_rekeyed, 6);
        assert_eq!(backups(&path).len(), 1, "exactly one backup");
        let conn = Connection::open(&path).unwrap();
        assert_eq!(count_non_canonical(&conn).unwrap(), 0);
        let nfc_f = format!("{NFC}/a.txt");
        assert_eq!(
            col_strings(&conn, "SELECT relative_path FROM local_file_index"),
            vec![nfc_f.clone()]
        );
        assert_eq!(
            col_strings(&conn, "SELECT relative_path FROM remote_file_index"),
            vec![nfc_f.clone()]
        );
        assert_eq!(
            col_strings(&conn, "SELECT relative_path FROM known_folders"),
            vec![NFC.to_string()]
        );
        assert_eq!(
            col_strings(
                &conn,
                "SELECT source_path||'|'||target_path||'|'||on_success_delete_path FROM sync_jobs"
            ),
            vec![format!("{NFC}|{NFC}2|{NFC}/del")]
        );
        assert_eq!(
            col_strings(
                &conn,
                "SELECT local_path||'|'||remote_path||'|'||conflicted_copy_path FROM sync_conflicts"
            ),
            vec![format!("{nfc_f}|{nfc_f}|{NFC}/copy")]
        );
        assert_eq!(
            col_strings(&conn, "SELECT from_path||'|'||to_path FROM refused_moves"),
            vec![format!("{NFC}|{NFC}x")]
        );
        // The backup holds the ORIGINAL rows.
        let bk = Connection::open(&backups(&path)[0]).unwrap();
        assert_eq!(
            col_strings(&bk, "SELECT relative_path FROM known_folders"),
            vec![NFD.to_string()]
        );

        // Second run: nothing to do, no second backup.
        let again = migrate(&path).unwrap();
        assert_eq!(again, RekeyReport::default());
        assert_eq!(backups(&path).len(), 1, "a no-op run writes no backup");
    }

    /// Reopens through the production path (`Db::new_at`: schema migrate FIRST, then this
    /// re-key). Collision fixtures go through here, never through `migrate(path)` above,
    /// because the schema migrate backfills `local_file_index.item_id` on every row and so
    /// decides which states this module can ever see.
    fn reopen(path: &Path) -> Db {
        Db::new_at(path).expect("startup migrates")
    }

    /// Both spellings of one path are in the table, written the way pre-103 builds wrote
    /// them: the scan wrote the on-disk (NFD) spelling and left `item_id` NULL until the
    /// schema migrate's backfill, the delta echo wrote the Dropbox (NFC) spelling.
    fn plant_local_pair(conn: &Connection, nfd_updated: &str, nfc_updated: &str) {
        conn.execute_batch(&format!(
            "INSERT INTO local_file_index VALUES('{NFD}', 'nfd-hash', 1, 1, '{nfd_updated}', NULL);
             INSERT INTO local_file_index VALUES('{NFC}', 'nfc-hash', 1, 1, '{nfc_updated}', NULL);"
        ))
        .unwrap();
    }

    #[test]
    fn local_index_collision_keeps_the_newest_row_whichever_spelling_it_has() {
        for (nfd_ts, nfc_ts, winner) in [
            (
                "2026-01-01T00:00:00+00:00",
                "2026-02-01T00:00:00+00:00",
                "nfc-hash",
            ),
            (
                "2026-02-01T00:00:00+00:00",
                "2026-01-01T00:00:00+00:00",
                "nfd-hash",
            ),
        ] {
            let (path, conn) = legacy_db();
            plant_local_pair(&conn, nfd_ts, nfc_ts);
            // The ids the schema migrate hands out, read the way `Db::new_at` will produce
            // them, so the survivor's identity can be compared with its own row's.
            let mut c = Connection::open(&path).unwrap();
            crate::storage::db::migrate(&mut c).unwrap();
            let ids: HashMap<String, i64> = {
                let mut stmt = c
                    .prepare("SELECT relative_path, item_id FROM local_file_index")
                    .unwrap();
                stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                    .unwrap()
                    .map(|r| r.unwrap())
                    .collect()
            };
            drop((c, conn));

            drop(reopen(&path));
            let conn = Connection::open(&path).unwrap();
            assert_eq!(count(&conn, "SELECT COUNT(*) FROM local_file_index"), 1);
            let (key, hash, item_id): (String, String, i64) = conn
                .query_row(
                    "SELECT relative_path, hash, item_id FROM local_file_index",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap();
            assert_eq!(key, NFC);
            assert_eq!(hash, winner, "the newest row's content wins");
            let winner_key = if winner == "nfc-hash" { NFC } else { NFD };
            assert_eq!(item_id, ids[winner_key], "and so does its identity");
        }
    }

    #[test]
    fn remote_index_collision_takes_rev_and_hash_from_the_newest_row_and_keeps_the_id() {
        // (a) The pre-103 upload wrote the NFD row (newest, no id); the delta echo wrote the
        // NFC row (older, with the id). The newer rev must win, and the id must survive.
        // (b) The reverse: the id is on the OLDER NFD row and the newest NFC row has none.
        for (nfd, nfc, expect) in [
            (
                "'new-hash', 'new-rev', 1, '2026-03-01T00:00:00+00:00', NULL",
                "'old-hash', 'old-rev', 1, '2026-01-01T00:00:00+00:00', 'id:9'",
                "new-hash|new-rev|id:9",
            ),
            (
                "'old-hash', 'old-rev', 1, '2026-01-01T00:00:00+00:00', 'id:9'",
                "'new-hash', 'new-rev', 1, '2026-03-01T00:00:00+00:00', NULL",
                "new-hash|new-rev|id:9",
            ),
        ] {
            let (path, conn) = legacy_db();
            conn.execute_batch(&format!(
                "INSERT INTO remote_file_index VALUES('{NFD}', {nfd});
                 INSERT INTO remote_file_index VALUES('{NFC}', {nfc});"
            ))
            .unwrap();
            drop(conn);
            drop(reopen(&path));
            let conn = Connection::open(&path).unwrap();
            assert_eq!(
                col_strings(
                    &conn,
                    "SELECT content_hash||'|'||rev||'|'||dropbox_id FROM remote_file_index"
                ),
                vec![expect.to_string()],
                "a stale rev must never survive a collision"
            );
            assert_eq!(
                col_strings(&conn, "SELECT relative_path FROM remote_file_index"),
                vec![NFC.to_string()]
            );
        }
    }

    /// L3. The state a pre-103 build leaves behind, built by raw SQL: the scan keyed the
    /// on-disk NFD spelling, the delta echo keyed Dropbox's NFC spelling, for the same path,
    /// in all three index tables. One row survives per table and the assertions name WHICH
    /// row's hash, rev and id it is, not just how many rows remain.
    #[test]
    fn a_pre_103_scan_plus_a_delta_echo_collapse_to_one_row_each_and_the_right_row_survives() {
        let (path, conn) = legacy_db();
        conn.execute_batch(&format!(
            "INSERT INTO local_file_index VALUES('{NFD}', 'scan-hash', 5, 1, '2026-01-01T00:00:00+00:00', NULL);
             INSERT INTO local_file_index VALUES('{NFC}', 'echo-hash', 5, 2, '2026-02-01T00:00:00+00:00', NULL);
             INSERT INTO remote_file_index VALUES('{NFD}', 'scan-hash', 'upload-rev', 1, '2026-01-01T00:00:00+00:00', NULL);
             INSERT INTO remote_file_index VALUES('{NFC}', 'echo-hash', 'echo-rev', 2, '2026-02-01T00:00:00+00:00', 'id:echo');
             INSERT INTO known_folders(relative_path, updated_at, dropbox_id, owns_content) VALUES('{NFD}', '2026-01-01T00:00:00+00:00', NULL, 1);
             INSERT INTO known_folders(relative_path, updated_at, dropbox_id, owns_content) VALUES('{NFC}', '2026-02-01T00:00:00+00:00', 'id:echo', 0);"
        ))
        .unwrap();
        drop(conn);

        let db = reopen(&path);
        let local = db.get_local_file(NFC).unwrap().expect("one local row");
        assert_eq!(local.hash, "echo-hash");
        let remote = db.get_remote_file(NFC).unwrap().expect("one remote row");
        assert_eq!(
            (
                remote.content_hash.as_str(),
                remote.rev.as_str(),
                remote.dropbox_id.as_deref()
            ),
            ("echo-hash", "echo-rev", Some("id:echo"))
        );
        assert_eq!(db.list_known_folders().unwrap(), vec![NFC.to_string()]);
        drop(db);

        let conn = Connection::open(&path).unwrap();
        for t in ["local_file_index", "remote_file_index", "known_folders"] {
            assert_eq!(count(&conn, &format!("SELECT COUNT(*) FROM {t}")), 1, "{t}");
        }
        assert_eq!(
            col_strings(
                &conn,
                "SELECT dropbox_id||'|'||owns_content FROM known_folders"
            ),
            vec!["id:echo|1".to_string()],
            "the folder keeps the id and the owns_content = 1 the dropped row held"
        );
        assert_eq!(count_non_canonical(&conn).unwrap(), 0);
    }

    #[test]
    fn known_folders_collision_keeps_the_id_and_folds_owns_content() {
        let (path, conn) = legacy_db();
        conn.execute_batch(&format!(
            "INSERT INTO known_folders(relative_path, updated_at, dropbox_id, owns_content) VALUES('{NFD}', '2026-01-01T00:00:00+00:00', 'id:f', 0);
             INSERT INTO known_folders(relative_path, updated_at, dropbox_id, owns_content) VALUES('{NFC}', '2026-02-01T00:00:00+00:00', NULL, 1);"
        ))
        .unwrap();
        drop(conn);
        drop(reopen(&path));
        let conn = Connection::open(&path).unwrap();
        assert_eq!(
            col_strings(
                &conn,
                "SELECT relative_path||'|'||dropbox_id||'|'||owns_content FROM known_folders"
            ),
            vec![format!("{NFC}|id:f|1")],
            "the id wins, and owns_content=1 from the dropped row is not lost"
        );
    }

    fn plant_job(
        conn: &Connection,
        job_type: &str,
        path: &str,
        status: &str,
        updated: &str,
        deferred: i64,
    ) {
        conn.execute(
            "INSERT INTO sync_jobs(job_type, source_path, target_path, status, created_at, \
             updated_at, delta_deferred, attempt_count, last_error) \
             VALUES(?1, ?2, ?2, ?3, 'c', ?4, ?5, 3, 'earlier error')",
            rusqlite::params![job_type, path, status, updated, deferred],
        )
        .unwrap();
    }

    #[test]
    fn two_active_jobs_on_one_key_keep_the_newest_and_delete_the_older() {
        let (path, conn) = legacy_db();
        plant_job(
            &conn,
            "upload",
            NFD,
            "retry_wait",
            "2026-01-01T00:00:00+00:00",
            0,
        );
        plant_job(
            &conn,
            "upload",
            NFC,
            "queued",
            "2026-02-01T00:00:00+00:00",
            0,
        );
        conn.execute(
            "INSERT INTO sync_jobs(job_type, source_path, target_path, status, created_at, updated_at) \
             VALUES('delete', NULL, ?1, 'queued', 'c', '2026-01-01T00:00:00+00:00')",
            [NFD],
        )
        .unwrap();
        drop(conn);
        drop(reopen(&path));
        let conn = Connection::open(&path).unwrap();
        assert_eq!(
            col_strings(
                &conn,
                "SELECT id||job_type||status FROM sync_jobs ORDER BY id"
            ),
            vec!["2uploadqueued".to_string(), "3deletequeued".to_string()],
            "the loser (job 1) is gone, not parked as failed"
        );
        // A different job_type on the same key is not a collision and is simply re-keyed.
        assert_eq!(
            col_strings(&conn, "SELECT target_path FROM sync_jobs WHERE id = 3"),
            vec![NFC.to_string()]
        );
        assert_eq!(count_non_canonical(&conn).unwrap(), 0);
    }

    /// H1. A loser left as `failed` made `requeue_failed_jobs` collide with the winner's
    /// slot in the partial unique index (`UNIQUE constraint failed`), which broke Retry for
    /// EVERY job, and put "superseded..." in the tray as the latest error.
    #[test]
    fn a_migration_collision_leaves_retry_working_and_no_error_in_the_tray() {
        let (path, conn) = legacy_db();
        plant_job(
            &conn,
            "upload",
            NFD,
            "retry_wait",
            "2026-01-01T00:00:00+00:00",
            0,
        );
        plant_job(
            &conn,
            "upload",
            NFC,
            "queued",
            "2026-02-01T00:00:00+00:00",
            0,
        );
        drop(conn);
        let db = reopen(&path);
        assert_eq!(db.requeue_failed_jobs().expect("Retry must not collide"), 0);
        assert_eq!(db.latest_failed_error().unwrap(), None);
        assert_eq!(db.list_recent_jobs(10).unwrap().len(), 1);
    }

    /// H1, the data-loss shape. Probe B: an OLDER non-deferred `local_delete` on the NFD
    /// spelling (what the original bug's sweep enqueued) and a NEWER deferred one on the NFC
    /// spelling. Once the winner is finished or cancelled, Retry must not be able to bring
    /// the loser back as a runnable, unverified delete.
    #[test]
    fn a_losing_local_delete_can_never_be_revived_by_retry() {
        for finish in ["cancel", "complete"] {
            let (path, conn) = legacy_db();
            plant_job(
                &conn,
                "local_delete",
                NFD,
                "queued",
                "2026-01-01T00:00:00+00:00",
                0,
            );
            plant_job(
                &conn,
                "local_delete",
                NFC,
                "retry_wait",
                "2026-02-01T00:00:00+00:00",
                1,
            );
            drop(conn);
            let db = reopen(&path);

            let jobs = db.list_recent_jobs(10).unwrap();
            assert_eq!(jobs.len(), 1, "only the winner survives");
            assert_eq!(jobs[0].id, 2);
            assert!(jobs[0].delta_deferred);
            match finish {
                "cancel" => assert!(db.cancel_deferred_local_delete(NFC).unwrap()),
                _ => db.mark_job_completed(2).unwrap(),
            }

            assert_eq!(db.requeue_failed_jobs().expect("Retry"), 0);
            assert!(
                db.pick_next_due_job().unwrap().is_none(),
                "nothing may be runnable after the winner is {finish}ed"
            );
            assert_eq!(db.latest_failed_error().unwrap(), None);
        }
    }

    /// M1. An active `local_delete` that this migration re-keyed was created by the bug
    /// (a sweep that judged an NFD spelling absent); it must re-verify against Dropbox
    /// before it unlinks. Jobs the migration did not touch keep their flag.
    #[test]
    fn a_rekeyed_active_local_delete_becomes_deferred_and_nothing_else_does() {
        let (path, conn) = legacy_db();
        plant_job(
            &conn,
            "local_delete",
            NFD,
            "queued",
            "2026-01-01T00:00:00+00:00",
            0,
        );
        plant_job(
            &conn,
            "local_delete",
            &format!("{NFD}2"),
            "retry_wait",
            "2026-01-01T00:00:00+00:00",
            0,
        );
        plant_job(
            &conn,
            "local_delete",
            &format!("{NFD}3"),
            "done",
            "2026-01-01T00:00:00+00:00",
            0,
        );
        plant_job(
            &conn,
            "upload",
            &format!("{NFD}4"),
            "queued",
            "2026-01-01T00:00:00+00:00",
            0,
        );
        // Already canonical: created by the fixed code's sweep, untouched by the migration.
        plant_job(
            &conn,
            "local_delete",
            &format!("{NFC}5"),
            "queued",
            "2026-01-01T00:00:00+00:00",
            0,
        );
        drop(conn);
        drop(reopen(&path));
        let conn = Connection::open(&path).unwrap();
        assert_eq!(
            col_strings(
                &conn,
                "SELECT id||job_type||status||':'||delta_deferred FROM sync_jobs ORDER BY id"
            ),
            vec![
                "1local_deletequeued:1".to_string(),
                "2local_deleteretry_wait:1".to_string(),
                "3local_deletedone:0".to_string(),
                "4uploadqueued:0".to_string(),
                "5local_deletequeued:0".to_string(),
            ]
        );
    }

    #[test]
    fn a_done_job_never_collides_with_an_active_one() {
        let (path, conn) = legacy_db();
        plant_job(&conn, "upload", NFD, "done", "2026-03-01T00:00:00+00:00", 0);
        plant_job(
            &conn,
            "upload",
            NFC,
            "queued",
            "2026-01-01T00:00:00+00:00",
            0,
        );
        drop(conn);
        drop(reopen(&path));
        let conn = Connection::open(&path).unwrap();
        assert_eq!(
            col_strings(&conn, "SELECT status FROM sync_jobs ORDER BY id"),
            vec!["done".to_string(), "queued".to_string()]
        );
    }

    #[test]
    fn a_deferred_flag_on_the_dropped_duplicate_survives_on_the_kept_job() {
        let (path, conn) = legacy_db();
        plant_job(
            &conn,
            "local_delete",
            NFD,
            "retry_wait",
            "2026-01-01T00:00:00+00:00",
            1,
        );
        plant_job(
            &conn,
            "local_delete",
            NFC,
            "queued",
            "2026-02-01T00:00:00+00:00",
            0,
        );
        drop(conn);
        drop(reopen(&path));
        let conn = Connection::open(&path).unwrap();
        assert_eq!(
            col_strings(&conn, "SELECT id||':'||delta_deferred FROM sync_jobs"),
            vec!["2:1".to_string()]
        );
    }

    /// M1(a). A pre-existing `failed` NFD `local_delete` next to an ACTIVE deferred NFC one.
    /// Unhandled, the failed row is re-keyed onto the winner's key: Retry then fails with
    /// `UNIQUE constraint failed`, the tray shows its error, and after the winner is cancelled
    /// Retry revives it. It is deleted instead.
    #[test]
    fn a_failed_legacy_job_shadowed_by_an_active_one_cannot_break_or_outlive_retry() {
        let (path, conn) = legacy_db();
        plant_job(
            &conn,
            "local_delete",
            NFD,
            "failed",
            "2026-03-01T00:00:00+00:00",
            0,
        );
        plant_job(
            &conn,
            "local_delete",
            NFC,
            "retry_wait",
            "2026-02-01T00:00:00+00:00",
            1,
        );
        drop(conn);
        let db = reopen(&path);

        let jobs = db.list_recent_jobs(10).unwrap();
        assert_eq!(jobs.len(), 1, "the failed shadow is deleted");
        assert_eq!(jobs[0].id, 2);
        assert_eq!(db.latest_failed_error().unwrap(), None, "tray unaffected");
        assert_eq!(db.requeue_failed_jobs().expect("Retry must not collide"), 0);

        assert!(db.cancel_deferred_local_delete(NFC).unwrap());
        assert_eq!(db.requeue_failed_jobs().expect("Retry"), 0);
        assert!(
            db.pick_next_due_job().unwrap().is_none(),
            "nothing destructive may revive once the winner is cancelled"
        );
    }

    /// M1(a) for a non-destructive type: Retry must still work.
    #[test]
    fn a_failed_legacy_upload_shadowed_by_a_queued_one_does_not_break_retry() {
        let (path, conn) = legacy_db();
        plant_job(
            &conn,
            "upload",
            NFD,
            "failed",
            "2026-03-01T00:00:00+00:00",
            0,
        );
        plant_job(
            &conn,
            "upload",
            NFC,
            "queued",
            "2026-02-01T00:00:00+00:00",
            0,
        );
        drop(conn);
        let db = reopen(&path);
        assert_eq!(db.requeue_failed_jobs().expect("Retry must not collide"), 0);
        assert_eq!(db.list_recent_jobs(10).unwrap().len(), 1);
    }

    /// M1(b). A lone `failed` NFD `local_delete` (the original bug's job) is re-keyed and
    /// deferred, and Retry keeps `delta_deferred`, so the revived job verifies first.
    #[test]
    fn a_lone_failed_legacy_local_delete_is_deferred_and_stays_deferred_through_retry() {
        let (path, conn) = legacy_db();
        plant_job(
            &conn,
            "local_delete",
            NFD,
            "failed",
            "2026-01-01T00:00:00+00:00",
            0,
        );
        drop(conn);
        let db = reopen(&path);
        assert_eq!(db.requeue_failed_jobs().unwrap(), 1);
        let jobs = db.list_recent_jobs(10).unwrap();
        assert_eq!(jobs.len(), 1);
        assert!(jobs[0].delta_deferred, "a revived delete must re-verify");
        assert_eq!(jobs[0].target_path.as_deref(), Some(NFC));
    }

    /// Two `failed` jobs (NFD and NFC) fold onto one key with no active job. Left alone,
    /// Retry requeues both and violates the unique index.
    #[test]
    fn two_failed_legacy_jobs_folding_onto_one_key_collapse_to_one_deferred_survivor() {
        let (path, conn) = legacy_db();
        plant_job(
            &conn,
            "local_delete",
            NFD,
            "failed",
            "2026-01-01T00:00:00+00:00",
            0,
        );
        plant_job(
            &conn,
            "local_delete",
            NFC,
            "failed",
            "2026-02-01T00:00:00+00:00",
            0,
        );
        drop(conn);
        let db = reopen(&path);
        assert_eq!(db.list_recent_jobs(10).unwrap().len(), 1);
        assert_eq!(db.requeue_failed_jobs().expect("Retry must not collide"), 1);
        let jobs = db.list_recent_jobs(10).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, 2, "the newest survives");
        assert!(jobs[0].delta_deferred);
        assert_eq!(jobs[0].target_path.as_deref(), Some(NFC));
    }

    /// Round-4 M2. A CANONICAL failed job next to a re-keyed ACTIVE twin: the single grouping
    /// rule collapses them (the active job wins and, being a `local_delete` group with a
    /// re-keyed member, is deferred), so Retry neither collides nor revives anything.
    #[test]
    fn a_canonical_failed_job_next_to_a_rekeyed_active_twin_is_collapsed() {
        let (path, conn) = legacy_db();
        plant_job(
            &conn,
            "local_delete",
            NFC,
            "failed",
            "2026-03-01T00:00:00+00:00",
            0,
        );
        plant_job(
            &conn,
            "local_delete",
            NFD,
            "queued",
            "2026-01-01T00:00:00+00:00",
            0,
        );
        drop(conn);
        let db = reopen(&path);
        assert_eq!(db.requeue_failed_jobs().expect("Retry must not collide"), 0);
        let jobs = db.list_recent_jobs(10).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(
            jobs[0].id, 2,
            "the active job wins even though the failed one is newer"
        );
        assert!(jobs[0].delta_deferred);
        assert_eq!(db.latest_failed_error().unwrap(), None);
    }

    /// Round-4 L1. A failed upload shadowed by an active one still hands its source-deletion
    /// debt to the winner.
    #[test]
    fn a_shadowed_failed_upload_hands_its_debt_to_the_active_winner() {
        let (path, conn) = legacy_db();
        conn.execute_batch(&format!(
            "INSERT INTO sync_jobs(job_type, source_path, target_path, on_success_delete_path, on_success_delete_rev, status, created_at, updated_at) \
               VALUES('upload', '{NFD}', '{NFD}', 'old.txt', 'rev-old', 'failed', 'c', '2026-03-01T00:00:00+00:00');
             INSERT INTO sync_jobs(job_type, source_path, target_path, status, created_at, updated_at) \
               VALUES('upload', '{NFC}', '{NFC}', 'queued', 'c', '2026-01-01T00:00:00+00:00');"
        ))
        .unwrap();
        drop(conn);
        let db = reopen(&path);
        assert_eq!(db.list_recent_jobs(10).unwrap().len(), 1);
        assert_eq!(
            db.peek_deferred_source_delete(2).unwrap(),
            Some(("old.txt".to_string(), Some("rev-old".to_string())))
        );
    }

    /// M2. The dropped upload carries a source-deletion debt the winner lacks: the winner
    /// inherits path and rev (the `COALESCE` rule of `enqueue_upload_then_delete`).
    #[test]
    fn a_dropped_upload_hands_its_source_deletion_debt_to_the_winner() {
        let (path, conn) = legacy_db();
        conn.execute_batch(&format!(
            "INSERT INTO sync_jobs(job_type, source_path, target_path, on_success_delete_path, on_success_delete_rev, status, created_at, updated_at) \
               VALUES('upload', '{NFD}', '{NFD}', 'old-name.txt', 'rev-old', 'retry_wait', 'c', '2026-01-01T00:00:00+00:00');
             INSERT INTO sync_jobs(job_type, source_path, target_path, status, created_at, updated_at) \
               VALUES('upload', '{NFC}', '{NFC}', 'queued', 'c', '2026-02-01T00:00:00+00:00');"
        ))
        .unwrap();
        drop(conn);
        let db = reopen(&path);
        let jobs = db.list_recent_jobs(10).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, 2);
        assert_eq!(
            db.peek_deferred_source_delete(2).unwrap(),
            Some(("old-name.txt".to_string(), Some("rev-old".to_string())))
        );
    }

    /// M2, the case a merge cannot settle: two different debts. The loser is kept as a
    /// finished row so `unsettled_source_deletion` can report it; the winner keeps its own.
    #[test]
    fn a_dropped_upload_owing_a_different_deletion_is_kept_not_deleted() {
        let (path, conn) = legacy_db();
        conn.execute(
            "INSERT INTO remote_file_index(relative_path, content_hash, rev, modified_ts, updated_at) \
             VALUES('a.txt', 'h', 'rev-a', 0, 'x')",
            [],
        )
        .unwrap();
        conn.execute_batch(&format!(
            "INSERT INTO sync_jobs(job_type, source_path, target_path, on_success_delete_path, on_success_delete_rev, status, created_at, updated_at) \
               VALUES('upload', '{NFD}', '{NFD}', 'a.txt', 'rev-a', 'retry_wait', 'c', '2026-01-01T00:00:00+00:00');
             INSERT INTO sync_jobs(job_type, source_path, target_path, on_success_delete_path, on_success_delete_rev, status, created_at, updated_at) \
               VALUES('upload', '{NFC}', '{NFC}', 'b.txt', 'rev-b', 'queued', 'c', '2026-02-01T00:00:00+00:00');"
        ))
        .unwrap();
        drop(conn);
        let db = reopen(&path);
        let conn = Connection::open(&path).unwrap();
        assert_eq!(
            col_strings(
                &conn,
                "SELECT id||':'||status||':'||on_success_delete_path||':'||on_success_delete_rev||':'||target_path \
                 FROM sync_jobs ORDER BY id"
            ),
            vec![
                format!("1:done:a.txt:rev-a:{NFC}"),
                format!("2:queued:b.txt:rev-b:{NFC}")
            ]
        );
        assert!(
            col_strings(&conn, "SELECT last_error FROM sync_jobs WHERE id = 1")[0]
                .contains("NOT performed")
        );
        assert_eq!(
            db.unsettled_source_deletion().unwrap(),
            Some(("a.txt".to_string(), NFC.to_string())),
            "the kept loser's debt must be reported to the user"
        );
        assert_eq!(
            db.peek_deferred_source_delete(2).unwrap(),
            Some(("b.txt".to_string(), Some("rev-b".to_string())))
        );
    }

    /// LOW 1. A success clears the marker, so a later downgrade and re-upgrade (new NFD
    /// rows) writes a NEW backup that holds those rows.
    #[test]
    fn a_later_migration_after_a_success_writes_a_new_backup_with_the_new_rows() {
        let (path, conn) = legacy_db();
        conn.execute(
            "INSERT INTO known_folders(relative_path, updated_at) VALUES(?1, 'x')",
            [NFD],
        )
        .unwrap();
        drop(conn);
        drop(reopen(&path));
        assert_eq!(backups(&path).len(), 1);

        let second = format!("{NFD}-later");
        Connection::open(&path)
            .unwrap()
            .execute(
                "INSERT INTO known_folders(relative_path, updated_at) VALUES(?1, 'x')",
                [&second],
            )
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        drop(reopen(&path));
        let all = backups(&path);
        assert_eq!(all.len(), 2, "the second migration needs its own backup");
        let holds_new_row = all.iter().any(|b| {
            let c = Connection::open(b).unwrap();
            col_strings(&c, "SELECT relative_path FROM known_folders").contains(&second)
        });
        assert!(
            holds_new_row,
            "no backup holds the rows the second run changed"
        );
    }

    /// Round-4 M1. A failed start, then an older build (which never clears
    /// `startup-error.txt`) writes more legacy rows, then the re-upgrade: the backup must hold
    /// the later rows, and the failed attempt's backup is replaced, not accumulated.
    #[test]
    fn a_failure_then_downgrade_written_rows_then_reupgrade_ends_with_one_backup_holding_them() {
        let (path, conn) = legacy_db();
        conn.execute_batch(&format!(
            "INSERT INTO known_folders(relative_path, updated_at) VALUES('{NFD}', 'x');
             CREATE TRIGGER boom BEFORE UPDATE ON known_folders BEGIN SELECT RAISE(ABORT, 'boom'); END;"
        ))
        .unwrap();
        drop(conn);
        Db::new_at(&path).err().expect("first launch fails");
        assert_eq!(backups(&path).len(), 1);
        assert!(crate::storage::db::startup_error_path(&path).exists());

        // The old build writes a new legacy row and leaves the error file in place.
        let later = format!("{NFD}-later");
        let c = Connection::open(&path).unwrap();
        c.execute_batch("DROP TRIGGER boom").unwrap();
        c.execute(
            "INSERT INTO known_folders(relative_path, updated_at) VALUES(?1, 'x')",
            [&later],
        )
        .unwrap();
        drop(c);
        std::thread::sleep(std::time::Duration::from_millis(5));
        drop(reopen(&path));

        let all = backups(&path);
        assert_eq!(
            all.len(),
            1,
            "the failed attempt's backup is replaced: {all:?}"
        );
        let bk = Connection::open(&all[0]).unwrap();
        assert!(
            col_strings(&bk, "SELECT relative_path FROM known_folders").contains(&later),
            "the surviving backup lacks the rows written after the failure"
        );
    }

    /// Only an exact file-name match is ever deleted: an error file that names something
    /// merely containing a backup's name, or a different file, removes nothing.
    #[test]
    fn only_an_exact_backup_name_in_the_error_file_is_deleted() {
        let (path, conn) = legacy_db();
        conn.execute(
            "INSERT INTO known_folders(relative_path, updated_at) VALUES(?1, 'x')",
            [NFD],
        )
        .unwrap();
        drop(conn);
        drop(reopen(&path));
        let old = backups(&path).remove(0);
        let old_name = old.file_name().unwrap().to_string_lossy().into_owned();
        let bystander = path.with_file_name("notes.txt");
        std::fs::write(&bystander, "keep").unwrap();

        for named in [format!("{old_name}.keep"), "notes.txt".to_string()] {
            Connection::open(&path)
                .unwrap()
                .execute(
                    "INSERT OR IGNORE INTO known_folders(relative_path, updated_at) VALUES(?1, 'x')",
                    [NFD],
                )
                .unwrap();
            std::fs::write(
                crate::storage::db::startup_error_path(&path),
                format!("t\nfailed (pre-migration backup: {named})\n"),
            )
            .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(5));
            migrate(&path).unwrap();
            assert!(old.exists(), "{named:?} must not delete {old_name}");
            assert!(bystander.exists());
        }
    }

    /// Round-4 L2. Another connection holds the write lock; the migration must queue on
    /// `busy_timeout` and then succeed. A deferred transaction fails at once instead.
    #[test]
    fn the_migration_waits_for_a_held_write_lock_and_then_succeeds() {
        let (path, conn) = legacy_db();
        conn.execute(
            "INSERT INTO known_folders(relative_path, updated_at) VALUES(?1, 'x')",
            [NFD],
        )
        .unwrap();
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        let p = path.clone();
        let started = std::time::Instant::now();
        let t = std::thread::spawn(move || {
            let mut c = Connection::open(&p).unwrap();
            c.busy_timeout(std::time::Duration::from_secs(10)).unwrap();
            rekey_to_canonical_form(&mut c, &p)
        });
        std::thread::sleep(std::time::Duration::from_millis(400));
        assert!(
            !t.is_finished(),
            "must be waiting for the lock, not finished"
        );
        conn.execute_batch("COMMIT").unwrap();
        let report = t.join().unwrap().expect("must wait, then migrate");
        assert!(started.elapsed() >= std::time::Duration::from_millis(400));
        assert_eq!(report.rows_rekeyed, 1);
        assert_eq!(
            col_strings(&conn, "SELECT relative_path FROM known_folders"),
            vec![NFC.to_string()]
        );
    }

    /// Round-4 L2: the other instance migrated while this one waited. The re-count under the
    /// lock finds nothing, changes nothing, and removes the backup this call had written.
    #[test]
    fn if_another_instance_migrated_first_nothing_changes_and_no_extra_backup_is_left() {
        let (path, conn) = legacy_db();
        conn.execute(
            "INSERT INTO known_folders(relative_path, updated_at) VALUES(?1, 'x')",
            [NFD],
        )
        .unwrap();
        let mut c = Connection::open(&path).unwrap();
        // The "other instance" migrates first.
        let report = rekey_with(&mut c, &path, &mut |_, _| Ok(())).unwrap();
        assert_eq!(report.rows_rekeyed, 1);
        let kept = backups(&path);
        assert_eq!(kept.len(), 1);

        // This call counted legacy rows before the other one migrated; it now reaches the
        // lock with a backup already written.
        let mut c2 = Connection::open(&path).unwrap();
        let fake = backup_path(&path);
        std::fs::write(&fake, "x").unwrap();
        let r = rekey_in_transaction(&mut c2, fake.clone(), &mut |_, _| Ok(())).unwrap();
        assert_eq!(r, RekeyReport::default());
        assert!(!fake.exists(), "the unneeded backup must be removed");
        assert_eq!(backups(&path), kept);
    }

    /// L4. Two launches in the same millisecond must not share a backup name.
    #[test]
    fn a_backup_name_is_never_reused_within_one_millisecond() {
        let path = fresh_path();
        let first = backup_path_at(&path, "20260101T000000000Z");
        assert!(first
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains(&format!("-{}.bak", std::process::id())));
        std::fs::write(&first, "x").unwrap();
        let second = backup_path_at(&path, "20260101T000000000Z");
        assert_ne!(first, second);
        std::fs::write(&second, "y").unwrap();
        let third = backup_path_at(&path, "20260101T000000000Z");
        assert!(third != first && third != second);
        assert!(
            third
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with(".bak"),
            "{third:?} must still match the backup pattern"
        );
    }

    /// L3. A `VACUUM INTO` that fails after creating a file must not leave it behind, but a
    /// file that was already there is not ours to delete.
    #[test]
    fn a_failed_backup_write_removes_its_partial_file_and_only_its_own() {
        let (path, conn) = legacy_db();
        let partial = path.with_file_name("app.db.pre-nfc-rekey-partial.bak");
        let err = write_backup(
            &path,
            &partial,
            |_, to| {
                std::fs::write(to, "half").unwrap();
                Err(rusqlite::Error::InvalidQuery)
            },
            &conn,
        )
        .unwrap_err();
        assert!(err.to_string().contains("could not write"), "{err}");
        assert!(!partial.exists(), "the partial backup was left behind");

        let theirs = path.with_file_name("app.db.pre-nfc-rekey-theirs.bak");
        std::fs::write(&theirs, "other launch").unwrap();
        write_backup(
            &path,
            &theirs,
            |_, _| Err(rusqlite::Error::InvalidQuery),
            &conn,
        )
        .unwrap_err();
        assert!(theirs.exists(), "a pre-existing file must not be deleted");
    }

    /// L2. The previous attempt's backup is deleted only after the new one is durable: when
    /// the new one cannot be synced (here: it was never created), the old one survives.
    #[test]
    fn the_previous_backup_survives_until_the_new_one_is_durable() {
        let (path, conn) = legacy_db();
        let old = path.with_file_name("app.db.pre-nfc-rekey-old.bak");
        std::fs::write(&old, "old").unwrap();
        std::fs::write(
            crate::storage::db::startup_error_path(&path),
            "t\nfailed (pre-migration backup: app.db.pre-nfc-rekey-old.bak)\n",
        )
        .unwrap();
        let fresh = path.with_file_name("app.db.pre-nfc-rekey-fresh.bak");
        write_backup(&path, &fresh, |_, _| Ok(()), &conn).unwrap_err();
        assert!(
            old.exists(),
            "the old backup was deleted before the new one was durable"
        );

        write_backup(
            &path,
            &fresh,
            |c, to| c.execute("VACUUM INTO ?1", [to]).map(|_| ()),
            &conn,
        )
        .unwrap();
        assert!(fresh.exists());
        assert!(!old.exists(), "once durable, the old backup is replaced");
    }

    /// L5. Counters: an inherited debt is not a re-keyed row; a group winner promoted to
    /// deferred counts once, whether or not it was itself re-keyed.
    #[test]
    fn the_report_counts_inherited_debts_and_promoted_winners_correctly() {
        // Inherited debt only: the winner is canonical, so nothing was re-keyed on it.
        let (path, conn) = legacy_db();
        conn.execute_batch(&format!(
            "INSERT INTO sync_jobs(job_type, source_path, target_path, on_success_delete_path, on_success_delete_rev, status, created_at, updated_at) \
               VALUES('upload', '{NFD}', '{NFD}', 'old.txt', 'r', 'retry_wait', 'c', '2026-01-01T00:00:00+00:00');
             INSERT INTO sync_jobs(job_type, source_path, target_path, status, created_at, updated_at) \
               VALUES('upload', '{NFC}', '{NFC}', 'queued', 'c', '2026-02-01T00:00:00+00:00');"
        ))
        .unwrap();
        drop(conn);
        let r = migrate(&path).unwrap();
        assert_eq!((r.jobs_dropped, r.rows_rekeyed), (1, 0), "{r:?}");

        // Canonical winner promoted because the dropped twin was a re-keyed local_delete.
        let (path, conn) = legacy_db();
        plant_job(
            &conn,
            "local_delete",
            NFD,
            "failed",
            "2026-01-01T00:00:00+00:00",
            0,
        );
        plant_job(
            &conn,
            "local_delete",
            NFC,
            "queued",
            "2026-02-01T00:00:00+00:00",
            0,
        );
        drop(conn);
        let r = migrate(&path).unwrap();
        assert_eq!(r.local_deletes_deferred, 1, "{r:?}");

        // Re-keyed winner promoted: counted once, not twice.
        let (path, conn) = legacy_db();
        plant_job(
            &conn,
            "local_delete",
            NFD,
            "queued",
            "2026-02-01T00:00:00+00:00",
            0,
        );
        plant_job(
            &conn,
            "local_delete",
            NFC,
            "failed",
            "2026-01-01T00:00:00+00:00",
            0,
        );
        drop(conn);
        let r = migrate(&path).unwrap();
        assert_eq!(r.local_deletes_deferred, 1, "{r:?}");
    }

    /// LOW 2. A failure BEFORE the migration (here the schema step: the file is not a
    /// database) is recorded too, and a full success clears the file.
    #[test]
    fn an_early_startup_failure_is_recorded_and_a_full_success_clears_it() {
        let path = fresh_path();
        std::fs::write(&path, vec![b'x'; 4096]).unwrap();
        let error_file = crate::storage::db::startup_error_path(&path);
        let err = Db::new_at(&path).err().expect("not a database");
        assert!(err.to_string().contains("startup-error.txt"), "{err}");
        let written = std::fs::read_to_string(&error_file).expect("early failure recorded");
        assert!(written.contains("not a database"), "{written}");

        std::fs::remove_file(&path).unwrap();
        drop(Db::new_at(&path).expect("clean start"));
        assert!(!error_file.exists(), "a full success clears the file");
    }

    /// LOW 2. When the file cannot be written the message carries the reason itself and
    /// does not point at a file that is not there.
    #[test]
    fn an_unwritable_startup_error_file_puts_the_reason_in_the_message() {
        let path = fresh_path()
            .parent()
            .unwrap()
            .join("missing-dir")
            .join("app.db");
        let err = Db::new_at(&path).err().expect("cannot open").to_string();
        assert!(!err.contains("details also in"), "{err}");
        assert!(err.contains("could not write"), "{err}");
        assert!(err.contains("unable to open"), "{err}");
    }

    /// LOW 3. Same `updated_at` and `modified_ts`: the row already in canonical form wins
    /// (the smaller key bytes would pick the NFD one, so only the tiebreak decides this).
    #[test]
    fn on_a_complete_tie_the_already_canonical_row_wins() {
        let (path, conn) = legacy_db();
        plant_local_pair(
            &conn,
            "2026-01-01T00:00:00+00:00",
            "2026-01-01T00:00:00+00:00",
        );
        drop(conn);
        drop(reopen(&path));
        let conn = Connection::open(&path).unwrap();
        assert_eq!(
            col_strings(&conn, "SELECT hash FROM local_file_index"),
            vec!["nfc-hash".to_string()]
        );
    }

    #[test]
    fn conflicts_are_rekeyed_in_place_and_none_is_dropped() {
        let (path, conn) = legacy_db();
        conn.execute_batch(&format!(
            "INSERT INTO sync_conflicts(local_path, remote_path, reason, created_at) VALUES('{NFD}', '{NFD}', 'a', 'c');
             INSERT INTO sync_conflicts(local_path, remote_path, reason, created_at) VALUES('{NFC}', '{NFC}', 'b', 'c');"
        ))
        .unwrap();
        let report = migrate(&path).unwrap();
        assert_eq!(report.collisions_dropped, 0);
        let conn = Connection::open(&path).unwrap();
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM sync_conflicts"), 2);
        assert_eq!(count_non_canonical(&conn).unwrap(), 0);
    }

    #[test]
    fn refused_moves_collision_keeps_the_newest_refusal() {
        let (path, conn) = legacy_db();
        conn.execute_batch(&format!(
            "INSERT INTO refused_moves VALUES('{NFD}', 'dest', '2026-01-01T00:00:00+00:00');
             INSERT INTO refused_moves VALUES('{NFC}', 'dest', '2026-02-01T00:00:00+00:00');"
        ))
        .unwrap();
        let report = migrate(&path).unwrap();
        assert_eq!(report.collisions_dropped, 1);
        let conn = Connection::open(&path).unwrap();
        assert_eq!(
            col_strings(
                &conn,
                "SELECT from_path||'|'||refused_at FROM refused_moves"
            ),
            vec![format!("{NFC}|2026-02-01T00:00:00+00:00")]
        );
    }

    /// Fail closed, at the migration level: an error after earlier tables were already
    /// rewritten rolls ALL of them back.
    #[test]
    fn a_failure_midway_rolls_back_every_table() {
        let (path, conn) = legacy_db();
        conn.execute_batch(&format!(
            "INSERT INTO known_folders(relative_path, updated_at) VALUES('{NFD}', 'x');
             INSERT INTO sync_jobs(job_type, source_path, target_path, status, created_at, updated_at) VALUES('upload', '{NFD}', '{NFD}', 'queued', 'c', 'u');"
        ))
        .unwrap();
        let mut c = Connection::open(&path).unwrap();
        let err = rekey_with(&mut c, &path, &mut |step, _| {
            if step == "sync_jobs" {
                Err(AppError::Storage("injected".into()))
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert!(err.to_string().contains("injected"));
        drop(c);
        let conn = Connection::open(&path).unwrap();
        assert_eq!(
            col_strings(&conn, "SELECT relative_path FROM known_folders"),
            vec![NFD.to_string()],
            "known_folders was rewritten before the failure and must be rolled back"
        );
        assert_eq!(
            col_strings(&conn, "SELECT target_path FROM sync_jobs"),
            vec![NFD.to_string()]
        );
    }

    /// The final verification is its own guard: if any step ever leaves a non-canonical
    /// value behind, the transaction must roll back instead of committing a half-migrated
    /// database. A stray NFD value is planted after the last step to prove it.
    #[test]
    fn a_surviving_non_canonical_value_rolls_everything_back() {
        let (path, conn) = legacy_db();
        conn.execute(
            "INSERT INTO known_folders(relative_path, updated_at) VALUES(?1, 'x')",
            [NFD],
        )
        .unwrap();
        let mut c = Connection::open(&path).unwrap();
        let err = rekey_with(&mut c, &path, &mut |step, tx| {
            if step == "refused_moves" {
                tx.execute("INSERT INTO refused_moves VALUES(?1, 'to', 't')", [NFD])?;
            }
            Ok(())
        })
        .unwrap_err();
        assert!(err.to_string().contains("non-canonical"), "{err}");
        drop(c);
        let conn = Connection::open(&path).unwrap();
        assert_eq!(
            col_strings(&conn, "SELECT relative_path FROM known_folders"),
            vec![NFD.to_string()],
            "the folder re-key must be rolled back with the rest"
        );
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM refused_moves"), 0);
    }

    /// Fail closed, at the startup level: a real SQLite failure inside the migration (a
    /// trigger that aborts any `sync_jobs` UPDATE) makes `Db::new_at` return an error, and
    /// that error is what `lib.rs::run` `expect`s before any loop exists. The original rows
    /// are intact.
    #[test]
    fn db_new_at_fails_and_leaves_the_rows_untouched_when_the_migration_fails() {
        let (path, conn) = legacy_db();
        conn.execute_batch(&format!(
            "INSERT INTO known_folders(relative_path, updated_at) VALUES('{NFD}', 'x');
             INSERT INTO sync_jobs(job_type, source_path, target_path, status, created_at, updated_at) VALUES('upload', '{NFD}', '{NFD}', 'queued', 'c', 'u');
             CREATE TRIGGER boom BEFORE UPDATE ON sync_jobs BEGIN SELECT RAISE(ABORT, 'boom'); END;"
        ))
        .unwrap();
        drop(conn);
        let err = Db::new_at(&path).err().expect("startup must fail closed");
        assert!(err.to_string().contains("boom"), "{err}");
        let conn = Connection::open(&path).unwrap();
        assert_eq!(
            col_strings(&conn, "SELECT relative_path FROM known_folders"),
            vec![NFD.to_string()]
        );
        assert_eq!(count_non_canonical(&conn).unwrap(), 3);
    }

    /// M3. A migration that keeps failing must not add a full copy of the database to the
    /// data directory on every launch, and must leave its reason where a panic cannot eat it.
    #[test]
    fn a_persistently_failing_migration_keeps_one_backup_and_writes_its_reason_to_disk() {
        let (path, conn) = legacy_db();
        conn.execute_batch(&format!(
            "INSERT INTO known_folders(relative_path, updated_at) VALUES('{NFD}', 'x');
             CREATE TRIGGER boom BEFORE UPDATE ON known_folders BEGIN SELECT RAISE(ABORT, 'boom'); END;"
        ))
        .unwrap();
        drop(conn);
        let error_file = crate::storage::db::startup_error_path(&path);

        let first = Db::new_at(&path).err().expect("first launch fails");
        assert!(first.to_string().contains("startup-error.txt"), "{first}");
        assert_eq!(backups(&path).len(), 1);
        let written = std::fs::read_to_string(&error_file).expect("reason written before Err");
        assert!(written.contains("boom"), "{written}");
        // First line is an RFC 3339 timestamp.
        let stamp = written.lines().next().unwrap();
        assert!(DateTime::parse_from_rfc3339(stamp).is_ok(), "{stamp:?}");

        // The backup stamp has millisecond resolution; make sure a second one WOULD differ.
        std::thread::sleep(std::time::Duration::from_millis(5));
        Db::new_at(&path).err().expect("second launch fails");
        assert_eq!(
            backups(&path).len(),
            1,
            "no second backup per failing launch"
        );
        // The kept backup is the pre-migration state.
        let bk = Connection::open(&backups(&path)[0]).unwrap();
        assert_eq!(
            col_strings(&bk, "SELECT relative_path FROM known_folders"),
            vec![NFD.to_string()]
        );

        // Once the cause is fixed the launch succeeds, still without another backup, and the
        // stale reason is removed.
        Connection::open(&path)
            .unwrap()
            .execute_batch("DROP TRIGGER boom")
            .unwrap();
        let db = reopen(&path);
        assert_eq!(db.list_known_folders().unwrap(), vec![NFC.to_string()]);
        assert_eq!(backups(&path).len(), 1);
        assert!(!error_file.exists(), "a successful start clears the reason");
    }

    /// The startup path itself: `Db::new_at` hands back a handle only after the re-key has
    /// run, so every reader sees canonical rows from its first call, and a restart is a
    /// no-op that writes no second backup.
    #[test]
    fn db_new_at_rekeys_before_returning_and_a_restart_is_a_no_op() {
        let (path, conn) = legacy_db();
        conn.execute(
            "INSERT INTO known_folders(relative_path, updated_at) VALUES(?1, 'x')",
            [NFD],
        )
        .unwrap();
        drop(conn);

        let db = Db::new_at(&path).expect("startup migrates");
        // The asserting reader accepts the row: it is already NFC.
        assert_eq!(db.list_known_folders().unwrap(), vec![NFC.to_string()]);
        assert_eq!(backups(&path).len(), 1);
        drop(db);

        drop(Db::new_at(&path).expect("restart"));
        assert_eq!(
            backups(&path).len(),
            1,
            "a restart must not write another backup"
        );
    }

    /// Dry run for the maintainer: `DBSYNC103_DRY_RUN_DB=/path/to/COPY.db cargo test
    /// --manifest-path apps/desktop/src-tauri/Cargo.toml dry_run_on_a_copy -- --ignored
    /// --nocapture`. Refuses to touch the real database.
    #[test]
    #[ignore = "manual: set DBSYNC103_DRY_RUN_DB to a COPY of a real database"]
    fn dry_run_on_a_copy_of_a_real_database() {
        let p = PathBuf::from(
            std::env::var("DBSYNC103_DRY_RUN_DB").expect("set DBSYNC103_DRY_RUN_DB to a copy"),
        );
        assert!(p.is_file(), "{p:?} is not a file");
        if let Ok(real) = crate::storage::db::db_path() {
            assert_ne!(
                std::fs::canonicalize(&p).unwrap(),
                std::fs::canonicalize(real).unwrap_or_default(),
                "refusing to run on the real database; copy it first"
            );
        }
        let mut conn = Connection::open(&p).unwrap();
        crate::storage::db::migrate(&mut conn).expect("schema migrate");
        let before = count_non_canonical(&conn).unwrap();
        let report = rekey_to_canonical_form(&mut conn, &p).expect("re-key");
        let after = count_non_canonical(&conn).unwrap();
        println!("non-canonical values before: {before}, after: {after}\n{report:#?}");
        assert_eq!(after, 0);
    }
}
