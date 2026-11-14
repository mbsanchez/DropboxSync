//! The File Provider projection of the remote tree (DBSYNC-95 slice 2a, ADR-0004). macOS only.
//!
//! The host is the single Dropbox client and the only writer of these tables; the File Provider
//! extension (slice 2b) reads them through a read-only connection and never talks to Dropbox. Three
//! tables, all additive (`migrate` runs inside the transactional `Db` migration):
//!
//! * `fp_identity`: the opaque `item_id` the system sees, minted from the same `item_id_seq` as
//!   `local_file_index.item_id` so the two can never collide, plus the Dropbox id it stands for. Rows
//!   are never deleted, so a revival, a reseed or a reset hands the same item the same identifier.
//! * `fp_item`: one row per item, with the parent as a stored id (0 is the domain root), the
//!   `path_lower` index that deletes are resolved through, both versions and the `changed_seq` that
//!   pages the working-set changes.
//! * `fp_state`: a single row with the anchor ingredients: `generation`, `seq`, `min_valid_seq` and
//!   `populated`.
//!
//! ## Where the rules come from, and how far they are checked
//!
//! The rules below are Dropbox's classic `/delta` contract: entries come in order, a `deleted` entry
//! names a path and nothing else, and a file or folder entry replaces what is at its path. That text is
//! NOT in the current v2 documentation we could read (the "detecting changes" page does not state it).
//! It was checked against the live capture of #234 (2a.0), and only for the shapes captured: a nested
//! create, a folder rename, a file move, an empty folder delete and the delete of a folder with
//! content, and later against the rename of a folder of 204 entries (#234, 2026-11-06).
//!
//! What the capture showed:
//!
//! 1. A `deleted` entry carries a path and nothing else, so a delete is resolved through the item's
//!    CURRENT `path_lower`, never through an id. Upserts are keyed by `dropbox_id`.
//! 2. A folder rename arrives as deletes of the old paths, then the folder at its new path with the
//!    SAME id, then every descendant with its same id. Nothing promises one batch: a small rename came
//!    in one invocation, the rename of 204 entries came in two (every delete, `has_more=false`, and 2.9 s
//!    later every re-add). DBSYNC-102 had already measured the same split on a share and an un-share.
//! 3. A move changes `rev` while the content stays. The content version is therefore derived from
//!    `content_hash` alone.
//! 4. A folder delete lists its children's deletes, but nothing proves that holds for a large or deep
//!    tree, so a delete also takes everything beneath its path.
//!
//! ## The model: each item changes only through its own entry
//!
//! The entries of a delta, across pages, in the order `run_remote_delta` accumulates them, are applied
//! one after the other inside the one IMMEDIATE transaction of the writer: the state after entry N is
//! the state entry N+1 starts from. There is no batch-level resolution, no carry of a moved folder's
//! descendants and no revival of anything: a row changes only through an entry that names it (its own
//! upsert, a `deleted` on its path or a parent's path, or the replacement of whatever stands at its
//! path). "At path P" is the folded path ([`fold`]); the root is skipped.
//!
//! * **`deleted P`.** The non-deleted row at `P` and every non-deleted row under `P/` become
//!   tombstones.
//! * **`file|folder P` with id `I`.**
//!   1. *A different non-deleted item `R` at `P`* is replaced. If the newcomer is a FILE, `R` is
//!      tombstoned, and so is its whole subtree when `R` is a folder (a file has no children). If the
//!      newcomer is a FOLDER and `R` is a file, `R` is tombstoned. If both are folders only `R` is
//!      tombstoned and its non-deleted children are re-parented to `I` (a parent change, stamped if
//!      observable); the deeper levels keep their own parents. A newcomer that is itself an orphan
//!      hands its children over as orphans.
//!   2. *An id the projection knows* is updated in place, in whatever state it is (live, hidden,
//!      orphan or deleted): path, name, parent, metadata, versions and state. Nothing else moves: its
//!      children keep their parent id and their paths until their own entries arrive.
//!   3. *An unknown id* is given an identity from `item_id_seq` and inserted.
//! * **End of batch.** Orphans whose parent folder now exists are healed, following selective sync, and
//!   rows left under a folder that became an orphan are demoted with it; both repeat until nothing
//!   moves, so a chain of orphans heals (or goes down) in one batch. A heal never touches a tombstone.
//! * **Stamps.** A new row is stamped as it is inserted (one write). A row that existed takes one
//!   sequence value after the batch if its final state differs from the one it began with, and none if
//!   it does not. So a row that moves around inside a batch and ends where it began costs nothing,
//!   and applying the same batch a second time (the cursor did not persist) ends in the same state
//!   and bumps nothing, so it signals nothing. A row an entry restates writes nothing at all.
//!
//! ### The two orders of a rename, with children that are not re-listed
//!
//! * **Deletes first** (what the capture showed): `deleted /a` tombstones the subtree, the re-add of
//!   the folder restores it by its id, and a child that is not re-listed stays deleted.
//! * **Deletes last**: the folder and the re-listed children move through their own upserts and keep
//!   their ids. A child that is not re-listed keeps its parent id and its old path, so the trailing
//!   `deleted /a` sweeps whatever still sits at the old paths; anything re-listed has already moved.
//!
//! ### Orphans: the rule for the extension (2b)
//!
//! A folder at state ORPHAN is NOT a parent. A child of an orphan folder is itself an orphan, and a row
//! that hangs under a folder that became an orphan is demoted with it, so a LIVE or HIDDEN row never
//! has an ORPHAN parent: following `parent_item` up from a listed row never meets a row that must not
//! be shown. An orphan has `parent_item = -1` and is never listed. (The one thing not chased: a row
//! left at a stale path under a folder that a later entry tombstones. It changes only through its own
//! entry, like every row.)
//!
//! ### A rename split across two invocations
//!
//! Both halves are ordinary batches. The first tombstones the rows and the second finds each id and
//! restores it in place, so identities survive the gap. Between them the projection says those rows are
//! deleted, and it is right to: nothing in the first half announces the second. What is held back is
//! the signal, not the data ([`ProjectionCommit::vanished`], `file_provider::SIGNAL_REMOVAL_GRACE`).
//!
//! ## The snapshot
//!
//! [`Db::project_snapshot`] is total and infers nothing. Every listed entry is placed by its id alone
//! (no displacement), every id that was not listed is swept, and only then are the parent and orphan
//! state of every remaining row resolved again, because a parent found while placing may be a row the
//! sweep removed. The result does not depend on the order of the listing. After a snapshot no listed
//! id is a tombstone (checked, and an error if not).
//!
//! * **Paths are folded.** `path_lower` is stored and queried as [`crate::path_util::fold`] of what
//!   Dropbox sent (NFC, then lowercase), the one comparison form for remote-vs-remote matches
//!   (DBSYNC-103). An NFD delete finds the NFC row it names.
//!
//! ## The change sequence
//!
//! `fp_state.seq` is global and monotonic. Every observable row change takes the next value through
//! [`Stamper::stamp`], the only place it moves, so paging by `changed_seq` has no ties. Writing the same
//! data again changes nothing and stamps nothing, which is what makes a replayed delta harmless.
//! Anchors expire in exactly three places: a snapshot ([`Db::project_snapshot`], which runs when the
//! Dropbox cursor is empty or was reset), a clear ([`clear_in`]) and the tombstone cap
//! ([`prune_tombstones`], which only ever raises `min_valid_seq` to the newest tombstone it dropped).
//! The first two move `min_valid_seq` up to the new `seq`. `generation` is minted once, at first
//! migration, so a database that is deleted and recreated expires every anchor an earlier one handed out.
//!
//! ## Rules this module holds to
//!
//! * Nothing is written unless the switch is on ([`Db::projection_active`]), except the clear, which
//!   is a no-op on an empty projection. A delta, a filter reapply and a download flag also need the
//!   projection to be populated: a delta must never patch a base it does not have.
//! * Every writer is one transaction.
//! * Nothing here logs a path or a name. Counts only.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime};

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use sha2::{Digest, Sha256};

use super::Db;
use crate::error::{AppError, AppResult};
use crate::models::DropboxEntry;
use crate::path_util::{fold, parse_prefix_csv, PathScope, PrefixFilter};

/// Visible in the domain.
pub(crate) const STATE_LIVE: i64 = 0;
/// Present in Dropbox but excluded by selective sync (DBSYNC-114): stored, reported as deleted.
pub(crate) const STATE_HIDDEN: i64 = 1;
/// A tombstone: kept until the next snapshot so a missed change is always reportable.
pub(crate) const STATE_DELETED: i64 = 2;
/// Its parent folder is not known (yet): never listed, healed when the parent arrives.
pub(crate) const STATE_ORPHAN: i64 = 3;

/// `fp_item.parent_item` of an item directly under the domain root.
pub(crate) const ROOT_ITEM: i64 = 0;
/// `fp_item.parent_item` of an orphan.
const NO_PARENT: i64 = -1;

/// Bumped only by a change the extension has to refuse to read past.
const SCHEMA_VERSION: i64 = 1;

/// The tables and their indexes. `Tests/projection-schema.sql` in the File Provider extension
/// is the copy the extension's tests build their database from; a test below fails when the two
/// differ, so neither can move alone.
const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS fp_identity (
    item_id INTEGER PRIMARY KEY,
    dropbox_id TEXT,
    created_at TEXT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_fp_identity_dropbox_id
    ON fp_identity(dropbox_id) WHERE dropbox_id IS NOT NULL;
CREATE TABLE IF NOT EXISTS fp_item (
    item_id INTEGER PRIMARY KEY REFERENCES fp_identity(item_id),
    parent_item INTEGER NOT NULL,
    path_lower TEXT NOT NULL,
    name TEXT NOT NULL,
    is_folder INTEGER NOT NULL CHECK (is_folder IN (0, 1)),
    size_bytes INTEGER NOT NULL,
    modified_ts INTEGER NOT NULL,
    content_hash TEXT NOT NULL,
    rev TEXT NOT NULL,
    content_version BLOB NOT NULL,
    meta_version BLOB NOT NULL,
    downloaded INTEGER NOT NULL DEFAULT 0 CHECK (downloaded IN (0, 1)),
    state INTEGER NOT NULL CHECK (state IN (0, 1, 2, 3)),
    changed_seq INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_fp_item_parent ON fp_item(parent_item, state);
CREATE INDEX IF NOT EXISTS idx_fp_item_seq ON fp_item(changed_seq);
CREATE INDEX IF NOT EXISTS idx_fp_item_path ON fp_item(path_lower);
CREATE INDEX IF NOT EXISTS idx_fp_item_orphan ON fp_item(path_lower) WHERE state = 3;
CREATE TABLE IF NOT EXISTS fp_pin (
    item_id INTEGER PRIMARY KEY REFERENCES fp_identity(item_id)
);
CREATE TABLE IF NOT EXISTS fp_state (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    schema_version INTEGER NOT NULL,
    generation INTEGER NOT NULL,
    seq INTEGER NOT NULL,
    min_valid_seq INTEGER NOT NULL,
    populated INTEGER NOT NULL CHECK (populated IN (0, 1))
);
";

/// Creates the tables if they are missing and seeds the single `fp_state` row. Called from the
/// transactional `migrate`; idempotent, and never alters an existing table.
pub(super) fn migrate(conn: &Connection) -> AppResult<()> {
    conn.execute_batch(SCHEMA_SQL)?;
    // Minted once: `OR IGNORE` keeps the generation of an existing database.
    let generation = i64::from(rand::random::<u32>().max(1));
    conn.execute(
        "INSERT OR IGNORE INTO fp_state(id, schema_version, generation, seq, min_valid_seq, populated)
         VALUES (1, ?1, ?2, 0, 0, 0)",
        params![SCHEMA_VERSION, generation],
    )?;
    Ok(())
}

// ── versions ────────────────────────────────────────────────────────────────────────────────────

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Fixed-length token of a string: 64 hex characters, whatever the input length.
fn digest_hex(parts: &[&[u8]]) -> String {
    let mut h = Sha256::new();
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            h.update([0u8]);
        }
        h.update(p);
    }
    hex(&h.finalize())
}

/// `NSFileProviderItemVersion.contentVersion`: derived from `content_hash` and from nothing else.
/// `rev` changes when a file is moved while its bytes stay the same (measured on #234), so using it
/// here would make the system re-download unchanged content after every move. A folder has no
/// content: a fixed token. 67 bytes at most; the system's limit is 128.
pub(crate) fn content_version(is_folder: bool, content_hash: &str) -> Vec<u8> {
    if is_folder {
        return b"c1:folder".to_vec();
    }
    format!("c1:{}", digest_hex(&[content_hash.as_bytes()])).into_bytes()
}

/// `NSFileProviderItemVersion.metadataVersion`: a hash of name, parent and `rev`, so a rename, a move
/// or a new revision each change it. 67 bytes, whatever the length of a name.
pub(crate) fn meta_version(name: &str, parent_item: i64, rev: &str) -> Vec<u8> {
    let parent = parent_item.to_string();
    format!(
        "m1:{}",
        digest_hex(&[name.as_bytes(), parent.as_bytes(), rev.as_bytes()])
    )
    .into_bytes()
}

// ── entries ─────────────────────────────────────────────────────────────────────────────────────

/// What a write needs of a live item (`Db::projection_write_target`).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct WriteTarget {
    /// From the root, `/`-separated, in the names' own case.
    pub(crate) path: String,
    pub(crate) is_folder: bool,
    pub(crate) rev: String,
    pub(crate) content_version: Vec<u8>,
    pub(crate) content_hash: String,
}

/// What the host needs to download a live file.
pub(crate) struct ProjectionDownloadTarget {
    pub(crate) rev: String,
    pub(crate) content_hash: String,
    /// The `content_version` column: what the extension compares the row with after the download.
    pub(crate) content_version: Vec<u8>,
    pub(crate) path_lower: String,
}

/// One file or folder as the projection stores it, taken from a raw Dropbox entry. Compact on
/// purpose: a snapshot holds one per item of the account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectedEntry {
    pub dropbox_id: String,
    /// Dropbox's normalized lowercase path, with a leading `/`. The index deletes resolve through.
    pub path_lower: String,
    pub name: String,
    pub is_folder: bool,
    pub size_bytes: i64,
    pub modified_ts: i64,
    pub content_hash: String,
    pub rev: String,
}

/// `/a/b` for a raw path, folded (NFC then lowercase: [`fold`]); `None` for the root or an empty path.
/// The projection's one comparison form for a path.
fn normalize_path_lower(raw: &str) -> Option<String> {
    let folded = fold(raw);
    let trimmed = folded.trim_matches('/');
    (!trimmed.is_empty()).then(|| format!("/{trimmed}"))
}

/// The folded path of an entry, from `path_lower` or, failing that, `path_display`.
fn entry_path_lower(e: &DropboxEntry) -> Option<String> {
    match (&e.path_lower, &e.path_display) {
        (Some(lower), _) => normalize_path_lower(lower),
        (None, Some(display)) => normalize_path_lower(display),
        (None, None) => None,
    }
}

fn last_component(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

impl ProjectedEntry {
    /// `None` for anything that is not a file or folder with an id and a path: a `deleted` entry, an
    /// unknown tag, or an entry the projection could not key.
    pub(crate) fn from_entry(e: &DropboxEntry) -> Option<Self> {
        let is_folder = match e.tag.as_str() {
            "file" => false,
            "folder" => true,
            _ => return None,
        };
        let dropbox_id = e.id.as_deref().filter(|id| !id.is_empty())?.to_string();
        let path_lower = entry_path_lower(e)?;
        let name = e
            .path_display
            .as_deref()
            .map(|d| last_component(d.trim_end_matches('/')))
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| last_component(&path_lower))
            .to_string();
        Some(Self {
            dropbox_id,
            path_lower,
            name,
            is_folder,
            size_bytes: if is_folder {
                0
            } else {
                e.size.unwrap_or(0).max(0)
            },
            modified_ts: e
                .server_modified
                .as_deref()
                .map(crate::remote_index::parse_rfc3339_ts_to_unix)
                .unwrap_or(0),
            content_hash: e.content_hash.clone().unwrap_or_default(),
            rev: e.rev.clone().unwrap_or_default(),
        })
    }
}

// ── the change sequence ─────────────────────────────────────────────────────────────────────────

/// Hands out the next `changed_seq`. The sequence is loaded when a writer's transaction opens and
/// saved when it ends, so a batch of ten thousand rows does not run twenty thousand statements.
struct Stamper {
    seq: i64,
    stamped: usize,
    /// How many rows this writer turned into tombstones (the cap check runs only when it is not 0).
    tombstoned: usize,
    /// How many rows were listed before this writer and are not at the end of it.
    vanished: usize,
}

impl Stamper {
    fn load(conn: &Connection) -> rusqlite::Result<Self> {
        let seq = conn.query_row("SELECT seq FROM fp_state WHERE id = 1", [], |r| r.get(0))?;
        Ok(Self {
            seq,
            stamped: 0,
            tombstoned: 0,
            vanished: 0,
        })
    }

    /// The only place the change sequence moves: every observable change takes exactly one value.
    fn stamp(&mut self) -> i64 {
        self.seq += 1;
        self.stamped += 1;
        self.seq
    }

    fn save(&self, conn: &Connection) -> rusqlite::Result<()> {
        if self.stamped > 0 {
            conn.execute("UPDATE fp_state SET seq = ?1 WHERE id = 1", [self.seq])?;
        }
        Ok(())
    }
}

/// What a writer committed. `rows` counts the sequence values it took (a snapshot or a clear also
/// takes one for the expiry), so `rows == 0` means nothing changed and nobody has to be told.
/// `vanished` counts the rows that were live before the batch and are not after it; only a delta acts
/// on it, because only a delta can be the first half of a rename.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProjectionCommit {
    pub rows: usize,
    pub seq: i64,
    pub vanished: usize,
}

/// The anchor ingredients, as stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProjectionState {
    pub generation: i64,
    pub seq: i64,
    pub min_valid_seq: i64,
    pub populated: bool,
}

/// Whether an anchor `v<format>.<generation>.<seq>` is expired: a different generation, a sequence below
/// `min_valid_seq` or one the database has not reached. Nothing else expires it. The extension holds
/// the same rule in Swift (slice 2b).
#[allow(dead_code)]
pub(crate) fn anchor_expired(state: &ProjectionState, generation: i64, seq: i64) -> bool {
    generation != state.generation || seq < state.min_valid_seq || seq > state.seq
}

// ── rows ────────────────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct StoredRow {
    item_id: i64,
    parent_item: i64,
    path_lower: String,
    name: String,
    is_folder: bool,
    size_bytes: i64,
    modified_ts: i64,
    content_hash: String,
    rev: String,
    content_version: Vec<u8>,
    meta_version: Vec<u8>,
    downloaded: bool,
    state: i64,
}

const ROW_COLUMNS: &str =
    "item_id, parent_item, path_lower, name, is_folder, size_bytes, modified_ts, \
                           content_hash, rev, content_version, meta_version, downloaded, state";

fn row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<StoredRow> {
    Ok(StoredRow {
        item_id: r.get(0)?,
        parent_item: r.get(1)?,
        path_lower: r.get(2)?,
        name: r.get(3)?,
        is_folder: r.get::<_, i64>(4)? != 0,
        size_bytes: r.get(5)?,
        modified_ts: r.get(6)?,
        content_hash: r.get(7)?,
        rev: r.get(8)?,
        content_version: r.get(9)?,
        meta_version: r.get(10)?,
        downloaded: r.get::<_, i64>(11)? != 0,
        state: r.get(12)?,
    })
}

fn load_row(conn: &Connection, item_id: i64) -> rusqlite::Result<Option<StoredRow>> {
    conn.prepare_cached(&format!(
        "SELECT {ROW_COLUMNS} FROM fp_item WHERE item_id = ?1"
    ))?
    .query_row([item_id], row_from)
    .optional()
}

/// The identity a Dropbox id already has, without minting one.
fn existing_identity(conn: &Connection, dropbox_id: &str) -> rusqlite::Result<Option<i64>> {
    conn.prepare_cached("SELECT item_id FROM fp_identity WHERE dropbox_id = ?1")?
        .query_row([dropbox_id], |r| r.get(0))
        .optional()
}

/// The item a Dropbox id stands for, minting its identity the first time it is seen.
///
/// The number comes from `item_id_seq`, the allocator `local_file_index.item_id` already uses
/// (DBSYNC-99): an AUTOINCREMENT table keeps its high-water mark after the claiming row is deleted,
/// so an identifier is never handed out twice, and the two kinds of item can never share one. It is
/// not `MAX(item_id) + 1`, which hands a dead item's number to a new one, and it is never derived
/// from a path.
fn identity_for(conn: &Connection, dropbox_id: &str) -> rusqlite::Result<i64> {
    if let Some(id) = existing_identity(conn, dropbox_id)? {
        return Ok(id);
    }
    conn.execute("INSERT INTO item_id_seq DEFAULT VALUES", [])?;
    let item_id = conn.last_insert_rowid();
    conn.execute("DELETE FROM item_id_seq", [])?;
    conn.execute(
        "INSERT INTO fp_identity(item_id, dropbox_id, created_at) VALUES (?1, ?2, ?3)",
        params![item_id, dropbox_id, chrono::Utc::now().to_rfc3339()],
    )?;
    Ok(item_id)
}

struct NewRow<'a> {
    item_id: i64,
    parent_item: i64,
    path_lower: &'a str,
    name: &'a str,
    is_folder: bool,
    size_bytes: i64,
    modified_ts: i64,
    content_hash: &'a str,
    rev: &'a str,
    state: i64,
}

/// Selective sync (DBSYNC-114): a file is visible when it is in the synced zone; a folder also when
/// it is the ancestor of an include entry, so what lies inside stays reachable.
fn visibility(filter: &PrefixFilter, path_lower: &str, is_folder: bool) -> i64 {
    let rel = path_lower.trim_matches('/');
    let visible = if is_folder {
        filter.scope(rel) != PathScope::Excluded
    } else {
        filter.file_synced(rel)
    };
    if visible {
        STATE_LIVE
    } else {
        STATE_HIDDEN
    }
}

/// `/a/b` of `/a/b/c.txt`; empty for an item directly under the root.
fn parent_path(path_lower: &str) -> &str {
    match path_lower.rfind('/') {
        Some(i) if i > 0 => &path_lower[..i],
        _ => "",
    }
}

/// A stored row a path lookup found.
#[derive(Debug, Clone)]
struct Hit {
    id: i64,
    is_folder: bool,
}

fn hit_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<Hit> {
    Ok(Hit {
        id: r.get(0)?,
        is_folder: r.get::<_, i64>(1)? != 0,
    })
}

/// Every row strictly beneath `path`, tombstones excluded. `path/` to `path0` is the range of strings
/// that begin with `path/` (`0` is the character after `/`), which the `path_lower` index serves and
/// which cannot match a sibling such as `path2`. `path` is a folded path, like every stored one.
fn rows_under(conn: &Connection, path: &str) -> rusqlite::Result<Vec<Hit>> {
    let mut stmt = conn.prepare_cached(
        "SELECT item_id, is_folder FROM fp_item
         WHERE state != 2 AND path_lower >= ?1 AND path_lower < ?2",
    )?;
    let rows = stmt.query_map([format!("{path}/"), format!("{path}0")], hit_from)?;
    rows.collect()
}

/// Every non-deleted row stored exactly at the folded `path`.
fn rows_at(conn: &Connection, path: &str) -> rusqlite::Result<Vec<Hit>> {
    let mut stmt = conn.prepare_cached(
        "SELECT item_id, is_folder FROM fp_item
         WHERE state != 2 AND path_lower = ?1",
    )?;
    let rows = stmt.query_map([path], hit_from)?;
    rows.collect()
}

/// The folder stored at `path` that a child at `path/x` hangs under. Neither a tombstone nor an ORPHAN
/// counts: a child of an orphan folder is itself an orphan, so a live row never has a non-live parent.
fn folder_at(conn: &Connection, path: &str) -> rusqlite::Result<Option<i64>> {
    conn.prepare_cached(
        "SELECT item_id FROM fp_item
         WHERE path_lower = ?1 AND is_folder = 1 AND state NOT IN (2, 3)
         ORDER BY item_id DESC LIMIT 1",
    )?
    .query_row([path], |r| r.get(0))
    .optional()
}

/// The non-deleted rows whose stored parent is `parent_item`.
fn children_of(conn: &Connection, parent_item: i64) -> rusqlite::Result<Vec<i64>> {
    let mut stmt =
        conn.prepare_cached("SELECT item_id FROM fp_item WHERE parent_item = ?1 AND state != 2")?;
    let rows = stmt.query_map([parent_item], |r| r.get(0))?;
    rows.collect()
}

/// The parent of an item at `path`: the root, the live folder stored at the parent path, or `None`
/// (an orphan) when there is none.
fn resolve_parent(conn: &Connection, path: &str) -> rusqlite::Result<Option<i64>> {
    let parent = parent_path(path);
    if parent.is_empty() {
        return Ok(Some(ROOT_ITEM));
    }
    folder_at(conn, parent)
}

// ── applying a batch, one entry at a time ───────────────────────────────────────────────────────

/// One step of a batch, in the order Dropbox sent it.
#[derive(Debug, Clone, Copy)]
enum Step<'a> {
    /// A `deleted` entry: the folded path, nothing else (Dropbox sends no id).
    Delete(&'a str),
    /// A `file` or `folder` entry.
    Upsert(&'a ProjectedEntry),
}

/// The owned form of a [`Step`], for a delta, whose entries are converted once.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Op {
    Delete(String),
    Upsert(ProjectedEntry),
}

impl Op {
    fn step(&self) -> Step<'_> {
        match self {
            Op::Delete(p) => Step::Delete(p),
            Op::Upsert(e) => Step::Upsert(e),
        }
    }
}

/// The raw entries of one delta invocation as steps, in their original order. Entries the projection
/// cannot key (an unknown tag, no id, no path, the root) are skipped.
fn ops_from_entries(entries: &[DropboxEntry]) -> Vec<Op> {
    entries
        .iter()
        .filter_map(|e| {
            if e.tag == "deleted" {
                entry_path_lower(e).map(Op::Delete)
            } else {
                ProjectedEntry::from_entry(e).map(Op::Upsert)
            }
        })
        .collect()
}

/// The state of one batch while it is applied. Each entry is applied in order (see the module docs).
/// A row that existed before the batch is stamped in [`Sequence::finish`], and only if its final state
/// differs from the one it began with; a row the batch creates is stamped as it is inserted, so it is
/// written once.
struct Sequence<'a> {
    conn: &'a Connection,
    st: &'a mut Stamper,
    filter: &'a PrefixFilter,
    /// Every row the batch touched, as it was before the batch (`None`: it did not exist). Insertion
    /// order is kept in `order`, so the stamps come out in a fixed order.
    before: HashMap<i64, Option<StoredRow>>,
    order: Vec<i64>,
    /// Rows the batch inserted (already stamped).
    created: HashSet<i64>,
    /// Ids of every upserted entry (the snapshot sweeps whatever is not in it).
    upserted: HashSet<i64>,
}

impl<'a> Sequence<'a> {
    fn new(conn: &'a Connection, st: &'a mut Stamper, filter: &'a PrefixFilter) -> Self {
        Self {
            conn,
            st,
            filter,
            before: HashMap::new(),
            order: Vec::new(),
            created: HashSet::new(),
            upserted: HashSet::new(),
        }
    }

    fn apply(&mut self, step: Step<'_>) -> rusqlite::Result<()> {
        match step {
            Step::Delete(path) => self.delete(path),
            Step::Upsert(entry) => self.upsert(entry),
        }
    }

    /// Remembers a row as it stood before the batch, the first time the batch touches it.
    fn touch(&mut self, item_id: i64) -> rusqlite::Result<()> {
        if !self.before.contains_key(&item_id) {
            let pre = load_row(self.conn, item_id)?;
            self.remember(item_id, pre);
        }
        Ok(())
    }

    fn remember(&mut self, item_id: i64, pre: Option<StoredRow>) {
        if let std::collections::hash_map::Entry::Vacant(slot) = self.before.entry(item_id) {
            slot.insert(pre);
            self.order.push(item_id);
        }
    }

    /// Writes a row. `downloaded` is not touched here: `finish` decides it once the row's final state is
    /// known. A row that did not exist before the batch is inserted with its stamp.
    fn put(&mut self, new: &NewRow<'_>) -> rusqlite::Result<()> {
        self.touch(new.item_id)?;
        let fresh =
            matches!(self.before.get(&new.item_id), Some(None)) && self.created.insert(new.item_id);
        let changed_seq = if fresh { self.st.stamp() } else { 0 };
        self.conn
            .prepare_cached(
                "INSERT INTO fp_item(item_id, parent_item, path_lower, name, is_folder, size_bytes, \
                 modified_ts, content_hash, rev, content_version, meta_version, downloaded, state, changed_seq)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 0, ?12, ?13)
                 ON CONFLICT(item_id) DO UPDATE SET
                     parent_item = excluded.parent_item, path_lower = excluded.path_lower,
                     name = excluded.name, is_folder = excluded.is_folder,
                     size_bytes = excluded.size_bytes, modified_ts = excluded.modified_ts,
                     content_hash = excluded.content_hash, rev = excluded.rev,
                     content_version = excluded.content_version,
                     meta_version = excluded.meta_version, state = excluded.state",
            )?
            .execute(params![
                new.item_id,
                new.parent_item,
                new.path_lower,
                new.name,
                new.is_folder,
                new.size_bytes,
                new.modified_ts,
                new.content_hash,
                new.rev,
                content_version(new.is_folder, new.content_hash),
                meta_version(new.name, new.parent_item, new.rev),
                new.state,
                changed_seq
            ])?;
        Ok(())
    }

    /// [`Sequence::put`] for a stored row whose data stays: a new parent or a new state only.
    fn put_stored(
        &mut self,
        old: &StoredRow,
        parent_item: i64,
        state: i64,
    ) -> rusqlite::Result<()> {
        self.put(&NewRow {
            item_id: old.item_id,
            parent_item,
            path_lower: &old.path_lower,
            name: &old.name,
            is_folder: old.is_folder,
            size_bytes: old.size_bytes,
            modified_ts: old.modified_ts,
            content_hash: &old.content_hash,
            rev: &old.rev,
            state,
        })
    }

    /// The parent and the state of an item that now sits at `path`: under the live folder stored at the
    /// parent path, with selective sync deciding its visibility, or an orphan when there is none.
    fn place(&self, path: &str, is_folder: bool) -> rusqlite::Result<(i64, i64)> {
        Ok(match resolve_parent(self.conn, path)? {
            Some(parent) => (parent, visibility(self.filter, path, is_folder)),
            None => (NO_PARENT, STATE_ORPHAN),
        })
    }

    fn tombstone(&mut self, item_id: i64) -> rusqlite::Result<()> {
        self.touch(item_id)?;
        self.conn
            .prepare_cached("UPDATE fp_item SET state = 2 WHERE item_id = ?1")?
            .execute([item_id])?;
        self.st.tombstoned += 1;
        Ok(())
    }

    /// `deleted P`: the non-deleted row at `P` and every non-deleted row under `P/`.
    fn delete(&mut self, path: &str) -> rusqlite::Result<()> {
        for hit in rows_at(self.conn, path)? {
            self.tombstone(hit.id)?;
        }
        for hit in rows_under(self.conn, path)? {
            self.tombstone(hit.id)?;
        }
        Ok(())
    }

    /// `file|folder P` with id `I`. A different non-deleted item `R` at `P` is replaced: a file
    /// tombstones `R` and everything under `P/` (a file has no children); a folder
    /// tombstones only `R` and, when `R` is a folder too, hands `R`'s children to `I`. Then `I` itself
    /// is updated in place wherever it is and whatever its state, or created when the id is new. Nothing
    /// else moves: a folder's children change only through their own entries.
    fn upsert(&mut self, e: &ProjectedEntry) -> rusqlite::Result<()> {
        let item_id = identity_for(self.conn, &e.dropbox_id)?;
        self.upserted.insert(item_id);

        let mut adopted: Vec<i64> = Vec::new();
        for hit in rows_at(self.conn, &e.path_lower)? {
            if hit.id == item_id {
                continue;
            }
            self.tombstone(hit.id)?;
            if !hit.is_folder {
                continue;
            }
            if e.is_folder {
                adopted.extend(children_of(self.conn, hit.id)?);
            }
        }

        // A file at `P` leaves nothing under `P/`, whether or not it displaced a row there: a replayed
        // batch finds itself already at `P` and must still clear what a folder there once held.
        if !e.is_folder {
            for sub in rows_under(self.conn, &e.path_lower)? {
                if sub.id != item_id {
                    self.tombstone(sub.id)?;
                }
            }
        }

        let state = self.write_entry(item_id, e)?;

        // The children of the folder this one replaced: an orphan folder is no parent, so they wait
        // as orphans with it.
        for child_id in adopted.into_iter().filter(|c| *c != item_id) {
            let Some(child) = load_row(self.conn, child_id)? else {
                continue;
            };
            let (parent, child_state) = if state == STATE_ORPHAN {
                (NO_PARENT, STATE_ORPHAN)
            } else {
                (
                    item_id,
                    visibility(self.filter, &child.path_lower, child.is_folder),
                )
            };
            self.put_stored(&child, parent, child_state)?;
        }
        Ok(())
    }

    /// The snapshot's upsert: by id alone. No displacement and no inference. The parent found now may
    /// be a row the sweep removes, so [`Sequence::reresolve_all`] runs afterwards.
    fn upsert_by_id(&mut self, e: &ProjectedEntry) -> rusqlite::Result<()> {
        let item_id = identity_for(self.conn, &e.dropbox_id)?;
        self.upserted.insert(item_id);
        self.write_entry(item_id, e)?;
        Ok(())
    }

    /// Writes the row of `item_id` as `e` says, at the place its path gives it, and returns its state.
    /// An entry that restates the row as it is writes nothing (a reseed over an unchanged tree is most
    /// of its rows).
    fn write_entry(&mut self, item_id: i64, e: &ProjectedEntry) -> rusqlite::Result<i64> {
        let old = load_row(self.conn, item_id)?;
        let (parent_item, state) = self.place(&e.path_lower, e.is_folder)?;
        let new = NewRow {
            item_id,
            parent_item,
            path_lower: &e.path_lower,
            name: &e.name,
            is_folder: e.is_folder,
            size_bytes: e.size_bytes,
            modified_ts: e.modified_ts,
            content_hash: &e.content_hash,
            rev: &e.rev,
            state,
        };
        if !old.as_ref().is_some_and(|o| restates(o, &new)) {
            self.remember(item_id, old);
            self.put(&new)?;
        }
        Ok(state)
    }

    /// End of a delta batch: every row the batch named or wrote takes the parent its path gives it now,
    /// parents first. A row's parent is bound when the row is written, and a later entry of the same
    /// batch can move that folder away and put another at the path: the row would stay live under the
    /// folder that left, and under a tombstone once that one is deleted.
    fn reresolve_touched(&mut self) -> rusqlite::Result<()> {
        let ids: HashSet<i64> = self
            .order
            .iter()
            .chain(self.upserted.iter())
            .copied()
            .collect();
        let mut rows = Vec::new();
        for id in ids {
            if let Some(row) = load_row(self.conn, id)? {
                if row.state == STATE_LIVE || row.state == STATE_HIDDEN {
                    rows.push(row);
                }
            }
        }
        rows.sort_by(|a, b| a.path_lower.cmp(&b.path_lower));
        for row in rows {
            let (parent, state) = self.place(&row.path_lower, row.is_folder)?;
            if parent != row.parent_item || state != row.state {
                self.put_stored(&row, parent, state)?;
            }
        }
        Ok(())
    }

    /// Ids upserted so far.
    fn seen(&self) -> &HashSet<i64> {
        &self.upserted
    }

    /// Re-resolves the parent and the orphan state of every stored row, parents before children (path
    /// order puts a prefix first). Only a row whose answer changed is written.
    fn reresolve_all(&mut self) -> rusqlite::Result<()> {
        let rows: Vec<(i64, i64, String, bool, i64)> = {
            let mut stmt = self.conn.prepare_cached(
                "SELECT item_id, parent_item, path_lower, is_folder, state FROM fp_item
                 WHERE state != 2 ORDER BY path_lower",
            )?;
            let mapped = stmt.query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get::<_, i64>(3)? != 0,
                    r.get(4)?,
                ))
            })?;
            mapped.collect::<rusqlite::Result<_>>()?
        };
        // Parent path to its folder: the pass goes parents first, so an answer never goes stale.
        let mut parents: HashMap<String, Option<i64>> = HashMap::new();
        for (item_id, parent_item, path, is_folder, state) in rows {
            let parent_path = parent_path(&path);
            let found = if parent_path.is_empty() {
                Some(ROOT_ITEM)
            } else if let Some(hit) = parents.get(parent_path) {
                *hit
            } else {
                let found = folder_at(self.conn, parent_path)?;
                parents.insert(parent_path.to_string(), found);
                found
            };
            let (parent, wanted) = match found {
                Some(p) => (p, visibility(self.filter, &path, is_folder)),
                None => (NO_PARENT, STATE_ORPHAN),
            };
            if parent != parent_item || wanted != state {
                if let Some(row) = load_row(self.conn, item_id)? {
                    self.put_stored(&row, parent, wanted)?;
                }
            }
        }
        Ok(())
    }

    /// End of batch. An orphan whose parent folder exists now becomes an ordinary item, following
    /// selective sync; a row whose parent is an orphan becomes one itself, so a live row never hangs
    /// under a parent that is not live. Both cascade down a chain, so they repeat until nothing moves.
    /// Only an orphan is ever healed: a tombstone stays one.
    fn heal_orphans(&mut self) -> rusqlite::Result<()> {
        // Each round either moves a row or ends; a chain is as deep as the tree, and the heal pass
        // takes a whole chain at once because it goes in path order, so a handful of rounds is plenty.
        for _ in 0..100 {
            let moved = self.heal_pass()? + self.demote_pass()?;
            if moved == 0 {
                break;
            }
        }
        Ok(())
    }

    fn heal_pass(&mut self) -> rusqlite::Result<usize> {
        let ids: Vec<i64> = {
            let mut stmt = self.conn.prepare_cached(
                "SELECT item_id FROM fp_item WHERE state = 3 ORDER BY path_lower",
            )?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut healed = 0;
        for id in ids {
            let Some(row) = load_row(self.conn, id)? else {
                continue;
            };
            if let Some(parent) = resolve_parent(self.conn, &row.path_lower)? {
                let state = visibility(self.filter, &row.path_lower, row.is_folder);
                self.put_stored(&row, parent, state)?;
                healed += 1;
            }
        }
        Ok(healed)
    }

    /// Rows that are not orphans but hang under one become orphans, down the whole chain.
    fn demote_pass(&mut self) -> rusqlite::Result<usize> {
        let mut demoted = 0;
        loop {
            let ids: Vec<i64> = {
                let mut stmt = self.conn.prepare_cached(
                    "SELECT c.item_id FROM fp_item p JOIN fp_item c ON c.parent_item = p.item_id
                     WHERE p.state = 3 AND c.state IN (0, 1)",
                )?;
                let rows = stmt.query_map([], |r| r.get(0))?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            if ids.is_empty() {
                return Ok(demoted);
            }
            for id in ids {
                if let Some(row) = load_row(self.conn, id)? {
                    self.put_stored(&row, NO_PARENT, STATE_ORPHAN)?;
                    demoted += 1;
                }
            }
        }
    }

    /// Stamps every pre-existing touched row whose final state differs from its state before the
    /// batch, once. A row that ends where it began (a replayed batch, a delete followed by an identical
    /// re-add) takes no value. Two tombstones are the same state whatever else the batch wrote into them.
    fn finish(mut self) -> rusqlite::Result<()> {
        let mut moved_folders = Vec::new();
        // Rows created by the batch were stamped when inserted.
        let mut stamped: HashSet<i64> = self.created.clone();
        let mut stamp = self.conn.prepare_cached(
            "UPDATE fp_item SET changed_seq = ?2, downloaded = ?3 WHERE item_id = ?1",
        )?;
        for item_id in &self.order {
            // A row the batch created was stamped when it was inserted.
            let Some(before) = &self.before[item_id] else {
                continue;
            };
            let Some(now) = load_row(self.conn, *item_id)? else {
                continue;
            };
            let unchanged = (before.state == STATE_DELETED && now.state == STATE_DELETED)
                || same_observable(before, &now);
            if unchanged {
                continue;
            }
            // New bytes make a local copy stale; anything else leaves the flag alone.
            let downloaded = before.downloaded && before.content_hash == now.content_hash;
            stamp.execute(params![item_id, self.st.stamp(), downloaded])?;
            stamped.insert(*item_id);
            if before.state == STATE_LIVE && now.state != STATE_LIVE {
                self.st.vanished += 1;
            }
            if now.is_folder && now.state == STATE_LIVE && before.parent_item != now.parent_item {
                moved_folders.push((*item_id, before.parent_item, now.parent_item));
            }
        }
        self.restamp_under_moved_folders(&moved_folders, stamped)
    }

    /// What is under a folder that moved into or out of a pinned folder now inherits the pin, or no longer
    /// (#237), which the extension reports on each item (`userInfo.inherited`), so the live rows under it
    /// are read again. A move that keeps the folder on the same side, the common case, stamps nothing
    /// more, and neither does a row this batch already stamped.
    fn restamp_under_moved_folders(
        &mut self,
        moved: &[(i64, i64, i64)],
        mut done: HashSet<i64>,
    ) -> rusqlite::Result<()> {
        if moved.is_empty() {
            return Ok(());
        }
        let pinned: HashSet<i64> = {
            let mut stmt = self.conn.prepare_cached("SELECT item_id FROM fp_pin")?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        if pinned.is_empty() {
            return Ok(());
        }
        for (folder, old_parent, new_parent) in moved {
            if under_pin(self.conn, &pinned, *old_parent)?
                == under_pin(self.conn, &pinned, *new_parent)?
            {
                continue;
            }
            let under: Vec<i64> = {
                let mut stmt = self.conn.prepare_cached(
                    "WITH RECURSIVE tree(id) AS (
                         SELECT item_id FROM fp_item WHERE parent_item = ?1 AND state = 0
                         UNION
                         SELECT i.item_id FROM fp_item i JOIN tree ON i.parent_item = tree.id
                         WHERE i.state = 0
                     )
                     SELECT id FROM tree ORDER BY id",
                )?;
                let rows = stmt.query_map([*folder], |r| r.get(0))?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            for id in under {
                if done.insert(id) {
                    self.conn
                        .prepare_cached("UPDATE fp_item SET changed_seq = ?2 WHERE item_id = ?1")?
                        .execute(params![id, self.st.stamp()])?;
                }
            }
        }
        Ok(())
    }
}

/// Whether `folder` or a folder above it is pinned. The root and an orphan's `-1` are not.
fn under_pin(conn: &Connection, pinned: &HashSet<i64>, folder: i64) -> rusqlite::Result<bool> {
    let mut at = folder;
    for _ in 0..4096 {
        if pinned.contains(&at) {
            return Ok(true);
        }
        if at == ROOT_ITEM || at == NO_PARENT {
            return Ok(false);
        }
        let parent: Option<i64> = conn
            .prepare_cached("SELECT parent_item FROM fp_item WHERE item_id = ?1")?
            .query_row([at], |r| r.get(0))
            .optional()?;
        match parent {
            Some(parent) => at = parent,
            None => return Ok(false),
        }
    }
    Ok(false)
}

/// Whether writing `new` over `old` would change nothing at all.
fn restates(old: &StoredRow, new: &NewRow<'_>) -> bool {
    old.parent_item == new.parent_item
        && old.path_lower == new.path_lower
        && old.name == new.name
        && old.is_folder == new.is_folder
        && old.size_bytes == new.size_bytes
        && old.modified_ts == new.modified_ts
        && old.content_hash == new.content_hash
        && old.rev == new.rev
        && old.state == new.state
}

/// Everything the extension can see of a row: parent, name, kind, size, mtime, hash, rev, state and
/// both versions. The stored `path_lower` is bookkeeping and is not part of it.
fn same_observable(a: &StoredRow, b: &StoredRow) -> bool {
    a.parent_item == b.parent_item
        && a.name == b.name
        && a.is_folder == b.is_folder
        && a.size_bytes == b.size_bytes
        && a.modified_ts == b.modified_ts
        && a.content_hash == b.content_hash
        && a.rev == b.rev
        && a.content_version == b.content_version
        && a.meta_version == b.meta_version
        && a.state == b.state
}

/// Applies a delta batch to the projection, entry by entry in the order given, then heals orphans and
/// stamps what changed. See the module docs for the rules.
fn apply_steps<'a>(
    conn: &Connection,
    st: &mut Stamper,
    filter: &PrefixFilter,
    steps: impl IntoIterator<Item = Step<'a>>,
) -> rusqlite::Result<()> {
    let mut run = Sequence::new(conn, st, filter);
    for step in steps {
        run.apply(step)?;
    }
    run.reresolve_touched()?;
    run.heal_orphans()?;
    run.finish()
}

/// Moves `min_valid_seq` up to a fresh sequence value, which expires every anchor handed out before
/// it. One of the two places that may do so.
fn expire_anchors(conn: &Connection, st: &mut Stamper, populated: bool) -> rusqlite::Result<()> {
    let seq = st.stamp();
    conn.execute(
        "UPDATE fp_state SET min_valid_seq = ?1, populated = ?2 WHERE id = 1",
        params![seq, populated],
    )?;
    Ok(())
}

/// A pin lives as long as its row: one left behind would pin whatever row came back under the id.
fn drop_orphan_pins(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM fp_pin WHERE item_id NOT IN (SELECT item_id FROM fp_item)",
        [],
    )?;
    Ok(())
}

/// Empties the projection and marks it unpopulated, if it holds anything. Identities stay. A no-op
/// (no stamp, no write) on a projection that is already empty and unpopulated.
fn clear_with(conn: &Connection, st: &mut Stamper) -> rusqlite::Result<bool> {
    let populated: bool =
        conn.query_row("SELECT populated FROM fp_state WHERE id = 1", [], |r| {
            r.get(0)
        })?;
    let has_rows: bool =
        conn.query_row("SELECT EXISTS (SELECT 1 FROM fp_item)", [], |r| r.get(0))?;
    if !populated && !has_rows {
        return Ok(false);
    }
    // Pins stay: identities survive a clear, so the reseed that follows (the switch turned back on, a
    // failed snapshot or delta recovering) brings back the same items, and the snapshot's sweep drops
    // the pins of those that did not come back. Leaving the account drops them (`clear_in`).
    conn.execute("DELETE FROM fp_item", [])?;
    expire_anchors(conn, st, false)?;
    Ok(true)
}

/// The clear, for a caller that already has a transaction open (`Db::reset_sync_state`).
pub(super) fn clear_in(conn: &Connection) -> rusqlite::Result<ProjectionCommit> {
    let mut st = Stamper::load(conn)?;
    clear_with(conn, &mut st)?;
    // The account was left: its pins were choices about its files.
    conn.execute("DELETE FROM fp_pin", [])?;
    st.save(conn)?;
    Ok(ProjectionCommit {
        rows: st.stamped,
        seq: st.seq,
        vanished: st.vanished,
    })
}

// ── the Db surface ──────────────────────────────────────────────────────────────────────────────

impl Db {
    /// Whether the projection is being fed: macOS (this module exists nowhere else) AND the user put
    /// Dropbox in Finder, which `file_provider::start`, `enable` and `disable` record (DBSYNC-98).
    pub(crate) fn projection_active(&self) -> bool {
        self.projection_enabled
            .load(std::sync::atomic::Ordering::Acquire)
    }

    pub(crate) fn set_projection_enabled(&self, on: bool) {
        self.projection_enabled
            .store(on, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn projection_state(&self) -> AppResult<ProjectionState> {
        let conn = self
            .read
            .lock()
            .map_err(|_| AppError::Storage("db read lock poisoned".into()))?;
        Ok(conn.query_row(
            "SELECT generation, seq, min_valid_seq, populated FROM fp_state WHERE id = 1",
            [],
            |r| {
                Ok(ProjectionState {
                    generation: r.get(0)?,
                    seq: r.get(1)?,
                    min_valid_seq: r.get(2)?,
                    populated: r.get(3)?,
                })
            },
        )?)
    }

    /// Whether the projection is populated or holds any row: the read that decides whether a clear
    /// has anything to do, so an empty one never opens a write transaction.
    pub(crate) fn projection_holds_anything(&self) -> AppResult<bool> {
        let conn = self
            .read
            .lock()
            .map_err(|_| AppError::Storage("db read lock poisoned".into()))?;
        Ok(conn.query_row(
            "SELECT populated != 0 OR EXISTS (SELECT 1 FROM fp_item) FROM fp_state WHERE id = 1",
            [],
            |r| r.get(0),
        )?)
    }

    /// One writer transaction: opened immediately (the host is the only writer, but a second launch
    /// racing on the file queues on the busy timeout instead of failing at the read-to-write
    /// upgrade), the sequence loaded, the closure run, the sequence saved, committed. A closure error
    /// rolls everything back.
    fn write_projection<T>(
        &self,
        writer: &'static str,
        f: impl FnOnce(&Connection, &mut Stamper) -> rusqlite::Result<T>,
    ) -> AppResult<(T, ProjectionCommit)> {
        #[cfg(test)]
        run_before_tx_hook(self);
        let mut conn = self
            .write
            .lock()
            .map_err(|_| AppError::Storage("db write lock poisoned".into()))?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut st = Stamper::load(&tx)?;
        let out = f(&tx, &mut st)?;
        st.save(&tx)?;
        tx.commit()?;
        let commit = ProjectionCommit {
            rows: st.stamped,
            seq: st.seq,
            vanished: st.vanished,
        };
        if commit.rows > 0 {
            // Counts only: a path or a name would put user data in a log.
            tracing::info!(
                writer,
                rows = commit.rows,
                seq = commit.seq,
                "File Provider projection committed"
            );
        }
        Ok((out, commit))
    }

    /// The snapshot writer: mark and sweep by Dropbox id, then expire every anchor. Runs when the
    /// Dropbox cursor is empty or was reset, and is the only writer besides the clear that expires
    /// anchors. Total, and with no inference: every listed entry is placed by its id alone (no
    /// displacement), whatever was not seen is deleted, and only then is the parent and orphan state of
    /// every remaining row resolved again, because a parent found while placing may be a row the sweep
    /// removed. The result does not depend on the order of the listing. Populates the projection.
    pub(crate) fn project_snapshot(
        &self,
        entries: &[ProjectedEntry],
    ) -> AppResult<ProjectionCommit> {
        if !self.projection_active() {
            return Ok(ProjectionCommit {
                rows: 0,
                seq: 0,
                vanished: 0,
            });
        }
        let (_, commit) = self.write_projection("snapshot", |conn, st| {
            // Read inside the transaction: a Save cannot commit between the read and the writes.
            let filter = load_filter_in(conn)?;
            let mut run = Sequence::new(conn, st, &filter);
            for e in entries {
                run.upsert_by_id(e)?;
            }
            // Invariant: nothing the listing named may be a tombstone.
            let tombstones: Vec<i64> = {
                let mut stmt =
                    conn.prepare_cached("SELECT item_id FROM fp_item WHERE state = 2")?;
                let rows = stmt.query_map([], |r| r.get(0))?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            if tombstones.iter().any(|id| run.seen().contains(id)) {
                return Err(rusqlite::Error::ToSqlConversionFailure(
                    "snapshot left a listed item deleted".into(),
                ));
            }
            let stored: Vec<i64> = {
                let mut stmt = conn.prepare_cached("SELECT item_id FROM fp_item")?;
                let rows = stmt.query_map([], |r| r.get(0))?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            for id in stored.into_iter().filter(|id| !run.seen().contains(id)) {
                conn.prepare_cached("DELETE FROM fp_item WHERE item_id = ?1")?
                    .execute([id])?;
            }
            drop_orphan_pins(conn)?;
            run.reresolve_all()?;
            run.heal_orphans()?;
            run.finish()?;
            expire_anchors(conn, st, true)
        })?;
        Ok(commit)
    }

    /// The delta writer: one invocation's raw entries, folders included. Writes nothing unless the
    /// switch is on and the projection is populated.
    pub(crate) fn project_delta(&self, entries: &[DropboxEntry]) -> AppResult<ProjectionCommit> {
        let none = ProjectionCommit {
            rows: 0,
            seq: 0,
            vanished: 0,
        };
        if !self.projection_active() {
            return Ok(none);
        }
        let ops = ops_from_entries(entries);
        if ops.is_empty() {
            return Ok(none);
        }
        let (_, commit) = self.write_projection("delta", |conn, st| {
            if !populated(conn)? {
                return Ok(());
            }
            let filter = load_filter_in(conn)?;
            apply_steps(conn, st, &filter, ops.iter().map(Op::step))?;
            if st.tombstoned > 0 {
                prune_tombstones(conn, tombstone_cap())?;
            }
            Ok(())
        })?;
        Ok(commit)
    }

    fn health(&self) -> std::sync::MutexGuard<'_, ProjectionHealth> {
        self.projection_health
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// [`Db::project_delta`] for the sync path, which must never be pinned by the projection. A failure
    /// is returned (the cursor stays and the batch replays) up to [`MAX_DELTA_FAILURES`] times in a
    /// row. The next one gives up: the projection is emptied and marked unpopulated (so it reseeds,
    /// with a backoff, instead of patching a base that missed a batch), the failure is logged at ERROR
    /// and `Ok` is returned so the cursor advances. If even the clear fails the error is returned, and
    /// the next attempt tries the clear again.
    pub(crate) fn project_delta_or_give_up(
        &self,
        entries: &[DropboxEntry],
    ) -> AppResult<ProjectionCommit> {
        let error = match self.project_delta(entries) {
            Ok(commit) => {
                self.health().delta_failures = 0;
                return Ok(commit);
            }
            Err(e) => e,
        };
        let failures = {
            let mut health = self.health();
            health.delta_failures = health.delta_failures.saturating_add(1);
            health.delta_failures
        };
        if failures < MAX_DELTA_FAILURES {
            return Err(error);
        }
        tracing::error!(
            failures,
            error = %error,
            "File Provider projection: giving up on the delta; emptying the projection so normal sync \
             goes on, and reseeding it later"
        );
        let commit = self.project_clear()?;
        let mut health = self.health();
        health.delta_failures = 0;
        health.schedule_reseed();
        Ok(commit)
    }

    /// Whether the projection is being fed, is not populated, and is not waiting out a reseed backoff:
    /// the delta cursor must then be treated as absent so a snapshot populates it. Always false with
    /// the switch off, so nothing changes there.
    pub(crate) fn projection_reseed_due(&self) -> AppResult<bool> {
        if !self.projection_active() || self.projection_state()?.populated {
            return Ok(false);
        }
        Ok(reseed_is_due(
            self.health().reseed_not_before,
            SystemTime::now(),
        ))
    }

    /// A reseed was attempted and failed before it reached the projection (the listing itself failed):
    /// back the next one off, so a failing listing is not retried on every iteration of the loop.
    pub(crate) fn schedule_projection_reseed(&self) {
        self.health().schedule_reseed();
    }

    /// A snapshot landed: the projection is whole again and the backoff starts over.
    pub(crate) fn note_snapshot_landed(&self) {
        let mut health = self.health();
        health.reseed_failures = 0;
        health.reseed_not_before = None;
        health.delta_failures = 0;
    }

    /// A snapshot failed. Empties the projection (a populated one must not outlive a cursor it no
    /// longer matches) and backs the next reseed off. The error is returned only when the clear failed
    /// too, which is the one case where the caller must not move the cursor.
    pub(crate) fn note_snapshot_failed(&self) -> AppResult<ProjectionCommit> {
        let commit = self.project_clear()?;
        self.health().schedule_reseed();
        Ok(commit)
    }

    /// Re-evaluates selective sync over every stored item and flips the hidden flag where it no
    /// longer matches, in one transaction and without a snapshot. Called when the filter is saved.
    pub(crate) fn project_reapply_filter(&self) -> AppResult<ProjectionCommit> {
        if !self.projection_active() {
            return Ok(ProjectionCommit {
                rows: 0,
                seq: 0,
                vanished: 0,
            });
        }
        let (_, commit) = self.write_projection("filter", |conn, st| {
            if !populated(conn)? {
                return Ok(());
            }
            let filter = load_filter_in(conn)?;
            let rows: Vec<(i64, String, bool, i64)> = {
                let mut stmt = conn.prepare_cached(
                    "SELECT item_id, path_lower, is_folder, state FROM fp_item WHERE state IN (0, 1)",
                )?;
                let mapped = stmt.query_map([], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0, r.get(3)?))
                })?;
                mapped.collect::<rusqlite::Result<_>>()?
            };
            for (item_id, path, is_folder, state) in rows {
                let wanted = visibility(&filter, &path, is_folder);
                if wanted != state {
                    conn.prepare_cached(
                        "UPDATE fp_item SET state = ?2, changed_seq = ?3 WHERE item_id = ?1",
                    )?
                    .execute(params![item_id, wanted, st.stamp()])?;
                }
            }
            Ok(())
        })?;
        Ok(commit)
    }

    /// What the host needs to download `item_id`. `None`: it is not a live file.
    pub(crate) fn projection_download_target(
        &self,
        item_id: i64,
    ) -> AppResult<Option<ProjectionDownloadTarget>> {
        let conn = self
            .read
            .lock()
            .map_err(|_| AppError::Storage("db read lock poisoned".into()))?;
        let target = conn
            .prepare_cached(
                "SELECT rev, content_hash, content_version, path_lower FROM fp_item
                 WHERE item_id = ?1 AND state = 0 AND is_folder = 0",
            )?
            .query_row([item_id], |r| {
                Ok(ProjectionDownloadTarget {
                    rev: r.get(0)?,
                    content_hash: r.get(1)?,
                    content_version: r.get(2)?,
                    path_lower: r.get(3)?,
                })
            })
            .optional()?;
        Ok(target)
    }

    /// Where a live file sits, for its dropbox.com page: `(path_lower, name)`.
    pub(crate) fn projection_web_location(
        &self,
        item_id: i64,
    ) -> AppResult<Option<(String, String)>> {
        let conn = self
            .read
            .lock()
            .map_err(|_| AppError::Storage("db read lock poisoned".into()))?;
        let location = conn
            .prepare_cached(
                "SELECT path_lower, name FROM fp_item WHERE item_id = ?1 AND state = 0",
            )?
            .query_row([item_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        Ok(location)
    }

    /// What a write needs of a live item: its Dropbox path, built from the names along its parents, and
    /// its versions. `None`: the item is not live, or a parent on the way is not.
    ///
    /// The path comes from the names and not from `path_lower`: a folder renamed through the File
    /// Provider is rewritten at once, but its children's `path_lower` only when the delta's echo lists
    /// them, and a write sent to the old path would recreate the old folder on Dropbox.
    pub(crate) fn projection_write_target(&self, item_id: i64) -> AppResult<Option<WriteTarget>> {
        let conn = self
            .read
            .lock()
            .map_err(|_| AppError::Storage("db read lock poisoned".into()))?;
        let mut names = Vec::new();
        let mut current = item_id;
        let mut target: Option<(bool, String, Vec<u8>, String)> = None;
        // A bound on the walk, in case a cycle ever got stored: no Dropbox tree is this deep.
        for _ in 0..4096 {
            let row: Option<(i64, String, bool, String, Vec<u8>, String)> = conn
                .prepare_cached(
                    "SELECT parent_item, name, is_folder, rev, content_version, content_hash FROM fp_item
                     WHERE item_id = ?1 AND state = 0",
                )?
                .query_row([current], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
                })
                .optional()?;
            let Some((parent, name, is_folder, rev, content_version, content_hash)) = row else {
                return Ok(None);
            };
            if target.is_none() {
                target = Some((is_folder, rev, content_version, content_hash));
            }
            names.push(name);
            if parent == 0 {
                let (is_folder, rev, content_version, content_hash) =
                    target.expect("set on the first row");
                names.reverse();
                return Ok(Some(WriteTarget {
                    path: format!("/{}", names.join("/")),
                    is_folder,
                    rev,
                    content_version,
                    content_hash,
                }));
            }
            current = parent;
        }
        Ok(None)
    }

    fn read_conn(&self) -> AppResult<std::sync::MutexGuard<'_, Connection>> {
        self.read
            .lock()
            .map_err(|_| AppError::Storage("db read lock poisoned".into()))
    }

    /// The state of a row, whatever it is; `None` for an id with no row.
    pub(crate) fn projection_row_state(&self, item_id: i64) -> AppResult<Option<i64>> {
        let conn = self.read_conn()?;
        let value = conn
            .prepare_cached("SELECT state FROM fp_item WHERE item_id = ?1")?
            .query_row([item_id], |r| r.get(0))
            .optional()?;
        Ok(value)
    }

    /// The stored `path_lower` of a row, whatever its state.
    pub(crate) fn projection_path_lower(&self, item_id: i64) -> AppResult<Option<String>> {
        let conn = self.read_conn()?;
        let value = conn
            .prepare_cached("SELECT path_lower FROM fp_item WHERE item_id = ?1")?
            .query_row([item_id], |r| r.get(0))
            .optional()?;
        Ok(value)
    }

    /// The live child of `parent_item` with this name, ignoring case as Dropbox does: its id, whether it
    /// is a folder, and its `content_hash`.
    pub(crate) fn projection_child(
        &self,
        parent_item: i64,
        name: &str,
    ) -> AppResult<Option<(i64, bool, String)>> {
        let conn = self.read_conn()?;
        let wanted = name.to_lowercase();
        let mut stmt = conn.prepare_cached(
            "SELECT item_id, name, is_folder, content_hash FROM fp_item WHERE parent_item = ?1 AND state = 0",
        )?;
        let rows = stmt.query_map([parent_item], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, bool>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (id, child, is_folder, hash) = row?;
            if child.to_lowercase() == wanted {
                return Ok(Some((id, is_folder, hash)));
            }
        }
        Ok(None)
    }

    /// Whether selective sync would hide an item written at `path` (any case): it would not show in
    /// Finder once recorded.
    pub(crate) fn projection_would_hide(&self, path: &str, is_folder: bool) -> AppResult<bool> {
        let conn = self.read_conn()?;
        let filter = load_filter_in(&conn)?;
        Ok(visibility(&filter, &path.to_lowercase(), is_folder) != STATE_LIVE)
    }

    /// Whether anything under a folder is hidden by selective sync or waiting as an orphan: content
    /// Finder never showed, which a recursive delete on Dropbox would take too.
    pub(crate) fn projection_folder_hides_content(&self, item_id: i64) -> AppResult<bool> {
        let conn = self.read_conn()?;
        let path: Option<String> = conn
            .prepare_cached(
                "SELECT path_lower FROM fp_item WHERE item_id = ?1 AND state = 0 AND is_folder = 1",
            )?
            .query_row([item_id], |r| r.get(0))
            .optional()?;
        let Some(path) = path else {
            return Ok(false);
        };
        let hidden: i64 = conn.query_row(
            "SELECT COUNT(*) FROM fp_item WHERE state IN (1, 3) AND path_lower >= ?1 AND path_lower < ?2",
            [format!("{path}/"), format!("{path}0")],
            |r| r.get(0),
        )?;
        Ok(hidden > 0)
    }

    /// Whether the folder, or anything under it, changed after `seq`: a child added, edited, moved in or
    /// deleted on Dropbox, at any visibility. A folder delete is refused on it, because Finder has not
    /// shown that change and Dropbox would take it with the folder (DBSYNC-122). `None` when the item is
    /// not a live folder: a folder already deleted or hidden is answered elsewhere, as before.
    pub(crate) fn projection_folder_changed_since(
        &self,
        item_id: i64,
        seq: i64,
    ) -> AppResult<Option<bool>> {
        let conn = self.read_conn()?;
        let folder: Option<(String, i64)> = conn
            .prepare_cached(
                "SELECT path_lower, changed_seq FROM fp_item WHERE item_id = ?1 AND is_folder = 1 AND state = 0",
            )?
            .query_row([item_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let Some((path, own)) = folder else {
            return Ok(None);
        };
        if own > seq {
            return Ok(Some(true));
        }
        let newer: i64 = conn.query_row(
            "SELECT COUNT(*) FROM fp_item WHERE path_lower >= ?1 AND path_lower < ?2 AND changed_seq > ?3",
            params![format!("{path}/"), format!("{path}0"), seq],
            |r| r.get(0),
        )?;
        Ok(Some(newer > 0))
    }

    /// After a folder moved here (a rename or move made in Finder), the rows under its old path take the
    /// new one. Only `path_lower` changes: it is bookkeeping the extension never sees, and the parents
    /// already follow the folder. Without it, a delete or a sweep that matches by path misses them until
    /// the delta echoes the move.
    pub(crate) fn projection_move_subtree(&self, item_id: i64, old_lower: &str) -> AppResult<()> {
        self.write_projection("move", |conn, _st| {
            let new_lower: Option<String> = conn
                .prepare_cached("SELECT path_lower FROM fp_item WHERE item_id = ?1 AND state = 0")?
                .query_row([item_id], |r| r.get(0))
                .optional()?;
            let Some(new_lower) = new_lower.filter(|n| n != old_lower) else {
                return Ok(());
            };
            conn.execute(
                "UPDATE fp_item SET path_lower = ?2 || substr(path_lower, length(?1) + 1)
                 WHERE state != 2 AND path_lower >= ?1 || '/' AND path_lower < ?1 || '0'",
                params![old_lower, new_lower],
            )?;
            Ok(())
        })?;
        Ok(())
    }

    /// The Dropbox path of a live folder, for a child created in it: `""` for the domain root.
    /// `None`: no live folder has this id.
    pub(crate) fn projection_folder_path(&self, item_id: i64) -> AppResult<Option<String>> {
        if item_id == 0 {
            return Ok(Some(String::new()));
        }
        Ok(self
            .projection_write_target(item_id)?
            .filter(|t| t.is_folder)
            .map(|t| t.path))
    }

    /// What deleting a live item weighs for the mass-deletion breaker, and against what: the live files it
    /// takes with it (1 for a file, those under a folder), and every live file of the projection.
    pub(crate) fn projection_delete_weight(&self, item_id: i64) -> AppResult<(usize, usize)> {
        let conn = self
            .read
            .lock()
            .map_err(|_| AppError::Storage("db read lock poisoned".into()))?;
        let tracked: i64 = conn.query_row(
            "SELECT COUNT(*) FROM fp_item WHERE state = 0 AND is_folder = 0",
            [],
            |r| r.get(0),
        )?;
        let row: Option<(bool, String)> = conn
            .prepare_cached(
                "SELECT is_folder, path_lower FROM fp_item WHERE item_id = ?1 AND state = 0",
            )?
            .query_row([item_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let weight = match row {
            None => 0,
            Some((false, _)) => 1,
            Some((true, path)) => {
                let under: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM fp_item
                     WHERE state = 0 AND is_folder = 0 AND substr(path_lower, 1, length(?1) + 1) = ?1 || '/'",
                    [&path],
                    |r| r.get(0),
                )?;
                // An empty folder still counts as something deleted.
                usize::try_from(under).unwrap_or(0).max(1)
            }
        };
        Ok((weight, usize::try_from(tracked).unwrap_or(0)))
    }

    /// The item a Dropbox id was given, live or not.
    pub(crate) fn projection_item_for_dropbox_id(
        &self,
        dropbox_id: &str,
    ) -> AppResult<Option<i64>> {
        let conn = self
            .read
            .lock()
            .map_err(|_| AppError::Storage("db read lock poisoned".into()))?;
        let id = conn
            .prepare_cached("SELECT item_id FROM fp_identity WHERE dropbox_id = ?1")?
            .query_row([dropbox_id], |r| r.get(0))
            .optional()?;
        Ok(id)
    }

    /// Test hook: the id of the row at `path_lower`.
    #[cfg(test)]
    pub(crate) fn projection_item_id(&self, path_lower: &str) -> Option<i64> {
        let conn = self.read.lock().unwrap();
        conn.query_row(
            "SELECT item_id FROM fp_item WHERE path_lower = ?1",
            [path_lower],
            |r| r.get(0),
        )
        .optional()
        .unwrap()
    }

    /// Pins or unpins a live item: *Make Available Offline* and *Make Online-Only* (#237). A pinned
    /// item is listed with the content policy that keeps it downloaded, and what is under a pinned folder
    /// is listed as inheriting it. Unpinning takes every pin under the item with it, so a folder made
    /// online-only can be evicted whole. When anything changed, the item and every live row under it take
    /// a new sequence value, so the system reads them again with what they now inherit. `false`: no live
    /// row, and nothing changed.
    pub(crate) fn set_projection_pinned(&self, item_id: i64, pinned: bool) -> AppResult<bool> {
        let (live, _) = self.write_projection("pin", |conn, st| {
            let state: Option<i64> = conn
                .prepare_cached("SELECT state FROM fp_item WHERE item_id = ?1")?
                .query_row([item_id], |r| r.get(0))
                .optional()?;
            if state != Some(STATE_LIVE) {
                return Ok(false);
            }
            // The item and every row under it, whatever their state.
            let subtree: Vec<(i64, i64)> = {
                let mut stmt = conn.prepare_cached(
                    "WITH RECURSIVE tree(id) AS (
                         SELECT ?1
                         UNION
                         SELECT i.item_id FROM fp_item i JOIN tree ON i.parent_item = tree.id
                     )
                     SELECT i.item_id, i.state FROM fp_item i JOIN tree ON i.item_id = tree.id
                     ORDER BY i.item_id",
                )?;
                let rows = stmt.query_map([item_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            let changed = if pinned {
                conn.prepare_cached("INSERT OR IGNORE INTO fp_pin(item_id) VALUES (?1)")?
                    .execute([item_id])?
            } else {
                let mut removed = 0;
                for (id, _) in &subtree {
                    removed += conn
                        .prepare_cached("DELETE FROM fp_pin WHERE item_id = ?1")?
                        .execute([id])?;
                }
                removed
            };
            if changed > 0 {
                for (id, row_state) in &subtree {
                    if *row_state == STATE_LIVE {
                        conn.prepare_cached(
                            "UPDATE fp_item SET changed_seq = ?2 WHERE item_id = ?1",
                        )?
                        .execute(params![id, st.stamp()])?;
                    }
                }
            }
            Ok(true)
        })?;
        Ok(live)
    }

    /// The live files a commit stamped that are pinned, themselves or through a folder above them, each
    /// with the pin that covers it and its path relative to the domain root: what a delta brought that a
    /// pin has to download now (#237), a new revision or a file added under a pinned folder. `seq` and
    /// `rows` are the commit's: its values are the `rows` up to `seq`, taken in one transaction.
    pub(crate) fn projection_pinned_files_stamped(
        &self,
        seq: i64,
        rows: usize,
    ) -> AppResult<Vec<(i64, String)>> {
        let conn = self.read_conn()?;
        let pinned: HashSet<i64> = {
            let mut stmt = conn.prepare_cached("SELECT item_id FROM fp_pin")?;
            let ids = stmt.query_map([], |r| r.get(0))?;
            ids.collect::<rusqlite::Result<_>>()?
        };
        if pinned.is_empty() || rows == 0 {
            return Ok(Vec::new());
        }
        let first = seq - i64::try_from(rows).unwrap_or(i64::MAX) + 1;
        let stamped: Vec<(i64, i64, String)> = {
            let mut stmt = conn.prepare_cached(
                "SELECT item_id, parent_item, name FROM fp_item
                 WHERE changed_seq BETWEEN ?1 AND ?2 AND state = 0 AND is_folder = 0
                 ORDER BY item_id",
            )?;
            let found = stmt.query_map(params![first, seq], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?;
            found.collect::<rusqlite::Result<_>>()?
        };
        let mut out = Vec::new();
        for (item_id, parent, name) in stamped {
            // Up to the root: the nearest pin, and the names on the way, which must all be live.
            let mut names = vec![name];
            let mut owner = pinned.contains(&item_id).then_some(item_id);
            let mut at = parent;
            let mut reachable = true;
            while at != ROOT_ITEM {
                let row: Option<(i64, String, i64)> = conn
                    .prepare_cached(
                        "SELECT parent_item, name, state FROM fp_item WHERE item_id = ?1",
                    )?
                    .query_row([at], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                    .optional()?;
                match row {
                    Some((up, folder, STATE_LIVE)) if names.len() < 4096 => {
                        if owner.is_none() && pinned.contains(&at) {
                            owner = Some(at);
                        }
                        names.push(folder);
                        at = up;
                    }
                    _ => {
                        reachable = false;
                        break;
                    }
                }
            }
            if let (true, Some(owner)) = (reachable, owner) {
                names.reverse();
                out.push((owner, names.join("/")));
            }
        }
        Ok(out)
    }

    /// The live files at or under a live item, as paths relative to the domain root built from the rows'
    /// names, which keep Dropbox's case: what pinning the item has to download (#237). Empty when the
    /// item, or a folder above it, is not live.
    pub(crate) fn projection_live_files_under(&self, item_id: i64) -> AppResult<Vec<String>> {
        let conn = self.read_conn()?;
        let mut names = Vec::new();
        let mut at = item_id;
        while at != ROOT_ITEM {
            let row: Option<(i64, String, i64)> = conn
                .prepare_cached("SELECT parent_item, name, state FROM fp_item WHERE item_id = ?1")?
                .query_row([at], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .optional()?;
            match row {
                Some((parent, name, STATE_LIVE)) if names.len() < 4096 => {
                    names.push(name);
                    at = parent;
                }
                _ => return Ok(Vec::new()),
            }
        }
        names.reverse();
        let mut stmt = conn.prepare_cached(
            "WITH RECURSIVE tree(id, rel, folder) AS (
                 SELECT item_id, ?2, is_folder FROM fp_item WHERE item_id = ?1 AND state = 0
                 UNION ALL
                 SELECT i.item_id, tree.rel || '/' || i.name, i.is_folder
                 FROM fp_item i JOIN tree ON i.parent_item = tree.id
                 WHERE tree.folder = 1 AND i.state = 0
             )
             SELECT rel FROM tree WHERE folder = 0 ORDER BY rel",
        )?;
        let rows = stmt.query_map(params![item_id, names.join("/")], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Whether an item is pinned itself (not through a folder above it).
    pub(crate) fn projection_pinned(&self, item_id: i64) -> AppResult<bool> {
        let conn = self.read_conn()?;
        let pinned = conn
            .prepare_cached("SELECT EXISTS (SELECT 1 FROM fp_pin WHERE item_id = ?1)")?
            .query_row([item_id], |r| r.get(0))?;
        Ok(pinned)
    }

    /// Records whether the local copy of a file matches the version the projection holds. Nothing calls
    /// it: a replicated extension's `isDownloaded` is ignored, and the system keeps what is downloaded
    /// itself, which is what Finder shows (#237).
    #[allow(dead_code)]
    pub(crate) fn set_projection_downloaded(
        &self,
        dropbox_id: &str,
        downloaded: bool,
    ) -> AppResult<ProjectionCommit> {
        if !self.projection_active() {
            return Ok(ProjectionCommit {
                rows: 0,
                seq: 0,
                vanished: 0,
            });
        }
        let (_, commit) = self.write_projection("downloaded", |conn, st| {
            if !populated(conn)? {
                return Ok(());
            }
            let current: Option<(i64, bool)> = conn
                .prepare_cached(
                    "SELECT item_id, downloaded FROM fp_item
                     WHERE item_id = (SELECT item_id FROM fp_identity WHERE dropbox_id = ?1)",
                )?
                .query_row([dropbox_id], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?;
            if let Some((item_id, was)) = current {
                if was != downloaded {
                    conn.prepare_cached(
                        "UPDATE fp_item SET downloaded = ?2, changed_seq = ?3 WHERE item_id = ?1",
                    )?
                    .execute(params![item_id, downloaded, st.stamp()])?;
                }
            }
            Ok(())
        })?;
        Ok(commit)
    }

    /// Test hook: `(path_lower, state)` of every stored item, for a test outside this module.
    #[cfg(test)]
    pub(crate) fn projection_states_by_path(&self) -> Vec<(String, i64)> {
        let conn = self.read.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT path_lower, state FROM fp_item ORDER BY path_lower")
            .unwrap();
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        rows.map(Result::unwrap).collect()
    }

    /// Test hook: ends a reseed backoff now, so a test need not wait out the real minutes.
    #[cfg(test)]
    pub(crate) fn end_projection_reseed_backoff(&self) {
        self.health().reseed_not_before = None;
    }

    /// Test hook: makes every later write of an `fp_item` row named `name` fail, so another module's
    /// test can force a projection failure without reaching the private connections.
    #[cfg(test)]
    pub(crate) fn make_projection_writes_fail_for(&self, name: &str) {
        let conn = self.write.lock().unwrap();
        conn.execute_batch(&format!(
            "CREATE TRIGGER sabotage_ins BEFORE INSERT ON fp_item WHEN NEW.name = '{name}'
               BEGIN SELECT RAISE(ABORT, 'sabotaged'); END;"
        ))
        .unwrap();
    }

    /// Empties the projection and marks it unpopulated, if it holds anything (the switch went off).
    pub(crate) fn project_clear(&self) -> AppResult<ProjectionCommit> {
        let (_, commit) = self.write_projection("clear", clear_with)?;
        Ok(commit)
    }
}

fn populated(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row("SELECT populated FROM fp_state WHERE id = 1", [], |r| {
        r.get(0)
    })
}

/// The selective-sync filter, read through the writer's own connection. Called inside the IMMEDIATE
/// transaction: every Save goes through the same write lock, so what is read here cannot change before
/// the rows it decides are committed, and a Save that lands first is the one the flags follow.
/// [`PrefixFilter::load`] would read through the pool and could be one Save behind.
fn load_filter_in(conn: &Connection) -> rusqlite::Result<PrefixFilter> {
    let get = |key: &str| -> rusqlite::Result<Option<String>> {
        conn.query_row(
            "SELECT value FROM app_config WHERE key = ?1 LIMIT 1",
            [key],
            |r| r.get(0),
        )
        .optional()
    };
    Ok(PrefixFilter::new(
        &parse_prefix_csv(get("include_prefixes_csv")?),
        &parse_prefix_csv(get("exclude_prefixes_csv")?),
    ))
}

/// Tombstones kept before the oldest are dropped. A tombstone is what lets an anchor older than the
/// deletion learn of it, and with one global `min_valid_seq` every anchor at or above it may still need
/// every tombstone after it: none can be dropped for free. Past this many the oldest go and
/// `min_valid_seq` rises to the newest one dropped, which expires exactly the anchors that could have
/// needed them (the system re-enumerates, as after any expiry). 50 000 rows is about 20 MB and more
/// than a whole account's worth of deletions between two snapshots in ordinary use.
const TOMBSTONE_CAP: usize = 50_000;

fn tombstone_cap() -> usize {
    #[cfg(test)]
    if let Some(cap) = TOMBSTONE_CAP_OVERRIDE.with(std::cell::Cell::get) {
        return cap;
    }
    TOMBSTONE_CAP
}

/// Drops the oldest tombstones beyond `cap` and raises `min_valid_seq` to the newest one dropped, so
/// no valid anchor can still need a deletion that is gone: an anchor `S` needs a tombstone stamped `t`
/// only if `S < t`, and after the raise every valid `S` is at least the largest dropped `t`. The third
/// place anchors expire (see the module docs). Returns the number of rows dropped.
fn prune_tombstones(conn: &Connection, cap: usize) -> rusqlite::Result<usize> {
    let newest_dropped: Option<i64> = conn
        .query_row(
            "SELECT changed_seq FROM fp_item WHERE state = 2
             ORDER BY changed_seq DESC LIMIT 1 OFFSET ?1",
            [i64::try_from(cap).unwrap_or(i64::MAX)],
            |r| r.get(0),
        )
        .optional()?;
    let Some(bound) = newest_dropped else {
        return Ok(0);
    };
    let dropped = conn.execute(
        "DELETE FROM fp_item WHERE state = 2 AND changed_seq <= ?1",
        [bound],
    )?;
    drop_orphan_pins(conn)?;
    conn.execute(
        "UPDATE fp_state SET min_valid_seq = MAX(min_valid_seq, ?1) WHERE id = 1",
        [bound],
    )?;
    Ok(dropped)
}

#[cfg(test)]
thread_local! {
    static TOMBSTONE_CAP_OVERRIDE: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    #[allow(clippy::type_complexity)]
    static BEFORE_TX_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce(&Db)>>> =
        const { std::cell::RefCell::new(None) };
}

/// Runs (once) the closure a test installed, right before a writer takes its transaction.
#[cfg(test)]
fn run_before_tx_hook(db: &Db) {
    if let Some(hook) = BEFORE_TX_HOOK.with(|h| h.borrow_mut().take()) {
        hook(db);
    }
}

// ── keeping the projection from pinning normal sync ─────────────────────────────────────────────

/// Consecutive delta failures tolerated before the projection is given up on. Each failure already
/// replays after the long-poll loop's error backoff (5, 10, 20, 40 s), so the fifth attempt is about
/// 75 seconds after the first: long enough for a lock, a full disk being cleaned or a second launch
/// to pass, short enough that normal sync is never held behind the projection for more than a couple
/// of minutes.
pub(crate) const MAX_DELTA_FAILURES: u32 = 5;
/// The first wait before a reseed that follows a give-up or a failed snapshot; it doubles each time
/// up to [`RESEED_BACKOFF_MAX`]. A reseed is a full listing of the account, so it must not repeat fast.
const RESEED_BACKOFF_MIN: Duration = Duration::from_secs(60);
const RESEED_BACKOFF_MAX: Duration = Duration::from_secs(30 * 60);

/// In-memory health of the projection feed. Deliberately not persisted: a relaunch is itself a retry
/// (`apply_switch` reseeds an unpopulated projection at startup).
#[derive(Debug, Default)]
pub(crate) struct ProjectionHealth {
    delta_failures: u32,
    reseed_failures: u32,
    /// Wall-clock time: `Instant` does not advance while the Mac sleeps, so a backoff set before a
    /// night's sleep would still be pending the next morning.
    reseed_not_before: Option<SystemTime>,
}

/// Whether a reseed may run at `now`. No backoff: due. A backoff that has passed: due. A backoff that
/// is further away than the longest one can ever be means the clock was set back, and the deadline is
/// meaningless: due as well, so a wrong clock cannot hold the projection off for hours.
fn reseed_is_due(not_before: Option<SystemTime>, now: SystemTime) -> bool {
    match not_before {
        None => true,
        Some(t) => match now.duration_since(t) {
            // The deadline is now or past.
            Ok(_) => true,
            // The deadline is ahead by this much; more than any backoff can be means a clock set back.
            Err(ahead) => ahead.duration() > RESEED_BACKOFF_MAX,
        },
    }
}

impl ProjectionHealth {
    fn schedule_reseed(&mut self) {
        let wait = RESEED_BACKOFF_MIN
            .checked_mul(1u32.checked_shl(self.reseed_failures).unwrap_or(u32::MAX))
            .map_or(RESEED_BACKOFF_MAX, |w| w.min(RESEED_BACKOFF_MAX));
        self.reseed_failures = self.reseed_failures.saturating_add(1);
        self.reseed_not_before = Some(SystemTime::now() + wait);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    use std::time::Instant;

    use super::*;
    use crate::models::DropboxListFolderResponse;
    use crate::remote_index::{
        cursor_or_seed, finish_seed, run_remote_delta, usable_delta_cursor, DeltaFetchOutcome,
        DeltaRunOutcome, REMOTE_DELTA_CURSOR_KEY,
    };
    use crate::state::AppState;

    // Shaped like the live capture on #234, with synthetic ids and revs (the repository is public and
    // the real ones belong to an account). What matters is the shape: a `deleted` entry carries a path
    // and nothing else, a move changes the rev and keeps the id and the hash.
    const FCAP: &str = "id:t00000000000FCAPROOT01";
    const SUB: &str = "id:t00000000000FCAPSUB002";
    const B: &str = "id:t00000000000FCAPFILE03";
    const HASH_B: &str = "5f1c0d3a9b7e2c44a1d8e6f03b9a7c5d2e4f6a8b0c1d3e5f7a9b1c3d5e7f9a0b";
    const HASH_B_OTHER: &str = "6f1c0d3a9b7e2c44a1d8e6f03b9a7c5d2e4f6a8b0c1d3e5f7a9b1c3d5e7f9a0b";

    struct World {
        state: AppState,
        _dir: tempfile::TempDir,
    }

    impl World {
        fn db(&self) -> &Db {
            &self.state.db
        }
    }

    /// A database with the switch ON and the projection populated (an empty snapshot), unless
    /// `populated` is false. Nothing here touches FileProvider, the keychain, the network or the real
    /// defaults: the switch is a flag on this `Db` instance.
    fn world(switch_on: bool, populated: bool) -> World {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::new_at(&dir.path().join("app.db")).expect("db");
        db.set_projection_enabled(switch_on);
        let state = AppState {
            secure_store: crate::storage::secure_store::SecureStore::new(),
            db: Arc::new(db),
            sync_engine: Arc::new(Mutex::new(crate::sync::engine::SyncEngine::new())),
            token_cache: Arc::new(Mutex::new(None)),
            scheduler_started: Arc::new(Mutex::new(false)),
            oauth_listener: Arc::new(Mutex::new(None)),
            sync_running: Arc::new(AtomicBool::new(false)),
            token_refresh_lock: Arc::new(Mutex::new(())),
            http_client: crate::state::build_http_client(),
        };
        let w = World { state, _dir: dir };
        if populated {
            // Through the production seed path (`finish_seed`: reconcile, project, persist the
            // cursor), not a bare `project_snapshot(&[])`.
            seed(&w, Vec::new()).expect("populate");
        }
        w
    }

    /// A snapshot as the seed builds it: the raw listing becomes projected entries exactly as
    /// `seed_remote_delta_cursor` collects them, then `finish_seed` projects them and persists the
    /// cursor. Returns the seed's result, so a test can see a failure.
    fn seed(w: &World, listing: Vec<DropboxEntry>) -> crate::error::AppResult<()> {
        let entries: Vec<ProjectedEntry> = listing
            .iter()
            .filter_map(ProjectedEntry::from_entry)
            .collect();
        finish_seed(&w.state, &HashMap::new(), "cursor-seeded", Some(entries))
    }

    fn entry(tag: &str, display: &str) -> DropboxEntry {
        DropboxEntry {
            tag: tag.to_string(),
            path_display: Some(display.to_string()),
            path_lower: Some(display.to_lowercase()),
            content_hash: None,
            rev: None,
            server_modified: None,
            size: None,
            id: None,
        }
    }

    fn dir(display: &str, id: &str) -> DropboxEntry {
        DropboxEntry {
            id: Some(id.to_string()),
            ..entry("folder", display)
        }
    }

    fn file_with(display: &str, id: &str, rev: &str, hash: &str) -> DropboxEntry {
        DropboxEntry {
            id: Some(id.to_string()),
            rev: Some(rev.to_string()),
            content_hash: Some(hash.to_string()),
            server_modified: Some("2026-11-01T10:00:00Z".to_string()),
            size: Some(5),
            ..entry("file", display)
        }
    }

    fn file(display: &str, id: &str, rev: &str) -> DropboxEntry {
        file_with(display, id, rev, HASH_B)
    }

    /// A `deleted` entry as Dropbox sends it: the path, no id, no rev.
    fn gone(display: &str) -> DropboxEntry {
        entry("deleted", display)
    }

    // The nine invocations of the capture that matter, in the order they were observed.
    fn inv1() -> Vec<DropboxEntry> {
        vec![dir("/FCAP", FCAP)]
    }
    fn inv2() -> Vec<DropboxEntry> {
        vec![dir("/FCAP/sub", SUB)]
    }
    fn inv3() -> Vec<DropboxEntry> {
        vec![file("/FCAP/sub/b.txt", B, "rev-aaa1")]
    }
    /// The folder rename `sub` to `sub2`: the old child, then the old folder, then both re-added.
    fn inv4() -> Vec<DropboxEntry> {
        vec![
            gone("/FCAP/sub/b.txt"),
            gone("/FCAP/sub"),
            dir("/FCAP/sub2", SUB),
            file("/FCAP/sub2/b.txt", B, "rev-bbb2"),
        ]
    }
    /// The file move `sub2/b.txt` to `FCAP/b.txt`: a new rev, the same bytes.
    fn inv6() -> Vec<DropboxEntry> {
        vec![gone("/FCAP/sub2/b.txt"), file("/FCAP/b.txt", B, "rev-ccc3")]
    }
    fn inv8() -> Vec<DropboxEntry> {
        vec![gone("/FCAP/sub2")]
    }
    fn inv9() -> Vec<DropboxEntry> {
        vec![gone("/FCAP/b.txt"), gone("/FCAP")]
    }

    fn try_run(w: &World, entries: Vec<DropboxEntry>) -> crate::error::AppResult<DeltaRunOutcome> {
        let mut page = Some(DropboxListFolderResponse {
            entries,
            cursor: "cursor-after".to_string(),
            has_more: false,
        });
        run_remote_delta(&w.state, "cursor-before", move |_| {
            Ok(DeltaFetchOutcome::Page(
                page.take().expect("the fake source serves one page"),
            ))
        })
    }

    fn run(w: &World, entries: Vec<DropboxEntry>) {
        try_run(w, entries).expect("run_remote_delta");
    }

    #[derive(Debug, Clone, PartialEq)]
    struct R {
        item_id: i64,
        dropbox_id: String,
        parent: i64,
        path: String,
        name: String,
        folder: bool,
        state: i64,
        seq: i64,
        content_hash: String,
        rev: String,
        cv: Vec<u8>,
        mv: Vec<u8>,
        downloaded: bool,
    }

    fn all_rows(db: &Db) -> Vec<R> {
        let conn = db.read.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT i.item_id, d.dropbox_id, i.parent_item, i.path_lower, i.name, i.is_folder,
                        i.state, i.changed_seq, i.content_hash, i.rev, i.content_version,
                        i.meta_version, i.downloaded
                 FROM fp_item i JOIN fp_identity d ON d.item_id = i.item_id
                 ORDER BY i.path_lower, i.item_id",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |r| {
                Ok(R {
                    item_id: r.get(0)?,
                    dropbox_id: r.get(1)?,
                    parent: r.get(2)?,
                    path: r.get(3)?,
                    name: r.get(4)?,
                    folder: r.get::<_, i64>(5)? != 0,
                    state: r.get(6)?,
                    seq: r.get(7)?,
                    content_hash: r.get(8)?,
                    rev: r.get(9)?,
                    cv: r.get(10)?,
                    mv: r.get(11)?,
                    downloaded: r.get::<_, i64>(12)? != 0,
                })
            })
            .unwrap();
        rows.map(Result::unwrap).collect()
    }

    fn row(db: &Db, dropbox_id: &str) -> R {
        all_rows(db)
            .into_iter()
            .find(|r| r.dropbox_id == dropbox_id)
            .unwrap_or_else(|| panic!("no row for {dropbox_id}"))
    }

    fn count(db: &Db, table: &str) -> i64 {
        db.read
            .lock()
            .unwrap()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    fn seq(db: &Db) -> i64 {
        db.projection_state().unwrap().seq
    }

    fn states(db: &Db) -> Vec<(String, i64)> {
        all_rows(db)
            .into_iter()
            .map(|r| (r.path, r.state))
            .collect()
    }

    /// A trigger that makes any insert or update of a row named `name` abort, through the writer
    /// connection, so a test can force a failure partway through a batch.
    fn sabotage(db: &Db, name: &str) {
        let conn = db.write.lock().unwrap();
        conn.execute_batch(&format!(
            "CREATE TRIGGER sabotage_ins BEFORE INSERT ON fp_item WHEN NEW.name = '{name}'
               BEGIN SELECT RAISE(ABORT, 'sabotaged'); END;
             CREATE TRIGGER sabotage_upd BEFORE UPDATE ON fp_item WHEN NEW.name = '{name}'
               BEGIN SELECT RAISE(ABORT, 'sabotaged'); END;"
        ))
        .unwrap();
    }

    fn unsabotage(db: &Db) {
        let conn = db.write.lock().unwrap();
        conn.execute_batch("DROP TRIGGER sabotage_ins; DROP TRIGGER sabotage_upd;")
            .unwrap();
    }

    // ── the capture, through run_remote_delta ───────────────────────────────────────────────────

    /// Invocations 1 to 3: a nested create. Each item is stored under the id of its parent folder, and
    /// a deleted-free batch is the simple case the others build on.
    ///
    /// Proof this test can fail: resolving every parent to `ROOT_ITEM` in `resolve_parent` leaves
    /// `sub` and `b.txt` under the root and fails the parent assertions.
    #[test]
    fn a_nested_create_stores_each_item_under_its_parent() {
        let w = world(true, true);
        run(&w, inv1());
        run(&w, inv2());
        run(&w, inv3());

        let (fcap, sub, b) = (row(w.db(), FCAP), row(w.db(), SUB), row(w.db(), B));
        assert_eq!(fcap.parent, ROOT_ITEM);
        assert_eq!(sub.parent, fcap.item_id);
        assert_eq!(b.parent, sub.item_id);
        assert_eq!(
            (fcap.name.as_str(), sub.name.as_str(), b.name.as_str()),
            ("FCAP", "sub", "b.txt")
        );
        assert_eq!(b.path, "/fcap/sub/b.txt");
        assert!(fcap.folder && sub.folder && !b.folder);
        assert!(all_rows(w.db()).iter().all(|r| r.state == STATE_LIVE));
        let seqs: HashSet<i64> = all_rows(w.db()).iter().map(|r| r.seq).collect();
        assert_eq!(seqs.len(), 3, "each item took its own sequence value");
    }

    /// Invocation 4, exactly as captured: deletes first, then the folder at its new path with the
    /// SAME id, then the child with its same id. One folder row moves, the child keeps its id, and
    /// nothing is left as a tombstone.
    ///
    /// Proof this test can fail: applying the deletes before the upserts (a batch-level reordering)
    /// or never updating a tombstoned id in place leaves the child or the folder deleted; never updating an existing
    /// id in place breaks the "same ids" assertions.
    #[test]
    fn a_folder_rename_moves_one_folder_row_and_the_child_keeps_its_id() {
        let w = world(true, true);
        run(&w, inv1());
        run(&w, inv2());
        run(&w, inv3());
        let before = all_rows(w.db());
        let (fcap0, sub0, b0) = (row(w.db(), FCAP), row(w.db(), SUB), row(w.db(), B));
        let identities = count(w.db(), "fp_identity");
        let seq_before = seq(w.db());

        run(&w, inv4());

        assert_eq!(
            seq(w.db()) - seq_before,
            2,
            "two observable changes (the folder's name, the child's rev) and no tombstone churn"
        );
        assert_eq!(all_rows(w.db()).len(), before.len(), "no row added or lost");
        assert_eq!(count(w.db(), "fp_identity"), identities, "no new identity");
        let (fcap, sub, b) = (row(w.db(), FCAP), row(w.db(), SUB), row(w.db(), B));
        assert_eq!(sub.item_id, sub0.item_id, "the folder keeps its id");
        assert_eq!(b.item_id, b0.item_id, "the child keeps its id");
        assert_eq!(
            (sub.name.as_str(), sub.path.as_str()),
            ("sub2", "/fcap/sub2")
        );
        assert_eq!(sub.state, STATE_LIVE);
        assert_eq!(
            b.parent, sub.item_id,
            "the child still points at the folder's id"
        );
        assert_eq!(b.path, "/fcap/sub2/b.txt");
        assert_eq!(b.state, STATE_LIVE);
        assert_eq!(fcap, fcap0, "the parent was not touched, not even stamped");
        assert!(sub.seq > sub0.seq, "the renamed folder was stamped");
        assert!(
            all_rows(w.db()).iter().all(|r| r.state != STATE_DELETED),
            "a rename leaves no tombstone"
        );
    }

    /// Invocation 6: a move changes the rev and nothing about the bytes. The content version must not
    /// move with it; the metadata version and the parent do.
    ///
    /// Proof this test can fail: adding `rev` to the `content_version` digest changes it here.
    #[test]
    fn a_move_changes_the_rev_and_never_the_content_version() {
        let w = world(true, true);
        for e in [inv1(), inv2(), inv3(), inv4()] {
            run(&w, e);
        }
        let (fcap, before) = (row(w.db(), FCAP), row(w.db(), B));

        run(&w, inv6());

        let after = row(w.db(), B);
        assert_eq!(after.item_id, before.item_id);
        assert_eq!(after.parent, fcap.item_id, "moved up into FCAP");
        assert_eq!(after.path, "/fcap/b.txt");
        assert_ne!(after.rev, before.rev);
        assert_eq!(after.cv, before.cv, "same bytes, same content version");
        assert_ne!(after.mv, before.mv, "a move is a metadata change");
        assert!(after.seq > before.seq);
    }

    /// Invocations 8 and 9: an empty folder is one entry, a folder with content lists the child's
    /// delete before its own. Everything ends as a tombstone and every identity survives.
    ///
    /// Proof this test can fail: skipping `tombstone` for a candidate whose parent is also a candidate
    /// leaves the child live.
    #[test]
    fn folder_deletes_tombstone_everything_and_keep_the_identities() {
        let w = world(true, true);
        for e in [inv1(), inv2(), inv3(), inv4(), inv6()] {
            run(&w, e);
        }
        let identities = count(w.db(), "fp_identity");

        let (seq_before, sub_before) = (seq(w.db()), row(w.db(), SUB).seq);
        run(&w, inv8());
        assert_eq!(row(w.db(), SUB).state, STATE_DELETED, "the empty folder");
        assert!(
            seq(w.db()) > seq_before,
            "a tombstone is a change the extension must see"
        );
        assert!(row(w.db(), SUB).seq > sub_before);
        assert_eq!(
            row(w.db(), B).state,
            STATE_LIVE,
            "the file moved out earlier"
        );

        run(&w, inv9());
        assert!(
            all_rows(w.db()).iter().all(|r| r.state == STATE_DELETED),
            "{:?}",
            states(w.db())
        );
        assert_eq!(count(w.db(), "fp_identity"), identities);
        assert_eq!(
            count(w.db(), "fp_item"),
            3,
            "tombstones are kept until a snapshot"
        );
    }

    /// Fact 4: nothing proves a deep or large delete lists every descendant, so a delete takes the
    /// subtree by path prefix. A sibling whose name merely starts the same (`subx`) is not a
    /// descendant.
    ///
    /// Proof this test can fail: dropping the `rows_under` pass of the delete loop leaves `b.txt` and
    /// `c.txt` live; dropping the `/` from the range's lower bound tombstones `sub-x.txt` and `sub.bak`
    /// too; widening the range to `path%` tombstones `subx`.
    #[test]
    fn a_delete_takes_the_subtree_by_path_prefix_and_not_a_lookalike_sibling() {
        let w = world(true, true);
        run(
            &w,
            vec![
                dir("/FCAP", FCAP),
                dir("/FCAP/sub", SUB),
                file("/FCAP/sub/b.txt", B, "r1"),
                dir("/FCAP/sub/deep", "id:t00000000000FCAPDEEP04"),
                file("/FCAP/sub/deep/c.txt", "id:t00000000000FCAPFILE05", "r2"),
                dir("/FCAP/subx", "id:t00000000000FCAPSUBX06"),
                file("/FCAP/subx/d.txt", "id:t00000000000FCAPFILE07", "r3"),
                // Names that sort BEFORE `sub/` but after `sub`: `-` and `.` are below `/` and `0`.
                file("/FCAP/sub-x.txt", "id:t00000000000FCAPFILE20", "r4"),
                file("/FCAP/sub.bak", "id:t00000000000FCAPFILE21", "r5"),
            ],
        );

        run(&w, vec![gone("/FCAP/sub")]);

        let by_path: HashMap<String, i64> = states(w.db()).into_iter().collect();
        for p in [
            "/fcap/sub",
            "/fcap/sub/b.txt",
            "/fcap/sub/deep",
            "/fcap/sub/deep/c.txt",
        ] {
            assert_eq!(by_path[p], STATE_DELETED, "{p}");
        }
        for p in [
            "/fcap",
            "/fcap/subx",
            "/fcap/subx/d.txt",
            "/fcap/sub-x.txt",
            "/fcap/sub.bak",
        ] {
            assert_eq!(by_path[p], STATE_LIVE, "{p}");
        }
    }

    /// The share/un-share shape (DBSYNC-102): a `deleted` entry followed by an identical re-add of the
    /// same item. To the extension nothing happened, so no row is touched and no value is taken. (The
    /// opposite order, the re-add FIRST, is an edit followed by a delete: the delete wins, see
    /// `a_delete_after_an_upsert_of_the_same_item_wins`.)
    ///
    /// Proof this test can fail: stamping every touched row (dropping the `unchanged` test in
    /// `Sequence::finish`) takes two sequence values for the tombstone and the re-add.
    #[test]
    fn a_delete_followed_by_an_identical_readd_of_one_item_changes_nothing() {
        let w = world(true, true);
        for e in [inv1(), inv2(), inv3()] {
            run(&w, e);
        }
        let (rows, s) = (all_rows(w.db()), seq(w.db()));

        run(
            &w,
            vec![
                gone("/FCAP/sub/b.txt"),
                file("/FCAP/sub/b.txt", B, "rev-aaa1"),
            ],
        );

        assert_eq!(all_rows(w.db()), rows);
        assert_eq!(seq(w.db()), s);
    }

    /// A different item that takes over a deleted path: the delete reaches the old item through the
    /// path it has NOW and the newcomer, listed after it, lives. In the other order the newcomer is
    /// deleted by the delete that follows it (sequential: the later entry wins), and the old item is
    /// replaced either way.
    ///
    /// Proof this test can fail: making `Sequence::delete` skip rows an earlier entry of the batch
    /// upserted (a "live beats deleted" sparing) leaves the newcomer live in the second order.
    #[test]
    fn a_new_item_at_a_deleted_path_replaces_the_old_one_and_the_later_entry_wins() {
        let fresh = "id:t00000000000FCAPFILE08";
        for new_first in [false, true] {
            let w = world(true, true);
            run(&w, vec![dir("/FCAP", FCAP), file("/FCAP/x.txt", B, "old")]);
            let mut batch = vec![gone("/FCAP/x.txt"), file("/FCAP/x.txt", fresh, "new")];
            if new_first {
                batch.reverse();
            }
            run(&w, batch);
            assert_eq!(row(w.db(), B).state, STATE_DELETED, "new_first={new_first}");
            let want = if new_first { STATE_DELETED } else { STATE_LIVE };
            assert_eq!(row(w.db(), fresh).state, want, "new_first={new_first}");
        }
    }

    // ── identity ────────────────────────────────────────────────────────────────────────────────

    /// Identifiers are minted from the allocator `local_file_index` uses, so they cannot collide with
    /// it, including after the local rows that held the highest numbers are deleted; and a revival, a
    /// clear and a reseed all hand the same item the same identifier.
    ///
    /// Proof this test can fail: minting with `MAX(item_id) + 1` over `fp_identity` hands out `1`
    /// first, which `local_file_index` already holds.
    #[test]
    fn identities_are_stable_and_never_collide_with_the_local_index() {
        let w = world(true, true);
        let db = w.db();
        db.upsert_local_file("l1.txt", "h", 1, 0).unwrap();
        db.upsert_local_file("l2.txt", "h", 1, 0).unwrap();
        run(&w, vec![dir("/FCAP", FCAP), file("/FCAP/b.txt", B, "r1")]);
        db.upsert_local_file("l3.txt", "h", 1, 0).unwrap();
        let dead: Vec<i64> = ["l3.txt", "l2.txt"]
            .iter()
            .map(|p| db.get_local_file(p).unwrap().unwrap().item_id.unwrap())
            .collect();
        db.remove_local_file("l3.txt").unwrap();
        db.remove_local_file("l2.txt").unwrap();
        run(
            &w,
            vec![file("/FCAP/c.txt", "id:t00000000000FCAPFILE09", "r2")],
        );

        let local: HashSet<i64> = db
            .list_local_files()
            .unwrap()
            .into_iter()
            .filter_map(|f| f.item_id)
            .collect();
        let projected: HashSet<i64> = all_rows(db).iter().map(|r| r.item_id).collect();
        assert_eq!(projected.len(), 3);
        assert!(local.is_disjoint(&projected), "{local:?} vs {projected:?}");
        // A dead local identity is not handed to a new projected item either.
        assert!(
            dead.iter().all(|id| !projected.contains(id)),
            "a deleted local identity {dead:?} was handed to a projected item {projected:?}"
        );

        let b = row(db, B).item_id;
        run(&w, vec![gone("/FCAP/b.txt")]);
        run(&w, vec![file("/FCAP/b.txt", B, "r3")]);
        assert_eq!(row(db, B).item_id, b, "a revival keeps its identifier");
        assert_eq!(row(db, B).state, STATE_LIVE);

        db.project_clear().unwrap();
        seed(&w, vec![file("/FCAP/b.txt", B, "r4")]).unwrap();
        assert_eq!(row(db, B).item_id, b, "a clear and a reseed keep it too");
    }

    // ── idempotence, sequence, anchors ──────────────────────────────────────────────────────────

    /// Replaying a batch (the cursor did not persist) changes no row and takes no sequence value, and
    /// asks nobody to re-enumerate.
    ///
    /// Proof this test can fail: dropping the `unchanged` test in `Sequence::finish` stamps every
    /// replayed row.
    #[test]
    fn replaying_a_batch_changes_nothing() {
        let w = world(true, true);
        for e in [inv1(), inv2(), inv3(), inv4()] {
            run(&w, e);
        }
        let (rows, s) = (all_rows(w.db()), seq(w.db()));
        let signals = crate::file_provider::signals_requested_on_this_thread();

        run(&w, inv4());
        run(&w, inv4());

        assert_eq!(all_rows(w.db()), rows);
        assert_eq!(seq(w.db()), s);
        assert_eq!(
            crate::file_provider::signals_requested_on_this_thread(),
            signals,
            "a replay that changed nothing must not signal"
        );
    }

    /// The rename of a folder of 204 entries as it was captured live (#234, 2026-11-06): every delete
    /// in one invocation, then every re-add in the next. Every identity survives the gap, and only the
    /// first half holds the signal back.
    ///
    /// Proof this test can fail: removing `self.st.vanished += 1` fails the hold check; passing `true`
    /// to `signal_after_delta` unconditionally fails the check on the second half.
    #[test]
    fn a_rename_split_across_two_invocations_keeps_every_id_and_holds_the_signal_once() {
        let w = world(true, true);
        run(&w, vec![dir("/FP", FCAP), file("/FP/b.txt", B, "rev-aaa1")]);
        let before: Vec<i64> = all_rows(w.db()).iter().map(|r| r.item_id).collect();
        let holds = crate::file_provider::signal_holds_on_this_thread();

        run(&w, vec![gone("/FP/b.txt"), gone("/FP")]);
        assert_eq!(
            crate::file_provider::signal_holds_on_this_thread(),
            holds + 1,
            "the deletes may be half a rename"
        );
        assert!(all_rows(w.db()).iter().all(|r| r.state == STATE_DELETED));

        run(&w, vec![dir("/FQ", FCAP), file("/FQ/b.txt", B, "rev-bbb2")]);
        assert_eq!(
            crate::file_provider::signal_holds_on_this_thread(),
            holds + 1,
            "the re-adds removed nothing"
        );
        let after = all_rows(w.db());
        assert_eq!(
            after.iter().map(|r| r.item_id).collect::<Vec<_>>(),
            before,
            "no identity was minted"
        );
        assert!(after.iter().all(|r| r.state == STATE_LIVE));
    }

    /// A rename inside one invocation removes nothing by the end of the batch, so nothing is held.
    ///
    /// Proof this test can fail: counting `vanished` at each tombstone instead of in `finish` makes it
    /// non-zero here.
    #[test]
    fn a_rename_inside_one_invocation_holds_nothing() {
        let w = world(true, true);
        for e in [inv1(), inv2(), inv3()] {
            run(&w, e);
        }
        let holds = crate::file_provider::signal_holds_on_this_thread();
        let signals = crate::file_provider::signals_requested_on_this_thread();

        run(&w, inv4());

        assert_eq!(crate::file_provider::signal_holds_on_this_thread(), holds);
        assert_eq!(
            crate::file_provider::signals_requested_on_this_thread(),
            signals + 1
        );
    }

    /// A real change asks for exactly one signal per commit.
    ///
    /// Proof this test can fail: removing the `commit.rows > 0` guard in `run_remote_delta` signals on
    /// the replay (the previous test); removing the signal call fails here.
    #[test]
    fn a_commit_that_changed_something_requests_a_signal() {
        let w = world(true, true);
        let before = crate::file_provider::signals_requested_on_this_thread();
        run(&w, inv1());
        assert_eq!(
            crate::file_provider::signals_requested_on_this_thread(),
            before + 1
        );
    }

    /// Every writer takes sequence values when it changes something, and none takes one when it does
    /// not. Values are unique per row, so paging by `changed_seq` has no ties.
    ///
    /// Proof this test can fail: replacing `st.stamp()` by `st.seq` in a writer (delta rows, the filter
    /// flip, the downloaded flag, or the expiry of a snapshot or a clear) leaves `seq` where it was.
    #[test]
    fn every_writer_advances_the_sequence_and_a_no_op_does_not() {
        let w = world(true, true);
        let db = w.db();

        let s0 = seq(db);
        run(&w, inv1());
        run(&w, inv2());
        run(&w, inv3());
        let s_delta = seq(db);
        assert!(s_delta > s0, "delta");
        run(&w, vec![file("/FCAP/sub/b.txt", B, "rev-edited")]);
        assert_eq!(
            seq(db),
            s_delta + 1,
            "an edit of an existing row takes exactly one value"
        );
        let s_delta = seq(db);
        let seqs: Vec<i64> = all_rows(db).iter().map(|r| r.seq).collect();
        assert_eq!(
            seqs.iter().collect::<HashSet<_>>().len(),
            seqs.len(),
            "no ties"
        );

        db.set_exclude_prefixes_csv("FCAP/sub").unwrap();
        let c = db.project_reapply_filter().unwrap();
        assert!(c.rows > 0 && c.seq > s_delta, "filter reapply");
        let s_filter = seq(db);
        assert_eq!(
            db.project_reapply_filter().unwrap().rows,
            0,
            "same filter, no-op"
        );
        assert_eq!(seq(db), s_filter);

        db.set_projection_downloaded(B, true).unwrap();
        let s_down = seq(db);
        assert!(s_down > s_filter, "downloaded flag");
        db.set_projection_downloaded(B, true).unwrap();
        assert_eq!(seq(db), s_down, "same flag, no-op");

        let all: Vec<DropboxEntry> = inv1().into_iter().chain(inv2()).chain(inv3()).collect();
        seed(&w, all).unwrap();
        let s_snap = seq(db);
        assert!(s_snap > s_down, "snapshot");

        db.project_clear().unwrap();
        let s_clear = seq(db);
        assert!(s_clear > s_snap, "clear");
        db.project_clear().unwrap();
        assert_eq!(seq(db), s_clear, "clearing an empty projection is a no-op");
    }

    /// `min_valid_seq` and `generation` move on a snapshot or a clear and on nothing else: not a delta,
    /// not a filter change, not a download flag.
    ///
    /// Proof this test can fail: calling `expire_anchors` from `project_delta` (or the filter writer)
    /// moves `min_valid_seq` at the first delta.
    #[test]
    fn anchors_expire_only_on_a_snapshot_or_a_clear() {
        let w = world(true, true);
        let db = w.db();
        let populated = db.projection_state().unwrap();
        assert!(populated.populated && populated.min_valid_seq > 0);

        run(&w, inv1());
        run(&w, inv2());
        run(&w, inv3());
        db.set_exclude_prefixes_csv("FCAP/sub").unwrap();
        db.project_reapply_filter().unwrap();
        db.set_projection_downloaded(B, true).unwrap();
        run(&w, inv4());
        let after_writes = db.projection_state().unwrap();
        assert_eq!(after_writes.min_valid_seq, populated.min_valid_seq);
        assert_eq!(after_writes.generation, populated.generation);
        assert!(after_writes.seq > populated.seq);

        seed(&w, Vec::new()).unwrap();
        let snap = db.projection_state().unwrap();
        assert!(snap.min_valid_seq > after_writes.seq, "a snapshot expires");
        assert_eq!(snap.min_valid_seq, snap.seq);
        assert_eq!(snap.generation, populated.generation);

        run(&w, inv1());
        db.project_clear().unwrap();
        let cleared = db.projection_state().unwrap();
        assert!(
            cleared.min_valid_seq > snap.min_valid_seq,
            "a clear expires"
        );
        assert!(!cleared.populated);
    }

    /// The expiry rule itself: a different generation, a sequence below `min_valid_seq`, or one the
    /// database has not reached. Nothing else.
    ///
    /// Proof this test can fail: dropping any one of the three disjuncts of `anchor_expired`.
    #[test]
    fn the_anchor_rule_expires_on_generation_floor_and_ceiling_only() {
        let s = ProjectionState {
            generation: 7,
            seq: 50,
            min_valid_seq: 10,
            populated: true,
        };
        assert!(!anchor_expired(&s, 7, 10));
        assert!(!anchor_expired(&s, 7, 30));
        assert!(!anchor_expired(&s, 7, 50));
        assert!(anchor_expired(&s, 8, 30), "another generation");
        assert!(anchor_expired(&s, 7, 9), "below the floor");
        assert!(anchor_expired(&s, 7, 51), "from the future");
    }

    /// A snapshot is a mark and sweep by Dropbox id: what it lists is upserted under its existing id,
    /// what it does not is gone (and so are old tombstones), and it populates the projection.
    ///
    /// Proof this test can fail: removing the sweep loop keeps the unseen item live; sweeping by path
    /// instead of by id would give the kept item a new identifier.
    #[test]
    fn a_snapshot_keeps_the_ids_of_what_it_sees_and_sweeps_the_rest() {
        let w = world(true, false);
        let db = w.db();
        assert!(!db.projection_state().unwrap().populated);
        let listing = |hold_b: bool| -> Vec<DropboxEntry> {
            let mut entries = vec![dir("/FCAP", FCAP), dir("/FCAP/sub", SUB)];
            if hold_b {
                entries.push(file("/FCAP/sub/b.txt", B, "r1"));
            }
            entries
        };
        seed(&w, listing(true)).unwrap();
        assert!(db.projection_state().unwrap().populated);
        assert_eq!(count(db, "fp_item"), 3);
        let sub = row(db, SUB).item_id;

        seed(&w, listing(false)).unwrap();
        assert_eq!(count(db, "fp_item"), 2, "the unseen file is swept");
        assert_eq!(row(db, SUB).item_id, sub);
        assert_eq!(count(db, "fp_identity"), 3, "its identity is not");
    }

    // ── versions ────────────────────────────────────────────────────────────────────────────────

    /// Both versions fit the system's 128 bytes whatever a name, a hash or a rev looks like; the
    /// content version follows the hash and only the hash, the metadata version follows name, parent
    /// and rev.
    ///
    /// Proof this test can fail: including `rev` in `content_version`, dropping `parent_item` from
    /// `meta_version`, or returning the raw name/hash (no digest) breaks one assertion each.
    #[test]
    fn versions_fit_128_bytes_and_depend_only_on_what_they_should() {
        let huge = "x".repeat(5000);
        for v in [
            content_version(false, &huge),
            content_version(false, ""),
            content_version(true, &huge),
            meta_version(&huge, i64::MAX, &huge),
            meta_version("", ROOT_ITEM, ""),
        ] {
            assert!(v.len() <= 128, "{} bytes", v.len());
            assert!(!v.is_empty());
        }
        assert_ne!(content_version(false, "h1"), content_version(false, "h2"));
        assert_eq!(content_version(false, "h1"), content_version(false, "h1"));
        assert_ne!(meta_version("a", 1, "r"), meta_version("b", 1, "r"));
        assert_ne!(meta_version("a", 1, "r"), meta_version("a", 2, "r"));
        assert_ne!(meta_version("a", 1, "r"), meta_version("a", 1, "s"));
        // A name that runs into the parent or the rev must not collide with one that does not.
        assert_ne!(meta_version("a1", 2, "r"), meta_version("a", 12, "r"));
        assert_ne!(content_version(true, ""), content_version(false, ""));
    }

    /// A new revision of unchanged bytes (what a move produces) leaves the stored content version
    /// alone; new bytes change it and make a local copy stale.
    ///
    /// Proof this test can fail: the same `rev`-in-`content_version` mutation as above.
    #[test]
    fn a_new_rev_with_the_same_bytes_keeps_the_content_version_and_new_bytes_do_not() {
        let w = world(true, true);
        let db = w.db();
        run(
            &w,
            vec![dir("/FCAP", FCAP), file("/FCAP/b.txt", B, "rev-1")],
        );
        db.set_projection_downloaded(B, true).unwrap();
        let first = row(db, B);

        run(&w, vec![file("/FCAP/b.txt", B, "rev-2")]);
        let same_bytes = row(db, B);
        assert_eq!(same_bytes.cv, first.cv);
        assert_ne!(same_bytes.mv, first.mv);
        assert!(
            same_bytes.downloaded,
            "the local copy is still the right bytes"
        );

        run(
            &w,
            vec![file_with("/FCAP/b.txt", B, "rev-3", "another-hash")],
        );
        let new_bytes = row(db, B);
        assert_ne!(new_bytes.cv, first.cv);
        assert!(!new_bytes.downloaded, "new bytes make the local copy stale");
    }

    // ── gating ──────────────────────────────────────────────────────────────────────────────────

    /// With the switch off nothing is written: no rows, no identities, no sequence value, no signal,
    /// whatever the batch and however the writers are called.
    ///
    /// Proof this test can fail: removing the `projection_active` guard from `project_delta` (or from
    /// the snapshot, filter or downloaded writer) writes rows or moves `seq`.
    #[test]
    fn with_the_switch_off_nothing_is_written() {
        let w = world(false, false);
        let db = w.db();
        let before = db.projection_state().unwrap();
        let signals = crate::file_provider::signals_requested_on_this_thread();

        run(&w, inv1());
        run(&w, inv4());
        assert_eq!(
            db.project_delta(&inv1()).unwrap().rows,
            0,
            "the writer refuses too"
        );
        let entry = ProjectedEntry::from_entry(&dir("/FCAP", FCAP)).unwrap();
        assert_eq!(db.project_snapshot(&[entry]).unwrap().rows, 0);
        assert_eq!(db.project_reapply_filter().unwrap().rows, 0);
        assert_eq!(db.set_projection_downloaded(FCAP, true).unwrap().rows, 0);
        assert_eq!(db.project_clear().unwrap().rows, 0);

        assert_eq!(count(db, "fp_item"), 0);
        assert_eq!(count(db, "fp_identity"), 0);
        assert_eq!(db.projection_state().unwrap(), before);
        assert_eq!(
            crate::file_provider::signals_requested_on_this_thread(),
            signals
        );
    }

    /// Switch on but never populated: a delta, a filter change and a download flag write nothing, so a
    /// stale base can never be patched into looking whole.
    ///
    /// Proof this test can fail: removing the `populated(conn)?` early return from `project_delta`.
    #[test]
    fn an_unpopulated_projection_refuses_every_incremental_writer() {
        let w = world(true, false);
        let db = w.db();
        let before = db.projection_state().unwrap();

        run(&w, inv1());
        db.set_exclude_prefixes_csv("FCAP").unwrap();
        assert_eq!(db.project_reapply_filter().unwrap().rows, 0);
        assert_eq!(db.set_projection_downloaded(FCAP, true).unwrap().rows, 0);

        assert_eq!(count(db, "fp_item"), 0);
        assert_eq!(db.projection_state().unwrap(), before);
    }

    /// `notPopulated` answers `serverUnreachable`, which only `signalErrorResolved:` lifts: the working-set
    /// signal the seed also sends does not.
    ///
    /// Proof this test can fail: removing the call from `finish_seed`.
    #[test]
    fn a_landed_snapshot_lifts_what_the_empty_projection_answered() {
        let w = world(true, false);
        let before = crate::file_provider::errors_resolved_on_this_thread();

        seed(&w, inv1()).unwrap();

        assert_eq!(
            crate::file_provider::errors_resolved_on_this_thread(),
            before + 1
        );
    }

    /// The switch is read once at startup, but the file outlives a run: a projection populated earlier
    /// must not be written by anything once the switch is off.
    ///
    /// Proof this test can fail: removing the `projection_active` guard from `project_delta`, the filter
    /// writer or the downloaded writer lets that writer through.
    #[test]
    fn every_incremental_writer_refuses_when_the_switch_is_off_over_a_populated_projection() {
        let w = world(true, true);
        let db = w.db();
        run(&w, inv1());
        run(&w, inv2());
        run(&w, inv3());
        db.set_projection_enabled(false);
        let (rows, state) = (all_rows(db), db.projection_state().unwrap());
        db.set_exclude_prefixes_csv("FCAP").unwrap();

        assert_eq!(db.project_delta(&inv4()).unwrap().rows, 0, "delta");
        assert_eq!(db.project_reapply_filter().unwrap().rows, 0, "filter");
        assert_eq!(
            db.set_projection_downloaded(B, true).unwrap().rows,
            0,
            "downloaded"
        );

        assert_eq!(all_rows(db), rows);
        assert_eq!(db.projection_state().unwrap(), state);
    }

    /// Rows without the `populated` flag (a state the writers never produce, but a database can be
    /// edited or half-restored) are not patched: a filter change and a download flag leave them alone.
    ///
    /// Proof this test can fail: removing the `populated(conn)?` early return from the filter writer or
    /// the downloaded writer.
    #[test]
    fn rows_without_the_populated_flag_are_not_patched() {
        let w = world(true, true);
        let db = w.db();
        run(&w, inv1());
        run(&w, inv2());
        run(&w, inv3());
        db.write
            .lock()
            .unwrap()
            .execute("UPDATE fp_state SET populated = 0", [])
            .unwrap();
        let (rows, state) = (all_rows(db), db.projection_state().unwrap());
        db.set_exclude_prefixes_csv("FCAP").unwrap();

        assert_eq!(db.project_reapply_filter().unwrap().rows, 0, "filter");
        assert_eq!(
            db.set_projection_downloaded(B, true).unwrap().rows,
            0,
            "downloaded"
        );

        assert_eq!(all_rows(db), rows);
        assert_eq!(db.projection_state().unwrap(), state);
    }

    // ── selective sync ──────────────────────────────────────────────────────────────────────────

    /// Excluded items are stored but hidden, an ancestor of an include entry stays visible, a filter
    /// change flips the flag in place (same ids, no snapshot), and an item that ARRIVES under an
    /// excluded path is hidden from the start.
    ///
    /// Proof this test can fail: making `visibility` always return `STATE_LIVE` (or ignoring
    /// `Ancestor`) breaks the first, second or fourth assertion.
    #[test]
    fn selective_sync_hides_and_unhides_by_flag() {
        let w = world(true, true);
        let db = w.db();
        run(
            &w,
            vec![
                dir("/FCAP", FCAP),
                dir("/FCAP/sub", SUB),
                file("/FCAP/sub/b.txt", B, "r1"),
                file("/FCAP/keep.txt", "id:t00000000000FCAPFILE10", "r2"),
            ],
        );
        let ids: Vec<i64> = all_rows(db).iter().map(|r| r.item_id).collect();
        let min_valid = db.projection_state().unwrap().min_valid_seq;

        db.set_exclude_prefixes_csv("FCAP/sub").unwrap();
        let flipped = db.project_reapply_filter().unwrap();
        assert_eq!(flipped.rows, 2, "the folder and its file, nothing else");
        let by_path: HashMap<String, i64> = states(db).into_iter().collect();
        assert_eq!(by_path["/fcap/sub"], STATE_HIDDEN);
        assert_eq!(by_path["/fcap/sub/b.txt"], STATE_HIDDEN);
        assert_eq!(by_path["/fcap"], STATE_LIVE);
        assert_eq!(by_path["/fcap/keep.txt"], STATE_LIVE);

        run(
            &w,
            vec![file("/FCAP/sub/new.txt", "id:t00000000000FCAPFILE11", "r3")],
        );
        assert_eq!(
            row(db, "id:t00000000000FCAPFILE11").state,
            STATE_HIDDEN,
            "arrives hidden"
        );

        // An include list: the folder above the entry is an ancestor, so it stays visible.
        db.set_exclude_prefixes_csv("").unwrap();
        db.set_include_prefixes_csv("FCAP/sub/b.txt").unwrap();
        db.project_reapply_filter().unwrap();
        let by_path: HashMap<String, i64> = states(db).into_iter().collect();
        assert_eq!(
            by_path["/fcap"], STATE_LIVE,
            "ancestor of the include entry"
        );
        assert_eq!(
            by_path["/fcap/sub"], STATE_LIVE,
            "ancestor of the include entry"
        );
        assert_eq!(by_path["/fcap/sub/b.txt"], STATE_LIVE);
        assert_eq!(by_path["/fcap/keep.txt"], STATE_HIDDEN);

        db.set_include_prefixes_csv("").unwrap();
        db.project_reapply_filter().unwrap();
        assert!(all_rows(db).iter().all(|r| r.state == STATE_LIVE));
        let after_ids: Vec<i64> = all_rows(db)
            .iter()
            .map(|r| r.item_id)
            .filter(|id| ids.contains(id))
            .collect();
        assert_eq!(after_ids.len(), ids.len(), "flags flip in place");
        assert_eq!(
            db.projection_state().unwrap().min_valid_seq,
            min_valid,
            "a filter change does not expire anchors"
        );
    }

    // ── failure and atomicity ───────────────────────────────────────────────────────────────────

    /// A failing writer rolls the whole batch back, so no half of it is ever visible to the extension.
    ///
    /// Proof this test can fail: committing the transaction when the closure fails (in
    /// `write_projection`) leaves the rows written before the failing one.
    #[test]
    fn a_failing_batch_leaves_nothing_behind() {
        let w = world(true, true);
        let db = w.db();
        sabotage(db, "poison.txt");
        let batch = [
            dir("/FCAP", FCAP),
            file("/FCAP/b.txt", B, "r1"),
            file("/FCAP/poison.txt", "id:t00000000000FCAPFILE12", "r2"),
        ];
        let entries: Vec<ProjectedEntry> = batch
            .iter()
            .filter_map(ProjectedEntry::from_entry)
            .collect();
        let before = db.projection_state().unwrap();

        assert!(db.project_snapshot(&entries).is_err());

        assert_eq!(count(db, "fp_item"), 0, "no partial rows");
        assert_eq!(
            db.projection_state().unwrap(),
            before,
            "no partial sequence"
        );
    }

    /// A projection failure inside `run_remote_delta` fails the run and leaves the cursor where it
    /// was, so the same batch is fetched again; once the cause is gone the replay succeeds and the
    /// cursor moves.
    ///
    /// Proof this test can fail: discarding the result of `project_delta` in `run_remote_delta`
    /// (`let _ =`) advances the cursor past a batch the projection never took.
    #[test]
    fn a_projection_failure_keeps_the_cursor_and_the_batch_replays() {
        let w = world(true, true);
        let db = w.db();
        db.set_app_config(REMOTE_DELTA_CURSOR_KEY, "cursor-before")
            .unwrap();
        run(&w, inv1());
        db.set_app_config(REMOTE_DELTA_CURSOR_KEY, "cursor-before")
            .unwrap();
        sabotage(db, "b.txt");

        let failed = try_run(&w, inv3());

        assert!(failed.is_err());
        assert_eq!(
            db.get_app_config(REMOTE_DELTA_CURSOR_KEY)
                .unwrap()
                .as_deref(),
            Some("cursor-before"),
            "the cursor did not move"
        );
        assert_eq!(count(db, "fp_item"), 1);

        unsafe_free_retry(&w);
    }

    fn unsafe_free_retry(w: &World) {
        let db = w.db();
        unsabotage(db);
        run(w, inv2());
        run(w, inv3());
        assert_eq!(
            db.get_app_config(REMOTE_DELTA_CURSOR_KEY)
                .unwrap()
                .as_deref(),
            Some("cursor-after")
        );
        assert_eq!(count(db, "fp_item"), 3);
    }

    // ── orphans ─────────────────────────────────────────────────────────────────────────────────

    /// A file whose folder is not known is an orphan (never listed) until the folder arrives, in a
    /// later batch, and then becomes an ordinary item. In the SAME batch the order does not matter.
    ///
    /// Proof this test can fail: removing the `heal_orphans` call leaves it at `STATE_ORPHAN`.
    #[test]
    fn a_child_listed_before_its_folder_is_an_orphan_until_the_folder_arrives() {
        let w = world(true, true);
        run(&w, vec![file("/FCAP/sub/b.txt", B, "r1")]);
        assert_eq!(row(w.db(), B).state, STATE_ORPHAN);
        run(&w, vec![dir("/FCAP", FCAP)]);
        assert_eq!(
            row(w.db(), B).state,
            STATE_ORPHAN,
            "grandparent still unknown"
        );
        run(&w, vec![dir("/FCAP/sub", SUB)]);
        let (sub, b) = (row(w.db(), SUB), row(w.db(), B));
        assert_eq!(b.state, STATE_LIVE);
        assert_eq!(b.parent, sub.item_id);

        let same_batch = world(true, true);
        let base = seq(same_batch.db());
        run(
            &same_batch,
            vec![
                file("/FCAP/sub/b.txt", B, "r1"),
                dir("/FCAP/sub", SUB),
                dir("/FCAP", FCAP),
            ],
        );
        assert!(all_rows(same_batch.db())
            .iter()
            .all(|r| r.state == STATE_LIVE));
        assert_eq!(
            seq(same_batch.db()) - base,
            3,
            "three rows, each written once: the folder was found in the batch, not healed afterwards"
        );
    }

    // ── lifecycle ───────────────────────────────────────────────────────────────────────────────

    /// `reset_sync_state` (logout, a new sync folder) empties the projection and expires its anchors,
    /// but the sequence never goes back and the identities stay.
    ///
    /// Proof this test can fail: removing `projection::clear_in` from `reset_sync_state`.
    #[test]
    fn reset_sync_state_empties_the_projection_without_rewinding_the_sequence() {
        let w = world(true, true);
        let db = w.db();
        run(&w, inv1());
        run(&w, inv2());
        let before = db.projection_state().unwrap();

        db.reset_sync_state().unwrap();

        let after = db.projection_state().unwrap();
        assert_eq!(count(db, "fp_item"), 0);
        assert_eq!(count(db, "fp_identity"), 2);
        assert!(!after.populated);
        assert!(after.seq > before.seq, "monotonic across the reset");
        assert_eq!(after.min_valid_seq, after.seq, "anchors expired");
        assert_eq!(after.generation, before.generation);
    }

    /// Reopening an existing database keeps the generation and the sequence; migrating twice changes
    /// nothing. A re-minted generation would silently expire every anchor at each launch.
    ///
    /// Proof this test can fail: `INSERT OR REPLACE` for the `fp_state` seed row in `migrate`.
    #[test]
    fn reopening_keeps_the_generation_and_the_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.db");
        let first = Db::new_at(&path).unwrap();
        first.set_projection_enabled(true);
        first.project_snapshot(&[]).unwrap();
        let state = first.projection_state().unwrap();
        drop(first);

        let second = Db::new_at(&path).unwrap();
        assert_eq!(second.projection_state().unwrap(), state);
    }

    // ── the schema contract with the extension ──────────────────────────────────────────────────

    fn normalized_schema(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare(
                "SELECT type || ' ' || name || ' ' || tbl_name || ' ' || COALESCE(sql, '')
                 FROM sqlite_master WHERE name LIKE '%fp\\_%' ESCAPE '\\' ORDER BY type, name",
            )
            .unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
        rows.map(|r| {
            // Comments and layout are not schema.
            r.unwrap()
                .lines()
                .map(|l| l.split("--").next().unwrap_or(""))
                .collect::<Vec<_>>()
                .join(" ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
    }

    /// The fixture the extension's tests build from is the schema the host really creates: same
    /// tables, columns, constraints and indexes.
    ///
    /// Proof this test can fail: changing one column, a CHECK or an index in either `SCHEMA_SQL` or
    /// `Tests/projection-schema.sql` makes the two lists differ.
    #[test]
    fn the_schema_fixture_matches_the_real_schema() {
        let fixture = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../native/macos/FileProviderExtension/Tests/projection-schema.sql"
        ));
        // The fixture may not depend on the tables the host's migration creates around it.
        let from_fixture = Connection::open_in_memory().unwrap();
        from_fixture.execute_batch(fixture).unwrap();
        let from_host = Connection::open_in_memory().unwrap();
        from_host.execute_batch(SCHEMA_SQL).unwrap();

        let (fixture_schema, host_schema) = (
            normalized_schema(&from_fixture),
            normalized_schema(&from_host),
        );
        assert!(
            host_schema.len() == 9,
            "sanity: four tables and five indexes, got {host_schema:?}"
        );
        assert_eq!(fixture_schema, host_schema);

        // And the migration creates exactly that, not a variation of it.
        let migrated = Connection::open_in_memory().unwrap();
        migrated
            .execute_batch("CREATE TABLE item_id_seq (id INTEGER PRIMARY KEY AUTOINCREMENT);")
            .unwrap();
        migrate(&migrated).unwrap();
        assert_eq!(normalized_schema(&migrated), host_schema);
    }

    // ── the mapping from raw entries ────────────────────────────────────────────────────────────

    #[test]
    fn raw_entries_map_to_projected_entries_and_deletes_to_paths() {
        let f = ProjectedEntry::from_entry(&file("/Fotos/Ñandú.jpg", B, "r1")).unwrap();
        assert_eq!(f.name, "Ñandú.jpg");
        assert_eq!(f.path_lower, "/fotos/ñandú.jpg");
        assert_eq!((f.size_bytes, f.is_folder), (5, false));
        assert!(f.modified_ts > 0);
        let d = ProjectedEntry::from_entry(&dir("/Fotos", FCAP)).unwrap();
        assert_eq!((d.size_bytes, d.modified_ts, d.is_folder), (0, 0, true));
        assert!(ProjectedEntry::from_entry(&gone("/Fotos")).is_none());
        let mut no_id = file("/x.txt", B, "r");
        no_id.id = None;
        assert!(
            ProjectedEntry::from_entry(&no_id).is_none(),
            "cannot key without an id"
        );
        let ops = ops_from_entries(&[gone("/A/B"), dir("/A", FCAP), entry("weird", "/z")]);
        assert_eq!(ops.len(), 2, "the unknown tag is skipped");
        assert_eq!(ops[0], Op::Delete("/a/b".to_string()));
        assert!(matches!(&ops[1], Op::Upsert(e) if e.path_lower == "/a" && e.is_folder));
        assert!(
            ops_from_entries(&[gone("/")]).is_empty(),
            "the root is skipped"
        );
    }

    /// With the switch off, `run_remote_delta` is exactly what it was: the cursor moves and the batch
    /// is applied to the remote index as before.
    #[test]
    fn with_the_switch_off_the_cursor_still_advances() {
        let w = world(false, false);
        run(&w, vec![file("/FCAP/b.txt", B, "r1")]);
        assert_eq!(
            w.db()
                .get_app_config(REMOTE_DELTA_CURSOR_KEY)
                .unwrap()
                .as_deref(),
            Some("cursor-after")
        );
        assert!(w.db().get_remote_file("FCAP/b.txt").unwrap().is_some());
    }

    // ── review round: displacement, ordering, folding ───────────────────────────────────────────

    const NFC_DIR: &str = "Caf\u{e9}";
    const NFD_DIR: &str = "Cafe\u{301}";
    const X_DIR: &str = "id:t00000000000FCAPXDIR10";
    const X_FILE: &str = "id:t00000000000FCAPXFIL11";
    const X_CHILD: &str = "id:t00000000000FCAPXCHD12";
    const X_CHILD2: &str = "id:t00000000000FCAPXCHD13";
    const X_NEW: &str = "id:t00000000000FCAPXNEW14";

    /// Dropbox's contract: a file or folder entry replaces whatever is at its path, with no `deleted`
    /// entry. A folder with a child, then ONLY a file at the folder's path under a new id: the folder
    /// and its child are gone, the file is the one live item.
    ///
    /// Proof this test can fail: removing the occupant loop from `upsert` leaves the folder, its
    /// child and the file all live at `/x`; skipping the subtree in that loop leaves the child live.
    #[test]
    fn a_file_that_replaces_a_folder_tombstones_the_folder_and_its_subtree() {
        let w = world(true, true);
        run(&w, vec![dir("/X", X_DIR), file("/X/c.txt", X_CHILD, "r1")]);

        run(&w, vec![file("/X", X_FILE, "r2")]);

        assert_eq!(row(w.db(), X_DIR).state, STATE_DELETED, "the folder");
        assert_eq!(row(w.db(), X_CHILD).state, STATE_DELETED, "its child");
        let new = row(w.db(), X_FILE);
        assert_eq!((new.state, new.parent), (STATE_LIVE, ROOT_ITEM));
        let live: Vec<_> = all_rows(w.db())
            .into_iter()
            .filter(|r| r.state == STATE_LIVE)
            .collect();
        assert_eq!(live.len(), 1, "{:?}", states(w.db()));
    }

    /// A folder that arrives under a NEW id at a folder's path replaces it and KEEPS its children: only
    /// the old folder is tombstoned, and its non-deleted children are re-parented to the newcomer (a
    /// parent change, so they are stamped). Deeper levels hang under their own parents and do not move.
    ///
    /// Proof this test can fail: tombstoning the old folder's subtree (the file-over-folder rule) in
    /// the folder branch of `upsert` deletes the child; skipping the re-parent leaves the child under
    /// the tombstone; re-parenting the grandchild too stamps it.
    #[test]
    fn a_folder_replacing_a_folder_keeps_its_children() {
        let w = world(true, true);
        run(
            &w,
            vec![
                dir("/X", X_DIR),
                file("/X/c.txt", X_CHILD, "r1"),
                dir("/X/s", X_CHILD2),
                file("/X/s/g.txt", X_FILE, "r1"),
                file("/X/gone.txt", &id(700), "r1"),
            ],
        );
        run(&w, vec![gone("/X/gone.txt")]);
        let (child0, deep0) = (row(w.db(), X_CHILD), row(w.db(), X_FILE));

        let batch = vec![dir("/X", X_NEW)];
        run(&w, dup(&batch));

        assert_eq!(row(w.db(), X_DIR).state, STATE_DELETED, "the old folder");
        let (new, child, sub, deep) = (
            row(w.db(), X_NEW),
            row(w.db(), X_CHILD),
            row(w.db(), X_CHILD2),
            row(w.db(), X_FILE),
        );
        assert_eq!((new.state, new.parent), (STATE_LIVE, ROOT_ITEM));
        assert_eq!(
            (child.state, child.parent, child.path.as_str()),
            (STATE_LIVE, new.item_id, "/x/c.txt")
        );
        assert!(child.seq > child0.seq, "its parent changed: stamped");
        assert_eq!(sub.parent, new.item_id, "the subfolder is a child too");
        assert_eq!(
            deep, deep0,
            "a grandchild keeps its own parent and is untouched"
        );
        assert_eq!(
            state_of(&w, &id(700)),
            STATE_DELETED,
            "a deleted child is not handed over"
        );
        assert_replay_changes_nothing(&w, &batch, "folder over folder");
    }

    /// A folder arriving at a FILE's path tombstones the file and nothing else; a file arriving at a
    /// folder's path tombstones the folder and its subtree (a file has no children).
    ///
    /// Proof this test can fail: tombstoning the subtree for a folder newcomer deletes the siblings
    /// of the replaced file's namesake; skipping the subtree for a file newcomer leaves `c.txt` live.
    #[test]
    fn a_folder_replacing_a_file_tombstones_only_the_file() {
        let w = world(true, true);
        run(
            &w,
            vec![file("/X", X_FILE, "r1"), file("/Y.txt", X_CHILD, "r1")],
        );

        let batch = vec![dir("/X", X_DIR)];
        run(&w, dup(&batch));

        assert_eq!(row(w.db(), X_FILE).state, STATE_DELETED);
        assert_eq!(row(w.db(), X_DIR).state, STATE_LIVE);
        assert_eq!(row(w.db(), X_CHILD).state, STATE_LIVE, "a bystander");
        assert_replay_changes_nothing(&w, &batch, "folder over file");
    }

    /// Deferred from the 2a/2b review (#236), found by a model-based fuzz harness: a row's parent is
    /// bound when it is written, and a later entry of the batch moved that folder away and put another
    /// at the path.
    ///
    /// Proof this test can fail: without `reresolve_touched`, `a` stays under the folder that moved to
    /// `/D`, and stays live under a tombstone once that one is deleted.
    #[test]
    fn a_child_follows_the_folder_now_at_its_path() {
        let w = world(true, true);
        run(
            &w,
            vec![
                dir("/e", "id:E"),
                dir("/e/D", "id:D1"),
                file("/e/D/a", "id:F", "r1"),
            ],
        );

        run(
            &w,
            vec![
                file("/e/D/a", "id:F", "r1"),
                dir("/D", "id:D1"),
                dir("/e/D", "id:D2"),
            ],
        );

        let d2 = row(w.db(), "id:D2").item_id;
        assert_eq!(
            (row(w.db(), "id:F").parent, row(w.db(), "id:F").state),
            (d2, STATE_LIVE)
        );
        run(&w, vec![gone("/D")]);
        assert_eq!(
            (row(w.db(), "id:F").parent, row(w.db(), "id:F").state),
            (d2, STATE_LIVE)
        );
    }

    /// Deferred from the 2a/2b review (#236): a file at `P` leaves nothing under `P/`, also when the
    /// batch is replayed because its cursor was not saved, and a later folder at `P` brings back nothing.
    ///
    /// Proof this test can fail: sweeping `P/` only when the file displaced a row at `P` leaves `/a/b`
    /// an orphan, which the folder at `/a` heals into a live ghost.
    #[test]
    fn a_file_replacing_a_folder_leaves_nothing_under_it_even_replayed() {
        let w = world(true, true);
        let first = || vec![dir("/a/b", "id:B4")];
        let second = || vec![file("/a", "id:A5", "r1")];
        run(&w, first());
        run(&w, second());
        run(&w, first());
        run(&w, second());
        assert_eq!(row(w.db(), "id:B4").state, STATE_DELETED);

        run(&w, vec![gone("/a"), dir("/a", "id:A6")]);

        assert_eq!(row(w.db(), "id:B4").state, STATE_DELETED);
    }

    /// What the batch itself upserts at a path is never "a different item": two listed items that swap
    /// paths displace nothing. A folder that moves onto another folder's path replaces it and gets that
    /// folder's children, and its OWN children stay where they are (they change only through their own
    /// entries).
    ///
    /// Proof this test can fail: removing the occupant loop of `upsert` leaves both folders live at
    /// `/p`; carrying the mover's children would move `x.txt` to `/p`.
    #[test]
    fn a_swap_and_a_folder_moving_onto_a_folder_follow_the_entries() {
        let w = world(true, true);
        run(&w, vec![dir("/A", X_DIR), file("/B", X_FILE, "r1")]);
        run(&w, vec![file("/A", X_FILE, "r2"), dir("/B", X_DIR)]);
        assert_eq!(row(w.db(), X_DIR).state, STATE_LIVE);
        assert_eq!(row(w.db(), X_FILE).state, STATE_LIVE);
        assert_eq!(row(w.db(), X_DIR).path, "/b");
        assert_eq!(row(w.db(), X_FILE).path, "/a");

        let w = world(true, true);
        run(
            &w,
            vec![
                dir("/a", X_DIR),
                file("/a/x.txt", X_CHILD, "r1"),
                dir("/p", X_NEW),
                file("/p/k.txt", X_CHILD2, "r1"),
            ],
        );
        run(&w, vec![dir("/p", X_DIR)]);
        assert_eq!(
            row(w.db(), X_NEW).state,
            STATE_DELETED,
            "the folder it replaced"
        );
        let (mover, k) = (row(w.db(), X_DIR), row(w.db(), X_CHILD2));
        assert_eq!(
            (k.state, k.parent, k.path.as_str()),
            (STATE_LIVE, mover.item_id, "/p/k.txt"),
            "the replaced folder's child is handed over"
        );
        let x = row(w.db(), X_CHILD);
        assert_eq!(
            (x.state, x.parent, x.path.as_str()),
            (STATE_LIVE, mover.item_id, "/a/x.txt"),
            "the mover's own child is not carried"
        );
    }

    /// A parent that was never stored leaves the child an orphan until the folder arrives, and then it
    /// becomes ordinary.
    ///
    /// Proof this test can fail: removing the `heal_orphans` call, or resolving the parent against
    /// tombstones (see `a_child_listed_under_a_folder_deleted_earlier_in_the_batch_is_an_orphan`).
    #[test]
    fn a_child_of_a_parent_that_was_never_stored_waits_as_an_orphan() {
        let w = world(true, true);
        run(&w, vec![gone("/P"), file("/P/c.txt", X_CHILD, "r1")]);
        assert_eq!(row(w.db(), X_CHILD).state, STATE_ORPHAN);
        run(&w, vec![dir("/P", X_DIR)]);
        let (p, c) = (row(w.db(), X_DIR), row(w.db(), X_CHILD));
        assert_eq!((c.state, c.parent), (STATE_LIVE, p.item_id));
    }

    /// An edit followed by a delete of the same item: the later entry wins, so the item is deleted.
    ///
    /// Proof this test can fail: applying the deletes before the upserts, or making `Sequence::delete`
    /// skip rows that the batch upserted, leaves A live.
    #[test]
    fn a_delete_after_an_upsert_of_the_same_item_wins() {
        let w = world(true, true);
        run(&w, vec![dir("/FCAP", FCAP), file("/FCAP/a.txt", B, "r1")]);

        run(&w, vec![file("/FCAP/a.txt", B, "r2"), gone("/FCAP/a.txt")]);

        let a = row(w.db(), B);
        assert_eq!(a.state, STATE_DELETED);
        assert_eq!(a.rev, "r2", "the edit was applied, then the delete");
        assert_eq!(row(w.db(), FCAP).state, STATE_LIVE);
    }

    /// The same in the other order is a delete followed by a re-add: the item lives (the capture's
    /// rename shape and DBSYNC-102's share shape).
    ///
    /// Proof this test can fail: comparing positions the wrong way round (`d < e.pos`) kills the
    /// re-added item here and spares the edited one in the test above.
    #[test]
    fn a_delete_before_a_readd_of_the_same_item_lets_it_live() {
        let w = world(true, true);
        run(&w, vec![dir("/FCAP", FCAP), file("/FCAP/a.txt", B, "r1")]);

        run(&w, vec![gone("/FCAP/a.txt"), file("/FCAP/a.txt", B, "r2")]);

        let a = row(w.db(), B);
        assert_eq!((a.state, a.rev.as_str()), (STATE_LIVE, "r2"));
    }

    /// The capture's rename shape, deletes first, still yields a move, and so does the same batch with
    /// the deletes LAST: a delete of the OLD path is not a delete of the item at its new one.
    ///
    /// Proof this test can fail: resolving a delete through anything but the rows' paths at the
    /// moment it runs (for example, remembering an upserted item's pre-batch path and deleting through
    /// that) kills the renamed folder when the deletes come last.
    #[test]
    fn a_rename_is_a_move_whether_its_deletes_come_first_or_last() {
        for deletes_last in [false, true] {
            let w = world(true, true);
            for e in [inv1(), inv2(), inv3()] {
                run(&w, e);
            }
            let ids = (row(w.db(), SUB).item_id, row(w.db(), B).item_id);
            let mut batch = inv4();
            if deletes_last {
                batch.rotate_left(2);
            }
            run(&w, batch);
            let (sub, b) = (row(w.db(), SUB), row(w.db(), B));
            assert_eq!((sub.item_id, b.item_id), ids);
            assert_eq!((sub.state, b.state), (STATE_LIVE, STATE_LIVE));
            assert_eq!(b.path, "/fcap/sub2/b.txt", "deletes_last={deletes_last}");
        }
    }

    /// A folder and its listed child upserted, then the folder deleted: both go.
    ///
    /// Proof this test can fail: dropping the `rows_under` pass of `Sequence::delete` kills the folder
    /// but leaves the child live.
    #[test]
    fn a_delete_after_a_folder_and_its_child_were_upserted_takes_both() {
        let w = world(true, true);
        run(&w, vec![dir("/X", X_DIR), file("/X/c.txt", X_CHILD, "r1")]);

        run(
            &w,
            vec![
                dir("/X", X_DIR),
                file("/X/c.txt", X_CHILD, "r2"),
                gone("/X"),
            ],
        );

        assert!(
            all_rows(w.db()).iter().all(|r| r.state == STATE_DELETED),
            "{:?}",
            states(w.db())
        );
    }

    /// Paths are folded, so a delete spelled NFD finds the NFC row it names.
    ///
    /// Proof this test can fail: storing and querying the raw lowercase path (no `fold` in
    /// `normalize_path_lower`) misses the row: the folder and its file stay live.
    #[test]
    fn an_nfd_delete_finds_the_nfc_folder_it_names() {
        let w = world(true, true);
        run(
            &w,
            vec![
                dir(&format!("/{NFC_DIR}"), X_DIR),
                file(&format!("/{NFC_DIR}/a.txt"), X_CHILD, "r1"),
            ],
        );
        assert_eq!(row(w.db(), X_DIR).path, "/caf\u{e9}", "stored folded");

        run(&w, vec![gone(&format!("/{NFD_DIR}"))]);

        assert_eq!(row(w.db(), X_DIR).state, STATE_DELETED);
        assert_eq!(row(w.db(), X_CHILD).state, STATE_DELETED, "the subtree too");
    }

    /// An NFD child of an NFC folder finds its parent (it is not an orphan), and the folder's name
    /// keeps the spelling Dropbox sent.
    ///
    /// Proof this test can fail: the same removal of `fold`: the child's parent path is not found.
    #[test]
    fn an_nfd_child_of_an_nfc_folder_gets_its_parent() {
        let w = world(true, true);
        run(&w, vec![dir(&format!("/{NFC_DIR}"), X_DIR)]);

        run(&w, vec![file(&format!("/{NFD_DIR}/a.txt"), X_CHILD, "r1")]);

        let (p, c) = (row(w.db(), X_DIR), row(w.db(), X_CHILD));
        assert_eq!((c.state, c.parent), (STATE_LIVE, p.item_id));
        assert_eq!(p.name, NFC_DIR);
        assert_eq!(c.path, "/caf\u{e9}/a.txt");
    }

    // ── review round: the four mutants that survived ────────────────────────────────────────────

    /// An orphan that heals follows selective sync: healed under an excluded folder it is hidden, not
    /// listed.
    ///
    /// Proof this test can fail: hardcoding `STATE_LIVE` in `Sequence::heal_orphans` lists the child.
    #[test]
    fn a_healed_orphan_follows_selective_sync() {
        let w = world(true, true);
        w.db().set_exclude_prefixes_csv("ex").unwrap();
        run(&w, vec![file("/ex/c.txt", X_CHILD, "r1")]);
        assert_eq!(row(w.db(), X_CHILD).state, STATE_ORPHAN);

        run(&w, vec![dir("/ex", X_DIR)]);

        assert_eq!(row(w.db(), X_DIR).state, STATE_HIDDEN);
        assert_eq!(row(w.db(), X_CHILD).state, STATE_HIDDEN);
    }

    // ── review round: the pipeline, not a hand-built world ──────────────────────────────────────

    /// The capture end to end through the real functions: a snapshot seeded by `finish_seed`, then the
    /// captured deltas through `run_remote_delta`. The projection ends where Dropbox's tree does.
    ///
    /// Proof this test can fail: dropping the `project_snapshot` call from `finish_seed` leaves the
    /// projection unpopulated, so every delta writes nothing.
    #[test]
    fn a_seeded_snapshot_then_the_captured_deltas_end_at_dropboxs_tree() {
        let w = world(true, false);
        seed(
            &w,
            vec![dir("/FCAP", FCAP), dir("/FCAP/sub", SUB), inv3().remove(0)],
        )
        .unwrap();
        assert!(w.db().projection_state().unwrap().populated);
        let ids = (row(w.db(), SUB).item_id, row(w.db(), B).item_id);

        run(&w, inv4());
        run(&w, inv6());

        let (sub, b) = (row(w.db(), SUB), row(w.db(), B));
        assert_eq!((sub.item_id, b.item_id), ids, "ids survive the snapshot");
        assert_eq!(sub.path, "/fcap/sub2");
        assert_eq!(b.path, "/fcap/b.txt");
        assert!(all_rows(w.db()).iter().all(|r| r.state == STATE_LIVE));
    }

    // ── review round: a cursor beside an empty projection reseeds ───────────────────────────────

    /// `reset_sync_state` racing an in-flight delta: the reset empties the projection and the cursor,
    /// then the delta (which had fetched its pages before) saves its cursor. A cursor now sits beside
    /// an unpopulated projection; it must count as no cursor, so the next iteration seeds, and once the
    /// seed lands the cursor is usable again.
    ///
    /// Proof this test can fail: removing the `projection_reseed_due` check from `usable_delta_cursor`
    /// returns the stale cursor and never seeds.
    #[test]
    fn a_reset_racing_a_delta_leaves_a_cursor_that_reseeds() {
        let w = world(true, true);
        run(&w, inv1());
        let mut page = Some(DropboxListFolderResponse {
            entries: inv2(),
            cursor: "cursor-after".to_string(),
            has_more: false,
        });
        let state = &w.state;
        run_remote_delta(state, "cursor-before", |_| {
            // The reset lands while the pages are being fetched.
            state.db.reset_sync_state().expect("reset");
            Ok(DeltaFetchOutcome::Page(page.take().unwrap()))
        })
        .expect("the delta still completes");
        assert_eq!(
            w.db()
                .get_app_config(REMOTE_DELTA_CURSOR_KEY)
                .unwrap()
                .as_deref(),
            Some("cursor-after"),
            "the racing delta saved its cursor"
        );
        assert!(!w.db().projection_state().unwrap().populated);

        assert_eq!(usable_delta_cursor(state).unwrap(), None);
        let mut seeded = 0;
        let outcome = cursor_or_seed(state, |s| {
            seeded += 1;
            finish_seed(s, &HashMap::new(), "cursor-seeded", Some(Vec::new()))?;
            Ok("cursor-seeded".to_string())
        })
        .unwrap();
        assert_eq!((outcome, seeded), (None, 1), "it seeded");

        assert!(w.db().projection_state().unwrap().populated);
        assert_eq!(
            usable_delta_cursor(state).unwrap().as_deref(),
            Some("cursor-seeded")
        );
    }

    /// With the switch OFF none of this applies: the cursor is used whatever the projection holds.
    ///
    /// Proof this test can fail: dropping the `projection_active` test from `projection_reseed_due`
    /// makes every switch-off cursor count as stale.
    #[test]
    fn with_the_switch_off_the_cursor_is_used_as_it_always_was() {
        let w = world(false, false);
        w.db()
            .set_app_config(REMOTE_DELTA_CURSOR_KEY, "c1")
            .unwrap();
        assert_eq!(
            usable_delta_cursor(&w.state).unwrap().as_deref(),
            Some("c1")
        );
        let mut seeded = false;
        let got = cursor_or_seed(&w.state, |_| {
            seeded = true;
            Ok(String::new())
        })
        .unwrap();
        assert_eq!((got.as_deref(), seeded), (Some("c1"), false));
        w.db().set_app_config(REMOTE_DELTA_CURSOR_KEY, "").unwrap();
        assert_eq!(usable_delta_cursor(&w.state).unwrap(), None);
        // And a failing first seed is the error it always was, with no backoff set aside.
        let failed = cursor_or_seed(&w.state, |_| Err(AppError::Network("down".into())));
        assert!(matches!(failed, Err(AppError::Network(_))), "{failed:?}");
        assert!(w.db().health().reseed_not_before.is_none());
    }

    // ── review round: the selective-sync filter is read inside the transaction ──────────────────

    /// A Save that commits after a writer decided to write but before its transaction opens is not
    /// missed: the flags follow the filter as it stands when the rows are written.
    ///
    /// Proof this test can fail: loading the filter before `write_projection` (the old
    /// `PrefixFilter::load(self)?` in `project_delta`) writes the child LIVE under the stale filter.
    #[test]
    fn a_save_just_before_the_transaction_is_honoured() {
        let w = world(true, true);
        BEFORE_TX_HOOK.with(|h| {
            *h.borrow_mut() = Some(Box::new(|db: &Db| {
                db.set_exclude_prefixes_csv("FCAP/sub").unwrap();
            }));
        });

        run(
            &w,
            vec![
                dir("/FCAP", FCAP),
                dir("/FCAP/sub", SUB),
                file("/FCAP/sub/b.txt", B, "r1"),
            ],
        );

        assert_eq!(row(w.db(), FCAP).state, STATE_LIVE);
        assert_eq!(row(w.db(), SUB).state, STATE_HIDDEN);
        assert_eq!(row(w.db(), B).state, STATE_HIDDEN);
    }

    // ── review round: the projection never pins normal sync ─────────────────────────────────────

    /// Four failures in a row replay (the cursor stays); the fifth gives up: the projection is emptied
    /// and unpopulated, the cursor ADVANCES, and the reseed waits out a backoff before it is due.
    ///
    /// Proof this test can fail: a threshold of 4 or 6 moves the give-up to the wrong attempt;
    /// returning the error instead of `Ok` after the clear pins the cursor; skipping `project_clear`
    /// leaves a populated projection that missed a batch; skipping `schedule_reseed` makes the reseed
    /// due at once.
    #[test]
    fn five_projection_failures_in_a_row_give_up_and_let_the_cursor_advance() {
        let w = world(true, true);
        let db = w.db();
        run(&w, inv1());
        db.set_app_config(REMOTE_DELTA_CURSOR_KEY, "cursor-before")
            .unwrap();
        sabotage(db, "b.txt");

        for attempt in 1..MAX_DELTA_FAILURES {
            assert!(try_run(&w, inv3()).is_err(), "attempt {attempt} replays");
            assert_eq!(
                db.get_app_config(REMOTE_DELTA_CURSOR_KEY)
                    .unwrap()
                    .as_deref(),
                Some("cursor-before")
            );
        }
        assert!(db.projection_state().unwrap().populated);

        try_run(&w, inv3()).expect("the last attempt gives up and succeeds");

        assert_eq!(
            db.get_app_config(REMOTE_DELTA_CURSOR_KEY)
                .unwrap()
                .as_deref(),
            Some("cursor-after"),
            "normal sync moved on"
        );
        assert!(!db.projection_state().unwrap().populated);
        assert_eq!(count(db, "fp_item"), 0);
        assert_eq!(
            usable_delta_cursor(&w.state).unwrap().as_deref(),
            Some("cursor-after"),
            "inside the backoff the cursor stays usable: sync is not held"
        );
        db.end_projection_reseed_backoff();
        assert_eq!(
            usable_delta_cursor(&w.state).unwrap(),
            None,
            "now it reseeds"
        );

        unsabotage(db);
        seed(&w, vec![dir("/FCAP", FCAP), file("/FCAP/b.txt", B, "r1")]).unwrap();
        assert!(db.projection_state().unwrap().populated);
        assert_eq!(
            db.health().reseed_failures,
            0,
            "a landed snapshot starts over"
        );
        assert!(db.health().reseed_not_before.is_none());
    }

    /// The count is of CONSECUTIVE failures: a success in between starts it over.
    ///
    /// Proof this test can fail: not resetting `delta_failures` on success gives up at the fifth
    /// failure overall.
    #[test]
    fn a_success_between_failures_resets_the_count() {
        let w = world(true, true);
        let db = w.db();
        run(&w, inv1());
        sabotage(db, "b.txt");
        for _ in 1..MAX_DELTA_FAILURES {
            assert!(try_run(&w, inv3()).is_err());
        }
        unsabotage(db);
        run(&w, inv2());
        sabotage(db, "b.txt");

        for attempt in 1..MAX_DELTA_FAILURES {
            assert!(
                try_run(&w, inv3()).is_err(),
                "attempt {attempt} after the success still replays"
            );
        }
        assert!(db.projection_state().unwrap().populated, "not given up");
    }

    /// A snapshot that fails empties the projection (a populated one must not outlive a cursor it no
    /// longer matches), does not hold the cursor back, and backs the reseed off.
    ///
    /// Proof this test can fail: dropping the `project_clear` in `note_snapshot_failed` leaves the stale
    /// projection populated; propagating the snapshot error from `finish_seed` leaves the cursor
    /// unset; dropping `schedule_reseed` makes the reseed due at once.
    #[test]
    fn a_failed_snapshot_empties_the_projection_and_does_not_hold_the_cursor() {
        let w = world(true, true);
        let db = w.db();
        run(&w, inv1());
        sabotage(db, "b.txt");

        seed(&w, vec![dir("/FCAP", FCAP), file("/FCAP/b.txt", B, "r1")])
            .expect("a projection failure is not a seed failure");

        assert_eq!(
            db.get_app_config(REMOTE_DELTA_CURSOR_KEY)
                .unwrap()
                .as_deref(),
            Some("cursor-seeded")
        );
        assert!(!db.projection_state().unwrap().populated);
        assert_eq!(
            count(db, "fp_item"),
            0,
            "no stale rows beside the new cursor"
        );
        assert_eq!(
            usable_delta_cursor(&w.state).unwrap().as_deref(),
            Some("cursor-seeded"),
            "backing off: sync goes on"
        );
        db.end_projection_reseed_backoff();
        assert_eq!(usable_delta_cursor(&w.state).unwrap(), None);
    }

    /// The reseed backoff doubles from a minute and stops at half an hour.
    ///
    /// Proof this test can fail: a constant wait, a missing cap or a missing doubling each break one
    /// bound.
    #[test]
    fn the_reseed_backoff_doubles_and_is_capped() {
        let mut health = ProjectionHealth::default();
        for k in 0..9u32 {
            let before = SystemTime::now();
            health.schedule_reseed();
            let wait = health
                .reseed_not_before
                .unwrap()
                .duration_since(before)
                .unwrap();
            let expected = Duration::from_secs((60u64 << k).min(30 * 60));
            assert!(
                wait >= expected && wait < expected + Duration::from_secs(5),
                "step {k}: {wait:?} vs {expected:?}"
            );
        }
    }

    // ── review round: tombstones are capped without dropping a deletion an anchor needs ─────────

    /// Past the cap the OLDEST tombstones go and `min_valid_seq` rises to the newest one dropped, so
    /// every anchor that could have needed a dropped deletion is expired and every other stays valid.
    /// Below the cap nothing is dropped and no anchor expires.
    ///
    /// Proof this test can fail: an off-by-one in the OFFSET keeps one tombstone too many or too few;
    /// not raising `min_valid_seq` leaves an anchor valid that needed a dropped deletion; deleting every
    /// tombstone (no `changed_seq <= bound`) fails the kept-count assertion.
    #[test]
    fn tombstones_past_the_cap_are_pruned_and_exactly_the_anchors_that_needed_them_expire() {
        TOMBSTONE_CAP_OVERRIDE.with(|c| c.set(Some(3)));
        let w = world(true, true);
        let db = w.db();
        let files: Vec<(String, String)> = (0..6)
            .map(|i| {
                (
                    format!("/f{i}.txt"),
                    format!("id:t0000000000000PRUNE{i:02}"),
                )
            })
            .collect();
        run(&w, files.iter().map(|(p, id)| file(p, id, "r1")).collect());
        let floor = db.projection_state().unwrap().min_valid_seq;
        let mut deleted_at: Vec<i64> = Vec::new();
        for (p, _) in &files[..3] {
            run(&w, vec![gone(p)]);
            deleted_at.push(seq(db));
        }
        assert_eq!(
            db.projection_state().unwrap().min_valid_seq,
            floor,
            "at the cap nothing is pruned and no anchor expires"
        );
        assert_eq!(
            all_rows(db)
                .iter()
                .filter(|r| r.state == STATE_DELETED)
                .count(),
            3
        );

        for (p, _) in &files[3..] {
            run(&w, vec![gone(p)]);
            deleted_at.push(seq(db));
        }

        let state = db.projection_state().unwrap();
        let kept: Vec<i64> = all_rows(db)
            .iter()
            .filter(|r| r.state == STATE_DELETED)
            .map(|r| r.seq)
            .collect();
        assert_eq!(kept.len(), 3, "the newest three tombstones");
        assert_eq!(
            state.min_valid_seq, deleted_at[2],
            "raised to the newest tombstone dropped"
        );
        // The property: no valid anchor needs a deletion that is gone.
        for anchor in state.min_valid_seq..=state.seq {
            for (i, &t) in deleted_at.iter().enumerate() {
                if t > anchor {
                    assert!(kept.contains(&t), "anchor {anchor} needs deletion #{i}");
                }
            }
        }
        // And the anchors that could have needed a dropped one are expired.
        assert!(anchor_expired(&state, state.generation, deleted_at[0] - 1));
        assert!(anchor_expired(&state, state.generation, deleted_at[2] - 1));
        assert!(!anchor_expired(&state, state.generation, deleted_at[2]));
        TOMBSTONE_CAP_OVERRIDE.with(|c| c.set(None));
    }

    // ── the sequential model: helpers ───────────────────────────────────────────────────────────

    /// `DropboxEntry` is not `Clone`; a test that applies a batch twice rebuilds it.
    fn dup(entries: &[DropboxEntry]) -> Vec<DropboxEntry> {
        entries
            .iter()
            .map(|e| DropboxEntry {
                tag: e.tag.clone(),
                path_display: e.path_display.clone(),
                path_lower: e.path_lower.clone(),
                content_hash: e.content_hash.clone(),
                rev: e.rev.clone(),
                server_modified: e.server_modified.clone(),
                size: e.size,
                id: e.id.clone(),
            })
            .collect()
    }

    fn id(n: u32) -> String {
        format!("id:t0000000000SEQ{n:08}")
    }

    /// The rows and the sequence, to compare before and after.
    fn picture(db: &Db) -> (Vec<R>, i64) {
        (all_rows(db), seq(db))
    }

    /// Applies the same batch a second time: the state is identical and the sequence did not move.
    fn assert_replay_changes_nothing(w: &World, batch: &[DropboxEntry], what: &str) {
        let before = picture(w.db());
        let signals = crate::file_provider::signals_requested_on_this_thread();
        run(w, dup(batch));
        assert_eq!(
            picture(w.db()),
            before,
            "{what}: a replay changed something"
        );
        assert_eq!(
            crate::file_provider::signals_requested_on_this_thread(),
            signals,
            "{what}: a replay signalled"
        );
    }

    fn state_of(w: &World, dropbox_id: &str) -> i64 {
        row(w.db(), dropbox_id).state
    }

    // ── the sequential model: the captured orders ───────────────────────────────────────────────

    /// The five shapes of the live capture, in the order they happened, each applied as one batch
    /// through `run_remote_delta`: nested create, folder rename, file move, empty folder delete, delete
    /// of a folder with content. After each one the projection is where Dropbox's tree is, the ids never
    /// change, and applying the batch again changes nothing.
    ///
    /// Proof this test can fail: dropping the `rows_under` pass or the tombstone of `Sequence::delete`
    /// breaks the last two steps; applying the deletes of a batch after its upserts deletes the renamed
    /// folder and its child in step 4; always minting a new identity in `upsert` breaks the "same ids"
    /// assertions.
    #[test]
    fn the_captured_invocations_in_order_end_where_dropboxs_tree_does() {
        let w = world(true, true);
        run(&w, inv1());
        run(&w, inv2());
        run(&w, inv3());
        let ids = (
            row(w.db(), FCAP).item_id,
            row(w.db(), SUB).item_id,
            row(w.db(), B).item_id,
        );
        let identities = count(w.db(), "fp_identity");

        // 4: the folder rename.
        run(&w, inv4());
        let (fcap, sub, b) = (row(w.db(), FCAP), row(w.db(), SUB), row(w.db(), B));
        assert_eq!((fcap.item_id, sub.item_id, b.item_id), ids, "same ids");
        assert_eq!(
            (sub.path.as_str(), sub.parent),
            ("/fcap/sub2", fcap.item_id)
        );
        assert_eq!(
            (b.path.as_str(), b.parent),
            ("/fcap/sub2/b.txt", sub.item_id)
        );
        assert!(all_rows(w.db()).iter().all(|r| r.state == STATE_LIVE));
        assert_replay_changes_nothing(&w, &inv4(), "inv4");

        // 6: the file moves up into FCAP.
        run(&w, inv6());
        let (fcap, sub, b) = (row(w.db(), FCAP), row(w.db(), SUB), row(w.db(), B));
        assert_eq!((b.path.as_str(), b.parent), ("/fcap/b.txt", fcap.item_id));
        assert_eq!((sub.state, b.state), (STATE_LIVE, STATE_LIVE));
        assert_replay_changes_nothing(&w, &inv6(), "inv6");

        // 8: the empty folder is deleted.
        run(&w, inv8());
        assert_eq!(state_of(&w, SUB), STATE_DELETED);
        assert_eq!(
            (state_of(&w, FCAP), state_of(&w, B)),
            (STATE_LIVE, STATE_LIVE)
        );
        assert_replay_changes_nothing(&w, &inv8(), "inv8");

        // 9: the folder with content is deleted, child first.
        run(&w, inv9());
        assert!(
            all_rows(w.db()).iter().all(|r| r.state == STATE_DELETED),
            "{:?}",
            states(w.db())
        );
        assert_replay_changes_nothing(&w, &inv9(), "inv9");
        assert_eq!(count(w.db(), "fp_identity"), identities, "no new identity");
    }

    // ── the sequential model: the phantoms the batch model left live ────────────────────────────

    /// A NEW item upserted and then deleted in the same batch is deleted (the batch model left it live
    /// forever).
    ///
    /// Proof this test can fail: making `Sequence::delete` skip rows that this batch upserted leaves
    /// the item live.
    #[test]
    fn a_new_item_upserted_and_then_deleted_in_one_batch_is_deleted() {
        let w = world(true, true);
        run(&w, vec![dir("/FCAP", FCAP)]);
        let batch = vec![file("/FCAP/n.txt", B, "r1"), gone("/FCAP/n.txt")];
        run(&w, dup(&batch));
        assert_eq!(state_of(&w, B), STATE_DELETED);
        assert_eq!(state_of(&w, FCAP), STATE_LIVE);
        assert_replay_changes_nothing(&w, &batch, "new then deleted");
    }

    /// A MOVED-IN item (it existed elsewhere) upserted at its new path and then deleted there is
    /// deleted.
    ///
    /// Proof this test can fail: the same skip in `Sequence::delete`; or resolving a delete through
    /// the item's path BEFORE the batch (not the path the upsert gave it) leaves it live at the new one.
    #[test]
    fn a_moved_in_item_upserted_and_then_deleted_in_one_batch_is_deleted() {
        let w = world(true, true);
        for e in [inv1(), inv2(), inv3()] {
            run(&w, e);
        }
        let batch = vec![file("/FCAP/m.txt", B, "r2"), gone("/FCAP/m.txt")];
        run(&w, dup(&batch));
        let b = row(w.db(), B);
        assert_eq!((b.state, b.path.as_str()), (STATE_DELETED, "/fcap/m.txt"));
        assert_eq!(state_of(&w, SUB), STATE_LIVE, "nothing else went");
        assert_replay_changes_nothing(&w, &batch, "moved in then deleted");
    }

    /// Two different ids upserted at one path: the later entry replaces the earlier one, in both
    /// orders (the batch model left both live).
    ///
    /// Proof this test can fail: removing the occupant loop from `upsert` leaves both live.
    #[test]
    fn two_ids_at_one_path_leave_only_the_later_one_live() {
        for a_first in [true, false] {
            let w = world(true, true);
            let (a, b) = (id(1), id(2));
            let mut batch = vec![file("/x.txt", &a, "r1"), file("/x.txt", &b, "r1")];
            let (first, last) = if a_first { (&a, &b) } else { (&b, &a) };
            if !a_first {
                batch.reverse();
            }
            run(&w, dup(&batch));
            assert_eq!(state_of(&w, first), STATE_DELETED, "a_first={a_first}");
            assert_eq!(state_of(&w, last), STATE_LIVE, "a_first={a_first}");
            assert_replay_changes_nothing(&w, &batch, "two ids");
        }
    }

    /// Orphan healing never brings a tombstone back: an orphan that is deleted while it waits for its
    /// folder stays deleted when the folder arrives. (It keeps `parent_item = -1`, which is what a
    /// careless healing query selects.)
    ///
    /// Proof this test can fail: selecting `parent_item = -1` instead of `state = orphan` in
    /// `heal_orphans` resurrects the deleted orphan, here and in the folder-delete variant.
    #[test]
    fn healing_never_revives_a_deleted_orphan() {
        let (c, p) = (id(3), id(4));
        for via_folder_delete in [false, true] {
            let w = world(true, true);
            run(&w, vec![file("/P/c.txt", &c, "r1")]);
            assert_eq!(state_of(&w, &c), STATE_ORPHAN);
            run(
                &w,
                vec![gone(if via_folder_delete { "/P" } else { "/P/c.txt" })],
            );
            assert_eq!(state_of(&w, &c), STATE_DELETED);

            run(&w, vec![dir("/P", &p)]);

            assert_eq!(
                state_of(&w, &c),
                STATE_DELETED,
                "via_folder_delete={via_folder_delete}"
            );
            assert_eq!(state_of(&w, &p), STATE_LIVE);
        }
    }

    // ── the sequential model: replay ────────────────────────────────────────────────────────────

    /// The replay property over every shape this module reasons about: from the same starting point,
    /// applying a batch a second time leaves the identical rows, the same sequence and no signal.
    ///
    /// Proof this test can fail: stamping every touched row regardless of its pre-image (dropping the
    /// `unchanged` test in `Sequence::finish`) bumps the sequence on every replay; making a tombstone
    /// revive its row on the second pass breaks the replacement cases.
    #[test]
    fn replaying_any_batch_ends_in_the_same_state_and_bumps_nothing() {
        type Case = (&'static str, Vec<Vec<DropboxEntry>>, Vec<DropboxEntry>);
        let base = || vec![inv1(), inv2(), inv3()];
        let (x, c, n) = (id(10), id(11), id(12));
        let cases: Vec<Case> = vec![
            ("rename", base(), inv4()),
            (
                "rename, child not re-listed",
                base(),
                vec![gone("/FCAP/sub"), dir("/FCAP/sub2", SUB)],
            ),
            (
                "file move",
                base(),
                vec![gone("/FCAP/sub/b.txt"), file("/FCAP/b.txt", B, "r9")],
            ),
            (
                "edit then delete",
                base(),
                vec![file("/FCAP/sub/b.txt", B, "r2"), gone("/FCAP/sub/b.txt")],
            ),
            (
                "delete then re-add",
                base(),
                vec![gone("/FCAP/sub/b.txt"), file("/FCAP/sub/b.txt", B, "r2")],
            ),
            ("folder delete", base(), vec![gone("/FCAP/sub")]),
            (
                "file replaces folder",
                vec![vec![dir("/X", &x), file("/X/c.txt", &c, "r1")]],
                vec![file("/X", &n, "r2")],
            ),
            (
                "folder replaces folder",
                vec![vec![dir("/X", &x), file("/X/c.txt", &c, "r1")]],
                vec![dir("/X", &n), file("/X/c.txt", &c, "r2")],
            ),
            (
                "two ids at one path",
                vec![],
                vec![file("/x.txt", &x, "r1"), file("/x.txt", &n, "r1")],
            ),
            (
                "new then deleted",
                base(),
                vec![file("/FCAP/n.txt", &n, "r1"), gone("/FCAP/n.txt")],
            ),
            (
                "swap of two folders",
                vec![vec![dir("/A", &x), dir("/B", &n)]],
                vec![dir("/B", &x), dir("/A", &n)],
            ),
            (
                "orphan then folder",
                vec![vec![file("/P/c.txt", &c, "r1")]],
                vec![dir("/P", &x)],
            ),
            (
                "case-only rename",
                vec![vec![dir("/Docs", &x), file("/Docs/a.txt", &c, "r1")]],
                vec![gone("/Docs"), dir("/DOCS", &x)],
            ),
        ];
        for (what, setup, batch) in cases {
            let w = world(true, true);
            for step in setup {
                run(&w, step);
            }
            run(&w, dup(&batch));
            assert!(count(w.db(), "fp_item") > 0);
            assert_replay_changes_nothing(&w, &batch, what);
            assert_replay_changes_nothing(&w, &batch, what);
        }
    }

    /// A row that moves twice inside one batch takes ONE sequence value, and a new row that is
    /// upserted twice takes one: nothing is stamped until the batch is done.
    ///
    /// Proof this test can fail: stamping inside `Sequence::put` (one value per write) takes two.
    #[test]
    fn a_row_written_twice_in_a_batch_takes_one_value() {
        let w = world(true, true);
        for e in [inv1(), inv2(), inv3()] {
            run(&w, e);
        }
        let before = seq(w.db());
        run(
            &w,
            vec![
                file("/FCAP/one.txt", B, "r2"),
                file("/FCAP/two.txt", B, "r3"),
            ],
        );
        assert_eq!(seq(w.db()) - before, 1, "an existing row, moved twice");
        assert_eq!(row(w.db(), B).path, "/fcap/two.txt");

        let before = seq(w.db());
        run(
            &w,
            vec![
                file("/new.txt", &id(20), "r1"),
                file("/new2.txt", &id(20), "r2"),
            ],
        );
        assert_eq!(seq(w.db()) - before, 1, "a new row, upserted twice");
    }

    /// A tombstone that stays a tombstone takes no value, whatever the batch wrote into it on the way.
    ///
    /// Proof this test can fail: dropping the "both deleted" clause of `Sequence::finish` stamps the
    /// row, because its rev differs from the stored one.
    #[test]
    fn a_tombstone_that_stays_a_tombstone_takes_no_value() {
        let w = world(true, true);
        run(&w, vec![dir("/FCAP", FCAP), file("/FCAP/a.txt", B, "r1")]);
        run(&w, vec![gone("/FCAP/a.txt")]);
        let before = picture(w.db());

        run(&w, vec![file("/FCAP/a.txt", B, "r7"), gone("/FCAP/a.txt")]);

        assert_eq!(seq(w.db()), before.1);
        assert_eq!(state_of(&w, B), STATE_DELETED);
    }

    /// An id listed twice ends at its LAST entry.
    ///
    /// Proof this test can fail: indexing the batch by id and keeping the first entry leaves it at `/a`.
    #[test]
    fn an_id_listed_twice_ends_at_its_last_entry() {
        let w = world(true, true);
        run(&w, vec![file("/a.txt", B, "r1"), file("/b.txt", B, "r2")]);
        let b = row(w.db(), B);
        assert_eq!((b.path.as_str(), b.rev.as_str()), ("/b.txt", "r2"));
        assert_eq!(count(w.db(), "fp_item"), 1);
    }

    // ── the sequential model: unlisted descendants of a re-added folder ─────────────────────────

    /// After the delete swept a folder up, a child listed under it has no parent: it is an orphan and
    /// not a child of a tombstone. (The batch model's "a child implies its parent" rule is gone: the
    /// entries are applied as they come.) Whatever the batch does not list again stays deleted.
    ///
    /// Proof this test can fail: letting `folder_at` return tombstones (dropping `state != 2`) parents
    /// the child to the deleted folder and lists it.
    #[test]
    fn a_child_listed_under_a_folder_deleted_earlier_in_the_batch_is_an_orphan() {
        let w = world(true, true);
        let (p, c, d) = (id(110), id(111), id(112));
        run(
            &w,
            vec![
                dir("/P", &p),
                file("/P/c.txt", &c, "r1"),
                file("/P/d.txt", &d, "r1"),
            ],
        );

        run(&w, vec![gone("/P"), file("/P/c.txt", &c, "r2")]);

        assert_eq!(state_of(&w, &p), STATE_DELETED);
        let c = row(w.db(), &c);
        assert_eq!((c.state, c.parent), (STATE_ORPHAN, NO_PARENT));
        assert_eq!(state_of(&w, &d), STATE_DELETED, "not listed again");
    }

    fn stored(n: i64) -> StoredRow {
        StoredRow {
            item_id: n,
            parent_item: 1,
            path_lower: "/p/a".into(),
            name: "a".into(),
            is_folder: false,
            size_bytes: 5,
            modified_ts: 7,
            content_hash: "h".into(),
            rev: "r".into(),
            content_version: b"c".to_vec(),
            meta_version: b"m".to_vec(),
            downloaded: false,
            state: STATE_LIVE,
        }
    }

    /// `restates` is true only when NOTHING would change, and `same_observable` only when nothing the
    /// extension sees changed: every single field difference breaks the first, and every field but the
    /// bookkeeping path breaks the second.
    ///
    /// Proof this test can fail: dropping any one comparison from either function.
    #[test]
    fn restates_and_same_observable_notice_every_field() {
        let base = stored(1);
        let new = |f: &dyn Fn(&mut StoredRow)| {
            let mut r = base.clone();
            f(&mut r);
            r
        };
        let as_new = |r: &StoredRow| NewRow {
            item_id: r.item_id,
            parent_item: r.parent_item,
            path_lower: Box::leak(r.path_lower.clone().into_boxed_str()),
            name: Box::leak(r.name.clone().into_boxed_str()),
            is_folder: r.is_folder,
            size_bytes: r.size_bytes,
            modified_ts: r.modified_ts,
            content_hash: Box::leak(r.content_hash.clone().into_boxed_str()),
            rev: Box::leak(r.rev.clone().into_boxed_str()),
            state: r.state,
        };
        assert!(restates(&base, &as_new(&base)));
        assert!(same_observable(&base, &base));
        let changes: Vec<(&str, StoredRow, bool)> = vec![
            ("parent", new(&|r| r.parent_item = 2), true),
            ("path", new(&|r| r.path_lower = "/q/a".into()), false),
            ("name", new(&|r| r.name = "b".into()), true),
            ("kind", new(&|r| r.is_folder = true), true),
            ("size", new(&|r| r.size_bytes = 6), true),
            ("mtime", new(&|r| r.modified_ts = 8), true),
            ("hash", new(&|r| r.content_hash = "g".into()), true),
            ("rev", new(&|r| r.rev = "s".into()), true),
            ("state", new(&|r| r.state = STATE_HIDDEN), true),
            (
                "content version",
                new(&|r| r.content_version = b"d".to_vec()),
                true,
            ),
            (
                "meta version",
                new(&|r| r.meta_version = b"n".to_vec()),
                true,
            ),
        ];
        for (what, changed, observable) in changes {
            if what != "content version" && what != "meta version" {
                assert!(!restates(&base, &as_new(&changed)), "restates: {what}");
            }
            assert_eq!(
                !same_observable(&base, &changed),
                observable,
                "observable: {what}"
            );
        }
    }

    // ── the sequential model: case, normalization, selective sync ───────────────────────────────

    /// A case-only rename, in both shapes Dropbox can send: an in-place upsert, and a delete followed
    /// by a re-add that lists the child again. The folder keeps its id and takes the new spelling;
    /// its child is untouched and takes no value; the stored path (folded) does not change.
    ///
    /// Proof this test can fail: comparing raw instead of folded paths in `upsert` treats the new
    /// spelling as another path and moves (or displaces) the folder; not listing the child again would
    /// leave it deleted in the second shape.
    #[test]
    fn a_case_only_rename_keeps_the_ids_and_stamps_only_the_folder() {
        for delete_and_readd in [false, true] {
            let w = world(true, true);
            let (d, c) = (id(70), id(71));
            run(&w, vec![dir("/Docs", &d), file("/Docs/a.txt", &c, "r1")]);
            let (d0, c0) = (row(w.db(), &d), row(w.db(), &c));
            let batch = if delete_and_readd {
                vec![
                    gone("/Docs"),
                    dir("/DOCS", &d),
                    file("/DOCS/a.txt", &c, "r1"),
                ]
            } else {
                vec![dir("/DOCS", &d)]
            };

            run(&w, dup(&batch));

            let (d1, c1) = (row(w.db(), &d), row(w.db(), &c));
            assert_eq!((d1.name.as_str(), d1.path.as_str()), ("DOCS", "/docs"));
            assert_eq!(d1.item_id, d0.item_id);
            assert_eq!(d1.state, STATE_LIVE);
            assert!(d1.seq > d0.seq, "the folder's name changed");
            assert_eq!(c1, c0, "the child is untouched, not even stamped");
            assert_replay_changes_nothing(&w, &batch, "case-only rename");
        }
    }

    /// An NFD re-add of an NFC folder after an NFD delete is the same item at the same folded path:
    /// same id, the re-listed child back, no tombstone left.
    ///
    /// Proof this test can fail: removing `fold` from `normalize_path_lower` makes the NFD delete miss
    /// the NFC row, so the folder is never tombstoned and the re-listed rows are written as changes
    /// (the child takes a new stamp).
    #[test]
    fn an_nfd_delete_and_readd_of_an_nfc_folder_is_the_same_item() {
        let w = world(true, true);
        let (d, c) = (id(80), id(81));
        run(
            &w,
            vec![
                dir(&format!("/{NFC_DIR}"), &d),
                file(&format!("/{NFC_DIR}/a.txt"), &c, "r1"),
            ],
        );
        let before = all_rows(w.db());

        run(
            &w,
            vec![
                gone(&format!("/{NFD_DIR}")),
                dir(&format!("/{NFD_DIR}"), &d),
                file(&format!("/{NFD_DIR}/a.txt"), &c, "r1"),
            ],
        );

        let after = all_rows(w.db());
        assert_eq!(after.len(), before.len());
        assert!(after.iter().all(|r| r.state == STATE_LIVE));
        assert_eq!(row(w.db(), &c).path, "/caf\u{e9}/a.txt");
        assert_eq!(
            row(w.db(), &c),
            before.iter().find(|r| r.dropbox_id == c).unwrap().clone()
        );
    }

    /// A folder moved into an excluded path is hidden, and so is a child that is listed there; moved out
    /// again, both are listed again. Each by its own entry.
    ///
    /// Proof this test can fail: giving a moved row the state it had (skipping `visibility` in `place`)
    /// leaves the folder or the child listed inside an excluded folder.
    #[test]
    fn a_folder_moved_into_an_excluded_path_is_hidden_with_its_listed_children() {
        let w = world(true, true);
        w.db().set_exclude_prefixes_csv("ex").unwrap();
        let (ex, s, c) = (id(90), id(91), id(92));
        run(
            &w,
            vec![
                dir("/src", &s),
                file("/src/c.txt", &c, "r1"),
                dir("/ex", &ex),
            ],
        );
        assert_eq!(
            (state_of(&w, &s), state_of(&w, &c), state_of(&w, &ex)),
            (STATE_LIVE, STATE_LIVE, STATE_HIDDEN)
        );

        let into = vec![dir("/ex/src", &s), file("/ex/src/c.txt", &c, "r1")];
        run(&w, dup(&into));
        assert_eq!(state_of(&w, &s), STATE_HIDDEN);
        assert_eq!(state_of(&w, &c), STATE_HIDDEN);
        assert_replay_changes_nothing(&w, &into, "moved into excluded");

        run(&w, vec![dir("/src", &s), file("/src/c.txt", &c, "r1")]);
        assert_eq!(state_of(&w, &s), STATE_LIVE);
        assert_eq!(state_of(&w, &c), STATE_LIVE);
    }

    /// A row upserted under an excluded path is hidden from the start, and an orphan that heals under
    /// one is hidden too (the heal case is `a_healed_orphan_follows_selective_sync`).
    ///
    /// Proof this test can fail: hardcoding `STATE_LIVE` in `Sequence::place` lists the new row.
    #[test]
    fn a_new_item_under_an_excluded_path_is_hidden_on_arrival() {
        let w = world(true, true);
        w.db().set_exclude_prefixes_csv("ex").unwrap();
        let (ex, c) = (id(95), id(96));
        run(&w, vec![dir("/ex", &ex), file("/ex/c.txt", &c, "r1")]);
        assert_eq!(state_of(&w, &ex), STATE_HIDDEN);
        assert_eq!(state_of(&w, &c), STATE_HIDDEN);
    }

    // ── review round: a failing reseed listing, and the reseed deadline ─────────────────────────

    /// A reseed whose LISTING fails, with a cursor stored, must not pin normal sync: the stored cursor
    /// is used, the reseed is backed off (so the next iteration does not list again at once), and
    /// after the backoff it tries again.
    ///
    /// Proof this test can fail: returning the seed's error (`seed(state)?`) fails the first call;
    /// dropping `schedule_projection_reseed` makes the second call list again.
    #[test]
    fn a_failing_reseed_listing_does_not_pin_normal_sync() {
        let w = world(true, true);
        run(&w, inv1());
        w.db()
            .set_app_config(REMOTE_DELTA_CURSOR_KEY, "cursor-stored")
            .unwrap();
        w.db().project_clear().unwrap();
        assert_eq!(
            usable_delta_cursor(&w.state).unwrap(),
            None,
            "a reseed is due"
        );

        let listings = std::cell::Cell::new(0);
        let failing = |_: &AppState| -> crate::error::AppResult<String> {
            listings.set(listings.get() + 1);
            Err(AppError::Network("listing failed".into()))
        };
        let got = cursor_or_seed(&w.state, failing).unwrap();
        assert_eq!(got.as_deref(), Some("cursor-stored"), "sync goes on");

        let got = cursor_or_seed(&w.state, failing).unwrap();
        assert_eq!(got.as_deref(), Some("cursor-stored"));
        assert_eq!(
            listings.get(),
            1,
            "backed off: the second call did not list"
        );

        w.db().end_projection_reseed_backoff();
        let got = cursor_or_seed(&w.state, failing).unwrap();
        assert_eq!(got.as_deref(), Some("cursor-stored"));
        assert_eq!(listings.get(), 2, "after the backoff it tries again");
    }

    /// With NO stored cursor a failing seed has nothing to fall back on: the error is returned, as it
    /// always was, and no backoff is set.
    ///
    /// Proof this test can fail: returning `Ok(Some(..))` or `Ok(None)` on every failure would swallow
    /// the first-ever seed's error.
    #[test]
    fn with_no_stored_cursor_a_failing_seed_is_still_an_error() {
        let w = world(true, false);
        let got = cursor_or_seed(&w.state, |_| Err(AppError::Network("down".into())));
        assert!(matches!(got, Err(AppError::Network(_))), "{got:?}");
        assert!(w.db().health().reseed_not_before.is_none());
    }

    /// The reseed deadline, through the real `usable_delta_cursor` and without
    /// `end_projection_reseed_backoff`: a deadline in the past is due, one ahead is not, and one
    /// further ahead than any backoff can be (the clock was set back) is due again.
    ///
    /// Proof this test can fail: flipping the comparison in `reseed_is_due` swaps the first two rows;
    /// dropping the clock-back clause fails the third.
    #[test]
    fn the_reseed_deadline_is_compared_against_the_wall_clock() {
        let w = world(true, false);
        w.db()
            .set_app_config(REMOTE_DELTA_CURSOR_KEY, "c1")
            .unwrap();
        let hour = Duration::from_secs(3600);
        let at = |t: Option<SystemTime>| {
            w.db().health().reseed_not_before = t;
            usable_delta_cursor(&w.state).unwrap()
        };
        let now = SystemTime::now();

        assert_eq!(at(Some(now - hour)), None, "past: due");
        assert_eq!(
            at(Some(now + Duration::from_secs(600))).as_deref(),
            Some("c1"),
            "ahead, within a backoff: not due"
        );
        assert_eq!(
            at(Some(now + hour)),
            None,
            "further than any backoff: clock set back, due"
        );
        assert_eq!(at(None), None, "no backoff: due");
    }

    /// The same rule at its edges, with an explicit `now`.
    ///
    /// Proof this test can fail: `>` for `>=` on the boundary, or `>=` for `>` on the cap, fails one row.
    #[test]
    fn the_reseed_rule_at_its_edges() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let ns = Duration::from_nanos(1);
        assert!(reseed_is_due(None, now));
        assert!(reseed_is_due(Some(now), now), "exactly now is due");
        assert!(reseed_is_due(Some(now - ns), now));
        assert!(
            !reseed_is_due(Some(now + ns), now),
            "an instant ahead is not"
        );
        assert!(
            !reseed_is_due(Some(now + RESEED_BACKOFF_MAX), now),
            "the longest backoff is still a backoff"
        );
        assert!(
            reseed_is_due(Some(now + RESEED_BACKOFF_MAX + ns), now),
            "past the longest backoff: the clock went back"
        );
    }

    // ── the contract model: the two orders of a rename, orphans, known ids ──────────────────────

    /// Two children under `sub`, so a rename can leave one of them unlisted.
    const C2: &str = "id:t00000000000FCAPFILE04";

    fn two_children() -> World {
        let w = world(true, true);
        for e in [inv1(), inv2(), inv3()] {
            run(&w, e);
        }
        run(&w, vec![file("/FCAP/sub/c.txt", C2, "rev-ccc1")]);
        w
    }

    /// Deletes FIRST (the order the live capture showed): `deleted /sub` tombstones the subtree, the
    /// re-add restores the folder, and a child that is not re-listed stays deleted. A re-listed child
    /// moves with its own entry. No inference.
    ///
    /// Proof this test can fail: reviving the unlisted `c.txt` (any carry or revival) fails the last
    /// assertion; not updating a tombstoned id in place leaves `b.txt` deleted.
    #[test]
    fn a_rename_with_the_deletes_first_leaves_an_unlisted_child_deleted() {
        let w = two_children();
        let batch = vec![
            gone("/FCAP/sub"),
            dir("/FCAP/sub2", SUB),
            file("/FCAP/sub2/b.txt", B, "rev-bbb2"),
        ];
        run(&w, dup(&batch));

        let (sub, b, c) = (row(w.db(), SUB), row(w.db(), B), row(w.db(), C2));
        assert_eq!((sub.state, sub.path.as_str()), (STATE_LIVE, "/fcap/sub2"));
        assert_eq!(
            (b.state, b.path.as_str(), b.parent),
            (STATE_LIVE, "/fcap/sub2/b.txt", sub.item_id)
        );
        assert_eq!(c.state, STATE_DELETED, "not re-listed: still deleted");
        assert_replay_changes_nothing(&w, &batch, "deletes first");
    }

    /// Deletes LAST: the folder and the re-listed child move through their own entries, so the trailing
    /// `deleted /sub` finds nothing of theirs. A child that was not re-listed still sits at the old
    /// path under the old parent id, so the trailing delete sweeps it; without that delete it keeps its
    /// parent id and its path. Neither order moves a row nobody listed.
    ///
    /// Proof this test can fail: a carry moves `c.txt` to `sub2` in the first half; a delete that
    /// tombstones through a row's pre-batch path kills `b.txt` in the second.
    #[test]
    fn a_rename_with_the_deletes_last_sweeps_what_still_sits_at_the_old_paths() {
        let moves = || {
            vec![
                dir("/FCAP/sub2", SUB),
                file("/FCAP/sub2/b.txt", B, "rev-bbb2"),
            ]
        };
        // Without a trailing delete the unlisted child keeps its parent id and its path.
        let w = two_children();
        run(&w, moves());
        let (sub, c) = (row(w.db(), SUB), row(w.db(), C2));
        assert_eq!(
            (c.state, c.path.as_str(), c.parent),
            (STATE_LIVE, "/fcap/sub/c.txt", sub.item_id)
        );

        // With the trailing deletes the old paths are swept, and only they.
        let w = two_children();
        let mut batch = moves();
        batch.extend([gone("/FCAP/sub/b.txt"), gone("/FCAP/sub")]);
        run(&w, dup(&batch));
        let (sub, b, c) = (row(w.db(), SUB), row(w.db(), B), row(w.db(), C2));
        assert_eq!((sub.state, sub.path.as_str()), (STATE_LIVE, "/fcap/sub2"));
        assert_eq!((b.state, b.path.as_str()), (STATE_LIVE, "/fcap/sub2/b.txt"));
        assert_eq!(c.state, STATE_DELETED, "swept at the old path");
        assert_replay_changes_nothing(&w, &batch, "deletes last");
    }

    /// An id the projection already knows is updated in place whatever state it is in: a tombstone is
    /// revived by its own entry, a hidden row becomes live, an orphan finds its place.
    ///
    /// Proof this test can fail: skipping known ids in `Sequence::upsert` (minting a new identity)
    /// leaves the old row in its state and adds a second row.
    #[test]
    fn a_known_id_is_updated_in_place_in_any_state() {
        let w = world(true, true);
        w.db().set_exclude_prefixes_csv("ex").unwrap();
        let (ex, deleted, hidden, orphan) = (id(150), id(151), id(152), id(153));
        run(
            &w,
            vec![
                dir("/ex", &ex),
                file("/ex/h.txt", &hidden, "r1"),
                file("/d.txt", &deleted, "r1"),
                file("/NOPE/o.txt", &orphan, "r1"),
            ],
        );
        run(&w, vec![gone("/d.txt")]);
        assert_eq!(
            (
                state_of(&w, &deleted),
                state_of(&w, &hidden),
                state_of(&w, &orphan)
            ),
            (STATE_DELETED, STATE_HIDDEN, STATE_ORPHAN)
        );
        let ids = count(w.db(), "fp_identity");
        let items = ["/d2.txt", "/h2.txt", "/o2.txt"];

        run(
            &w,
            vec![
                file(items[0], &deleted, "r2"),
                file(items[1], &hidden, "r2"),
                file(items[2], &orphan, "r2"),
            ],
        );

        for (i, id) in [&deleted, &hidden, &orphan].into_iter().enumerate() {
            let r = row(w.db(), id);
            assert_eq!((r.state, r.parent), (STATE_LIVE, ROOT_ITEM), "{id}");
            assert_eq!(r.path, items[i]);
        }
        assert_eq!(count(w.db(), "fp_identity"), ids, "no identity minted");
        assert_eq!(count(w.db(), "fp_item"), 4);
    }

    /// A folder at state ORPHAN is no parent: what is listed under it is an orphan too, so a live row
    /// never has a non-live parent. When the folder's own parent arrives the whole chain heals.
    ///
    /// Proof this test can fail: letting `folder_at` return orphans (state != 2 only) places `x.txt` live
    /// under the orphan folder `b` and the end-of-batch demotion has to take it back (a third write).
    /// The end state is the same either way, which is why the check is a write count.
    #[test]
    fn a_child_of_an_orphan_folder_is_an_orphan_and_heals_with_it() {
        let w = world(true, true);
        let (a, b, x) = (id(160), id(161), id(162));
        count_item_writes(w.db());
        run(&w, vec![dir("/a/b", &b), file("/a/b/x.txt", &x, "r1")]);
        // Placed as orphans at once: two inserts, and no demotion churn afterwards.
        assert_eq!(item_writes(w.db()), 2);
        assert_eq!(state_of(&w, &b), STATE_ORPHAN);
        let xr = row(w.db(), &x);
        assert_eq!((xr.state, xr.parent), (STATE_ORPHAN, NO_PARENT));

        run(&w, vec![dir("/a", &a)]);

        let (ra, rb, rx) = (row(w.db(), &a), row(w.db(), &b), row(w.db(), &x));
        assert_eq!((rb.state, rb.parent), (STATE_LIVE, ra.item_id));
        assert_eq!((rx.state, rx.parent), (STATE_LIVE, rb.item_id));
    }

    /// A chain of thirty orphans listed deepest first heals in ONE batch when the top arrives.
    ///
    /// Proof this test can fail: a heal that makes one pass in listing order, without sorting by path
    /// and without repeating, heals only the level under the new folder.
    #[test]
    fn a_deep_orphan_chain_heals_in_one_batch() {
        const DEPTH: u32 = 30;
        let w = world(true, true);
        let mut chain = Vec::new();
        let mut path = "/top".to_string();
        for k in 1..=DEPTH {
            path = format!("{path}/n{k}");
            chain.push(dir(&path, &id(300 + k)));
        }
        chain.push(file(&format!("{path}/leaf.txt"), &id(400), "r1"));
        chain.reverse();
        run(&w, chain);
        assert!(all_rows(w.db()).iter().all(|r| r.state == STATE_ORPHAN));

        run(&w, vec![dir("/top", &id(300))]);

        assert!(
            all_rows(w.db()).iter().all(|r| r.state == STATE_LIVE),
            "{:?}",
            states(w.db())
        );
        for k in 1..=DEPTH {
            let parent = row(w.db(), &id(300 + k - 1)).item_id;
            assert_eq!(row(w.db(), &id(300 + k)).parent, parent, "level {k}");
        }
        let leaf = row(w.db(), &id(400));
        assert_eq!(leaf.parent, row(w.db(), &id(300 + DEPTH)).item_id);
    }

    /// A live folder that its own entry moves under a parent that does not exist becomes an orphan. The
    /// rows still hanging under it are demoted with it (a live row never has a non-live parent), and
    /// then, in the same batch, adopted by the live folder that now stands at their path.
    ///
    /// Proof this test can fail: dropping `demote_pass` leaves `x.txt` live under an orphan; running
    /// `heal_pass` once instead of until nothing moves leaves `y.txt` an orphan.
    #[test]
    fn rows_left_under_a_folder_that_became_an_orphan_find_the_folder_at_their_path() {
        let w = world(true, true);
        let (p, f, x, y, g) = (id(170), id(171), id(172), id(173), id(174));
        run(
            &w,
            vec![
                dir("/p", &p),
                dir("/p/f", &f),
                file("/p/f/x.txt", &x, "r1"),
                file("/p/f/y.txt", &y, "r1"),
            ],
        );

        // F moves under /q, which is not there; G takes F's old path. x.txt and y.txt still hang under
        // F, so they are demoted, and then healed under G by their paths.
        let batch = vec![dir("/q/f", &f), dir("/p/f", &g)];
        run(&w, dup(&batch));
        assert_eq!(state_of(&w, &f), STATE_ORPHAN);
        let rg = row(w.db(), &g);
        assert_eq!((rg.state, rg.parent), (STATE_LIVE, row(w.db(), &p).item_id));
        for child in [&x, &y] {
            let r = row(w.db(), child);
            assert_eq!((r.state, r.parent), (STATE_LIVE, rg.item_id), "{child}");
        }
        assert_replay_changes_nothing(&w, &batch, "demote and heal");
    }

    /// A folder that replaces a folder while its own parent is not at the path any more (the parent moved
    /// by its own entry, and nothing carried the folder along) is an orphan, so the children it is
    /// handed are orphans too and not live rows under an orphan parent.
    ///
    /// Proof this test can fail: handing the children over as live (dropping the `state == STATE_ORPHAN`
    /// branch of the adoption loop in `upsert`) puts `c.txt` live under the orphan folder until the
    /// end-of-batch demotion takes it back, which costs a write: the end state is the same, the count
    /// is not.
    #[test]
    fn a_folder_replacing_a_folder_while_an_orphan_hands_its_children_over_as_orphans() {
        let w = world(true, true);
        let (a, r, n, c) = (id(190), id(191), id(192), id(193));
        run(
            &w,
            vec![dir("/a", &a), dir("/a/f", &r), file("/a/f/c.txt", &c, "r1")],
        );

        // A moves away (nothing carries R along), and a different folder arrives at R's path.
        let batch = vec![dir("/b", &a), dir("/a/f", &n)];
        count_item_writes(w.db());
        run(&w, dup(&batch));
        // R (tombstone, stamp), A (move, stamp), N (insert), c (one hand-over, stamp). Handing c over
        // as a live row and demoting it afterwards would reach the same state with a third write to c.
        assert_eq!(item_writes(w.db()), 7);

        assert_eq!(state_of(&w, &r), STATE_DELETED);
        assert_eq!(state_of(&w, &n), STATE_ORPHAN, "/a is not there any more");
        let rc = row(w.db(), &c);
        assert_eq!((rc.state, rc.parent), (STATE_ORPHAN, NO_PARENT));
        assert_replay_changes_nothing(&w, &batch, "orphan folder over folder");
    }

    /// A folder that moves up onto its own parent's path replaces that parent, and is not handed to
    /// itself as one of the parent's children.
    ///
    /// Proof this test can fail: dropping the `c != item_id` filter of the adoption loop in `upsert`
    /// makes the folder its own parent.
    #[test]
    fn a_folder_moving_onto_its_parents_path_is_not_its_own_child() {
        let w = world(true, true);
        let (p, i, c) = (id(195), id(196), id(197));
        run(
            &w,
            vec![dir("/a", &p), dir("/a/i", &i), file("/a/i/c.txt", &c, "r1")],
        );

        let batch = vec![dir("/a", &i)];
        run(&w, dup(&batch));

        assert_eq!(state_of(&w, &p), STATE_DELETED);
        let (ri, rc) = (row(w.db(), &i), row(w.db(), &c));
        assert_eq!((ri.state, ri.parent), (STATE_LIVE, ROOT_ITEM));
        assert_eq!((rc.state, rc.parent), (STATE_LIVE, ri.item_id));
        assert_replay_changes_nothing(&w, &batch, "move up");
    }

    /// The demotion itself: nothing else moves the child, so it goes down with its folder.
    ///
    /// Proof this test can fail: dropping `demote_pass` leaves `x.txt` live with a parent that is an
    /// orphan.
    #[test]
    fn a_live_row_under_a_folder_that_became_an_orphan_is_demoted() {
        let w = world(true, true);
        let (p, f, x) = (id(180), id(181), id(182));
        run(
            &w,
            vec![dir("/p", &p), dir("/p/f", &f), file("/p/f/x.txt", &x, "r1")],
        );
        let batch = vec![dir("/q/f", &f)];
        run(&w, dup(&batch));

        assert_eq!(state_of(&w, &f), STATE_ORPHAN);
        let rx = row(w.db(), &x);
        assert_eq!((rx.state, rx.parent), (STATE_ORPHAN, NO_PARENT));
        assert_replay_changes_nothing(&w, &batch, "demotion");
        // The invariant, for every row: a live row's parent is a live or hidden folder.
        let rows = all_rows(w.db());
        for r in rows.iter().filter(|r| r.state == STATE_LIVE) {
            assert!(
                r.parent == ROOT_ITEM
                    || rows
                        .iter()
                        .any(|p| p.item_id == r.parent && p.state == STATE_LIVE),
                "{r:?}"
            );
        }
    }

    // ── the guards that remain ──────────────────────────────────────────────────────────────────

    /// Counts every write to `fp_item` (insert, update, delete) in a table the test can read, through
    /// triggers, so "a restated row writes nothing" is a number and not a belief.
    fn count_item_writes(db: &Db) {
        let conn = db.write.lock().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS wlog(k INTEGER);
             CREATE TRIGGER IF NOT EXISTS wlog_i AFTER INSERT ON fp_item BEGIN INSERT INTO wlog VALUES (1); END;
             CREATE TRIGGER IF NOT EXISTS wlog_u AFTER UPDATE ON fp_item BEGIN INSERT INTO wlog VALUES (2); END;
             CREATE TRIGGER IF NOT EXISTS wlog_d AFTER DELETE ON fp_item BEGIN INSERT INTO wlog VALUES (3); END;",
        )
        .unwrap();
    }

    fn item_writes(db: &Db) -> i64 {
        let conn = db.write.lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM wlog", [], |r| r.get(0))
            .unwrap()
    }

    /// A tree of folders and files, the same on every call.
    fn tree() -> Vec<DropboxEntry> {
        vec![
            dir("/A", &id(500)),
            dir("/A/B", &id(501)),
            file("/A/B/c.txt", &id(502), "r1"),
            file("/A/d.txt", &id(503), "r1"),
            dir("/Z", &id(504)),
        ]
    }

    /// A reseed over an unchanged tree writes NO item row, and a delta that restates every folder and
    /// file writes none either: the guards were worth 10 to 100 times in time, so a restated row must
    /// cost nothing.
    ///
    /// Proof this test can fail: dropping the `restates` test from `upsert_by_id` rewrites every row of
    /// the reseed; dropping it from `upsert` rewrites the delta's; dropping the `unchanged` test of
    /// `finish` or the write guard of `reresolve_all` shows up as updates.
    #[test]
    fn restated_rows_write_nothing_on_a_reseed_or_a_delta() {
        let w = world(true, false);
        seed(&w, tree()).unwrap();
        let before = all_rows(w.db());
        count_item_writes(w.db());

        seed(&w, tree()).unwrap();
        // The tree is the same: only the anchor rows move (the snapshot expires anchors, by design).
        assert_eq!(item_writes(w.db()), 0, "an unchanged reseed");
        let after = all_rows(w.db());
        assert_eq!(after, before);

        run(&w, tree());
        assert_eq!(item_writes(w.db()), 0, "a delta restating every row");
        assert_eq!(all_rows(w.db()), before);
    }

    /// `[deleted /a/x, folder /a A]`: restating A after a delete beneath it must not disturb what was
    /// tombstoned or what A holds. The stamp of A and of its other child stay, `x` is DELETED (not an
    /// orphan, not live), and nothing else is written.
    ///
    /// Proof this test can fail: dropping the `hit.id == item_id` guard of the occupant loop in `upsert`
    /// tombstones A and writes it back (two writes, and the extra tombstone churn).
    #[test]
    fn restating_a_folder_after_a_delete_inside_it_disturbs_nothing() {
        let w = world(true, true);
        let (a, x, y) = (id(510), id(511), id(512));
        run(
            &w,
            vec![
                dir("/a", &a),
                file("/a/x.txt", &x, "r1"),
                file("/a/y.txt", &y, "r1"),
            ],
        );
        let (a0, y0) = (row(w.db(), &a), row(w.db(), &y));
        count_item_writes(w.db());

        let batch = vec![gone("/a/x.txt"), dir("/a", &a)];
        run(&w, dup(&batch));

        assert_eq!(state_of(&w, &x), STATE_DELETED);
        assert_eq!(row(w.db(), &a), a0, "A is untouched, not even stamped");
        assert_eq!(row(w.db(), &y), y0, "its other child too");
        assert_eq!(
            item_writes(w.db()),
            2,
            "x: the tombstone and its stamp, and nothing else"
        );
        assert_replay_changes_nothing(&w, &batch, "restated folder");
    }

    /// The orphan heal reads the partial index on `state = 3`, not the table: with every batch running
    /// it, a scan of all rows is a cost on each delta.
    ///
    /// Proof this test can fail: dropping `idx_fp_item_orphan` from the schema (or writing the query
    /// with a bound parameter, which SQLite cannot match to a partial index) turns the plan into a scan.
    #[test]
    fn the_orphan_heal_uses_its_partial_index() {
        let w = world(true, true);
        let conn = w.db().read.lock().unwrap();
        let plan: Vec<String> = conn
            .prepare("EXPLAIN QUERY PLAN SELECT item_id FROM fp_item WHERE state = 3 ORDER BY path_lower")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(
            plan.iter().any(|d| d.contains("idx_fp_item_orphan")),
            "{plan:?}"
        );
    }

    // ── the snapshot: total, by id, the same for any order ──────────────────────────────────────

    /// What an observer sees of the projection, keyed by Dropbox id: the Dropbox id of the parent (or
    /// `root`), the state and the path. Independent of the item ids the order of arrival minted.
    fn shape(db: &Db) -> std::collections::BTreeMap<String, (String, i64, String)> {
        let rows = all_rows(db);
        rows.iter()
            .map(|r| {
                let parent = if r.parent == ROOT_ITEM {
                    "root".to_string()
                } else if r.parent == NO_PARENT {
                    "none".to_string()
                } else {
                    rows.iter()
                        .find(|p| p.item_id == r.parent)
                        .map_or("dangling".to_string(), |p| p.dropbox_id.clone())
                };
                (r.dropbox_id.clone(), (parent, r.state, r.path.clone()))
            })
            .collect()
    }

    fn permutations<T: Clone>(items: &[T]) -> Vec<Vec<T>> {
        if items.len() <= 1 {
            return vec![items.to_vec()];
        }
        let mut out = Vec::new();
        for i in 0..items.len() {
            let mut rest = items.to_vec();
            let head = rest.remove(i);
            for mut tail in permutations(&rest) {
                tail.insert(0, head.clone());
                out.push(tail);
            }
        }
        out
    }

    /// A snapshot gives the same result for any order of the same listing, over an empty projection and
    /// over a stored one, children before parents included.
    ///
    /// Proof this test can fail: resolving a parent only while placing (no `reresolve_all`) leaves a
    /// child listed before its folder an orphan; a placement that reads the stored tree and not the
    /// listing breaks the second world.
    #[test]
    fn a_snapshot_gives_the_same_result_for_any_listing_order() {
        let listing = vec![
            dir("/A", &id(600)),
            dir("/A/B", &id(601)),
            file("/A/B/c.txt", &id(602), "r1"),
            file("/A/f.txt", &id(603), "r1"),
        ];
        let canonical = {
            let w = world(true, false);
            seed(&w, dup(&listing)).unwrap();
            shape(w.db())
        };
        assert_eq!(canonical[&id(602)].0, id(601));
        assert!(canonical.values().all(|(_, st, _)| *st == STATE_LIVE));
        let indexes: Vec<usize> = (0..listing.len()).collect();
        for order in permutations(&indexes) {
            let listed = || -> Vec<DropboxEntry> {
                order
                    .iter()
                    .map(|&i| dup(&listing[i..=i]).remove(0))
                    .collect()
            };
            let w = world(true, false);
            seed(&w, listed()).unwrap();
            assert_eq!(shape(w.db()), canonical, "fresh: {order:?}");
            let w = world(true, false);
            seed(&w, dup(&listing)).unwrap();
            seed(&w, listed()).unwrap();
            assert_eq!(shape(w.db()), canonical, "over a stored tree: {order:?}");
        }
    }

    /// A parent that was replaced between two snapshots: stored folder X at `/a` holding F; the new
    /// listing has F first and a NEW folder Y at `/a`, and no X. X is swept and F, found under X while
    /// it was placed, is parented to Y.
    ///
    /// Proof this test can fail: dropping `reresolve_all` leaves F's parent pointing at the swept X.
    #[test]
    fn a_snapshot_reparents_a_child_whose_parent_was_replaced() {
        let w = world(true, false);
        let (x, y, f) = (id(610), id(611), id(612));
        seed(&w, vec![dir("/a", &x), file("/a/f.txt", &f, "r1")]).unwrap();
        let f0 = row(w.db(), &f).item_id;

        seed(&w, vec![file("/a/f.txt", &f, "r1"), dir("/a", &y)]).unwrap();

        let (rf, ry) = (row(w.db(), &f), row(w.db(), &y));
        assert_eq!(rf.item_id, f0, "F survives");
        assert_eq!((rf.state, rf.parent), (STATE_LIVE, ry.item_id));
        assert_eq!(count(w.db(), "fp_item"), 2, "X is gone");
    }

    /// Items moved between two snapshots are all kept, under their new places, by their ids.
    ///
    /// Proof this test can fail: matching rows by path instead of by id inserts duplicates; skipping the
    /// sweep keeps stale rows; skipping `reresolve_all` leaves F under its old parent.
    #[test]
    fn a_snapshot_keeps_items_that_moved_since_the_last_one() {
        let w = world(true, false);
        let (x, f, g, z) = (id(620), id(621), id(622), id(623));
        seed(
            &w,
            vec![
                dir("/a", &x),
                file("/a/f.txt", &f, "r1"),
                file("/a/g.txt", &g, "r1"),
            ],
        )
        .unwrap();
        let ids = (
            row(w.db(), &x).item_id,
            row(w.db(), &f).item_id,
            row(w.db(), &g).item_id,
        );

        // F's folder is not listed: F is an orphan, kept.
        let moved = vec![
            file("/b/f.txt", &g, "r1"),
            dir("/b", &x),
            file("/c/f.txt", &f, "r1"),
        ];
        seed(&w, dup(&moved)).unwrap();
        assert_eq!(
            (
                row(w.db(), &x).item_id,
                row(w.db(), &f).item_id,
                row(w.db(), &g).item_id
            ),
            ids
        );
        assert_eq!(state_of(&w, &f), STATE_ORPHAN, "no folder at /c yet");
        let (rx, rg) = (row(w.db(), &x), row(w.db(), &g));
        assert_eq!((rg.state, rg.parent), (STATE_LIVE, rx.item_id));
        assert_eq!(rg.path, "/b/f.txt");

        // The folder arrives in the next listing: F heals.
        let mut next = moved;
        next.push(dir("/c", &z));
        seed(&w, next).unwrap();
        let (rf, rz) = (row(w.db(), &f), row(w.db(), &z));
        assert_eq!((rf.state, rf.parent), (STATE_LIVE, rz.item_id));
        assert_eq!(count(w.db(), "fp_item"), 4);
    }

    /// An id that is a tombstone and is listed by the snapshot comes back live, and the snapshot leaves
    /// no tombstone behind.
    ///
    /// Proof this test can fail: making `upsert_by_id` skip a stored row that is deleted leaves it
    /// deleted, and the invariant check turns the snapshot into an error; forcing the placed state to
    /// DELETED does the same.
    #[test]
    fn a_snapshot_lists_a_tombstoned_id_as_live() {
        let w = world(true, true);
        run(&w, vec![dir("/X", X_DIR), file("/X/c.txt", X_CHILD, "r1")]);
        run(&w, vec![gone("/X")]);
        assert_eq!(state_of(&w, X_CHILD), STATE_DELETED);

        seed(&w, vec![dir("/X", X_DIR), file("/X/c.txt", X_CHILD, "r1")]).unwrap();

        assert!(
            all_rows(w.db()).iter().all(|r| r.state == STATE_LIVE),
            "{:?}",
            states(w.db())
        );
        assert_eq!(count(w.db(), "fp_item"), 2);
    }

    // ── the switch ──────────────────────────────────────────────────────────────────────────────

    /// A switch-on with a stored cursor and an unpopulated projection seeds (the cursor is treated as
    /// absent), and a seed that FAILS keeps the stored cursor and backs off. Startup wrote nothing.
    ///
    /// Proof this test can fail: a cursor wipe in `apply_switch` leaves `usable_delta_cursor` and the
    /// failed seed with nothing to fall back on (the third assertion).
    #[test]
    fn a_switch_on_with_a_cursor_and_no_projection_seeds_and_a_failed_seed_keeps_the_cursor() {
        let w = world(true, false);
        w.db()
            .set_app_config(REMOTE_DELTA_CURSOR_KEY, "cursor-stored")
            .unwrap();
        assert_eq!(
            crate::file_provider::apply_switch(w.db(), true),
            Ok(crate::file_provider::SwitchEffect::SeedPending)
        );

        let seeded = std::cell::Cell::new(false);
        let got = cursor_or_seed(&w.state, |_| {
            seeded.set(true);
            Ok("cursor-seeded".to_string())
        })
        .unwrap();
        assert!(
            seeded.get(),
            "a valid cursor beside no projection still seeds"
        );
        assert_eq!(got, None);

        let w = world(true, false);
        w.db()
            .set_app_config(REMOTE_DELTA_CURSOR_KEY, "cursor-stored")
            .unwrap();
        crate::file_provider::apply_switch(w.db(), true).unwrap();
        let got = cursor_or_seed(&w.state, |_| Err(AppError::Network("down".into()))).unwrap();
        assert_eq!(got.as_deref(), Some("cursor-stored"));
        assert_eq!(
            w.db()
                .get_app_config(REMOTE_DELTA_CURSOR_KEY)
                .unwrap()
                .as_deref(),
            Some("cursor-stored")
        );
    }

    /// With the switch off, `cursor_or_seed` behaves as it always did even if a cursor shows up while
    /// the seed runs: a failing seed is an error, with no reseed backoff set.
    ///
    /// Proof this test can fail: dropping the `projection_active` guard of `fall_back_to_stored_cursor`
    /// swallows the error and returns the cursor.
    #[test]
    fn with_the_switch_off_a_failing_seed_is_an_error_even_if_a_cursor_appeared() {
        let w = world(false, false);
        let got = cursor_or_seed(&w.state, |s| {
            s.db.set_app_config(REMOTE_DELTA_CURSOR_KEY, "appeared")
                .unwrap();
            Err(AppError::Network("down".into()))
        });
        assert!(matches!(got, Err(AppError::Network(_))), "{got:?}");
        assert!(w.db().health().reseed_not_before.is_none());
    }

    /// Switching off over a projection that holds nothing opens no write transaction at all; over one
    /// that holds rows it clears.
    ///
    /// Proof this test can fail: calling `project_clear` unconditionally in `apply_switch` runs the
    /// writer (the hook fires) on an empty projection.
    #[test]
    fn switching_off_over_an_empty_projection_opens_no_write_transaction() {
        let fired = Arc::new(AtomicBool::new(false));
        let w = world(false, false);
        let flag = fired.clone();
        BEFORE_TX_HOOK.with(|h| {
            *h.borrow_mut() = Some(Box::new(move |_: &Db| {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
            }));
        });
        assert_eq!(
            crate::file_provider::apply_switch(w.db(), false),
            Ok(crate::file_provider::SwitchEffect::Untouched)
        );
        assert!(!fired.load(std::sync::atomic::Ordering::SeqCst));

        // And it still clears when there is something to clear.
        let w = world(true, true);
        run(&w, inv1());
        assert_eq!(
            crate::file_provider::apply_switch(w.db(), false),
            Ok(crate::file_provider::SwitchEffect::Cleared)
        );
        assert!(fired.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(count(w.db(), "fp_item"), 0);
    }

    // ── review round: how long a snapshot holds the write lock ──────────────────────────────────

    /// MEASUREMENT, not a guard: a snapshot of about 50 000 items through the real seed path, timed.
    /// Run with `--nocapture` to see the numbers. The assertion is only that the projection is whole.
    #[test]
    fn a_fifty_thousand_item_snapshot_is_measured() {
        let w = world(true, false);
        let mut listing = Vec::with_capacity(50_000);
        for d in 0..500 {
            listing.push(dir(
                &format!("/D{d}"),
                &format!("id:t00000000000MEASD{d:05}"),
            ));
            for f in 0..99 {
                listing.push(file(
                    &format!("/D{d}/f{f}.txt"),
                    &format!("id:t00000000000MEASF{d:05}{f:03}"),
                    "r1",
                ));
            }
        }
        let started = Instant::now();
        seed(&w, listing).unwrap();
        let first = started.elapsed();
        assert_eq!(count(w.db(), "fp_item"), 50_000);

        // A reseed over an unchanged tree (the common case after a cursor reset).
        let mut again = Vec::with_capacity(50_000);
        for d in 0..500 {
            again.push(dir(
                &format!("/D{d}"),
                &format!("id:t00000000000MEASD{d:05}"),
            ));
            for f in 0..99 {
                again.push(file(
                    &format!("/D{d}/f{f}.txt"),
                    &format!("id:t00000000000MEASF{d:05}{f:03}"),
                    "r1",
                ));
            }
        }
        let started = Instant::now();
        seed(&w, again).unwrap();
        let reseed = started.elapsed();
        eprintln!("MEASURE snapshot of 50000 items: first {first:?}, unchanged reseed {reseed:?}");
    }

    // ── pins (#237) ─────────────────────────────────────────────────────────────────────────────

    const PIN_A: &str = "id:t00000000000PINFILE01";
    const PIN_B: &str = "id:t00000000000PINFILE02";

    fn pinned_world() -> (World, i64) {
        let w = world(true, true);
        run(&w, vec![file("/a.txt", PIN_A, "r1")]);
        let id = w.db().projection_item_id("/a.txt").unwrap();
        (w, id)
    }

    fn row_seq(db: &Db, item_id: i64) -> i64 {
        all_rows(db)
            .into_iter()
            .find(|r| r.item_id == item_id)
            .unwrap()
            .seq
    }

    /// A pin that changes takes one sequence value, on the item's own row, so the system reads that item
    /// again; the same pin twice takes none.
    ///
    /// Proof this test can fail: not stamping the row leaves the system with the old content policy;
    /// stamping without a change moves the sequence for nothing.
    #[test]
    fn a_pin_moves_its_item_in_the_change_sequence_once() {
        let (w, id) = pinned_world();
        let db = w.db();
        let s0 = seq(db);

        assert!(db.set_projection_pinned(id, true).unwrap());
        assert!(db.projection_pinned(id).unwrap());
        assert_eq!(seq(db), s0 + 1);
        assert_eq!(row_seq(db, id), s0 + 1, "the item's row carries the change");
        assert!(db.set_projection_pinned(id, true).unwrap());
        assert_eq!(seq(db), s0 + 1, "the same pin again is not a change");

        assert!(db.set_projection_pinned(id, false).unwrap());
        assert!(!db.projection_pinned(id).unwrap());
        assert_eq!(row_seq(db, id), s0 + 2);
    }

    /// Only a live row can be pinned.
    ///
    /// Proof this test can fail: dropping the state check pins a deleted item.
    #[test]
    fn a_deleted_item_cannot_be_pinned() {
        let (w, id) = pinned_world();
        run(&w, vec![gone("/a.txt")]);

        assert!(!w.db().set_projection_pinned(id, true).unwrap());
        assert!(!w.db().projection_pinned(id).unwrap());
        assert!(!w.db().set_projection_pinned(9_999, true).unwrap());
    }

    /// A pin goes with its row: a snapshot that no longer lists the item drops it, so the item coming back
    /// under the same id is not pinned by a choice made for the old one.
    ///
    /// Proof this test can fail: dropping the sweep's cleanup leaves the pin on the returning item.
    #[test]
    fn a_pin_goes_with_its_row() {
        let (w, id) = pinned_world();
        let db = w.db();
        db.set_projection_pinned(id, true).unwrap();
        seed(&w, vec![file("/b.txt", PIN_B, "r1")]).unwrap();
        run(&w, vec![file("/a.txt", PIN_A, "r2")]);
        assert_eq!(
            db.projection_item_id("/a.txt"),
            Some(id),
            "the same identity came back"
        );
        assert!(
            !db.projection_pinned(id).unwrap(),
            "after a snapshot without it"
        );
    }

    /// Found in review: a recovery clear (a failed snapshot, the delta failing) dropped every pin, and the
    /// reseed brought the same files back unpinned. A clear keeps them; leaving the account drops them.
    ///
    /// Proof this test can fail: dropping pins in the clear loses the first; keeping them on reset keeps
    /// the second.
    #[test]
    fn a_clear_keeps_pins_and_leaving_the_account_drops_them() {
        let (w, id) = pinned_world();
        let db = w.db();
        db.set_projection_pinned(id, true).unwrap();

        db.project_clear().unwrap();
        seed(&w, vec![file("/a.txt", PIN_A, "r3")]).unwrap();
        assert_eq!(db.projection_item_id("/a.txt"), Some(id));
        assert!(
            db.projection_pinned(id).unwrap(),
            "after a clear and the reseed"
        );

        db.reset_sync_state().unwrap();
        assert!(
            !db.projection_pinned(id).unwrap(),
            "after leaving the account"
        );
    }

    /// Found in review: what is under a pinned folder kept its old version, so the system never learned
    /// it now inherits the pin; and unpinning a folder left pins under it that stopped its eviction.
    ///
    /// Proof this test can fail: stamping only the item, or deleting only its own pin.
    #[test]
    fn a_pin_reaches_what_is_under_the_folder() {
        let w = world(true, true);
        run(
            &w,
            vec![
                dir("/Top", "id:t00000000000PINDIR021"),
                file("/Top/a.txt", "id:t00000000000PINFILE21", "r1"),
                file("/Top/b.txt", "id:t00000000000PINFILE22", "r1"),
            ],
        );
        let db = w.db();
        let (top, a, b) = (
            db.projection_item_id("/top").unwrap(),
            db.projection_item_id("/top/a.txt").unwrap(),
            db.projection_item_id("/top/b.txt").unwrap(),
        );
        db.set_projection_pinned(b, true).unwrap();
        let before = seq(db);

        db.set_projection_pinned(top, true).unwrap();
        assert!(
            row_seq(db, a) > before && row_seq(db, b) > before,
            "the children are read again"
        );

        db.set_projection_pinned(top, false).unwrap();
        assert!(!db.projection_pinned(top).unwrap());
        assert!(!db.projection_pinned(b).unwrap(), "a pin under it goes too");
    }

    /// Proof this test can fail: dropping the cleanup after pruning leaves the pin on the returning item.
    #[test]
    fn a_pin_goes_with_its_pruned_tombstone() {
        let (w, id) = pinned_world();
        let db = w.db();
        db.set_projection_pinned(id, true).unwrap();
        TOMBSTONE_CAP_OVERRIDE.with(|c| c.set(Some(0)));
        run(&w, vec![gone("/a.txt")]);
        TOMBSTONE_CAP_OVERRIDE.with(|c| c.set(None));
        assert_eq!(
            db.projection_row_state(id).unwrap(),
            None,
            "the tombstone was pruned"
        );

        run(&w, vec![file("/a.txt", PIN_A, "r2")]);

        assert_eq!(db.projection_item_id("/a.txt"), Some(id));
        assert!(!db.projection_pinned(id).unwrap());
    }

    /// Proof this test can fail: dropping the recursion misses the nested file; dropping a state check
    /// lists a deleted one; building the path from `path_lower` loses the case Finder shows.
    #[test]
    fn the_files_under_an_item_are_its_live_files_at_any_depth_with_their_case() {
        let w = world(true, true);
        run(
            &w,
            vec![
                dir("/Top", "id:t00000000000PINDIR001"),
                dir("/Top/Sub", "id:t00000000000PINDIR002"),
                file("/Top/Sub/Deep.txt", "id:t00000000000PINFILE11", "r1"),
                file("/Top/a.txt", "id:t00000000000PINFILE12", "r1"),
                file("/Top/gone.txt", "id:t00000000000PINFILE13", "r1"),
            ],
        );
        run(&w, vec![gone("/Top/gone.txt")]);
        let db = w.db();
        let top = db.projection_item_id("/top").unwrap();
        let deep = db.projection_item_id("/top/sub/deep.txt").unwrap();

        assert_eq!(
            db.projection_live_files_under(top).unwrap(),
            vec!["Top/Sub/Deep.txt".to_string(), "Top/a.txt".to_string()]
        );
        assert_eq!(
            db.projection_live_files_under(deep).unwrap(),
            vec!["Top/Sub/Deep.txt".to_string()]
        );
        let gone_id = db.projection_item_id("/top/gone.txt").unwrap();
        assert!(db.projection_live_files_under(gone_id).unwrap().is_empty());
    }

    /// Found in the second review: a folder moved into a pinned folder kept its files' old versions, so the
    /// system never learned they now inherit the pin (and the reverse when moved out). With no pin at all,
    /// nothing extra is stamped.
    ///
    /// Proof this test can fail: stamping only the moved folder leaves its file where it was.
    #[test]
    fn what_is_under_a_folder_moved_into_a_pinned_folder_is_read_again() {
        for pin in [true, false] {
            let w = world(true, true);
            run(
                &w,
                vec![
                    dir("/Top", "id:t00000000000PINDIR031"),
                    dir("/Other", "id:t00000000000PINDIR032"),
                    file("/Other/x.txt", "id:t00000000000PINFILE31", "r1"),
                ],
            );
            let db = w.db();
            let top = db.projection_item_id("/top").unwrap();
            let x = db.projection_item_id("/other/x.txt").unwrap();
            if pin {
                db.set_projection_pinned(top, true).unwrap();
            }
            let before = row_seq(db, x);

            run(
                &w,
                vec![
                    gone("/Other/x.txt"),
                    gone("/Other"),
                    dir("/Top/Other", "id:t00000000000PINDIR032"),
                    file("/Top/Other/x.txt", "id:t00000000000PINFILE31", "r1"),
                ],
            );

            assert_eq!(row_seq(db, x) > before, pin, "pinned: {pin}");

            // And out again, by the folder alone, as a move made in Finder reaches the projection. (A
            // folder-only delta re-resolves the rows under it by itself, so this checks the pinned case.)
            if pin {
                let before = row_seq(db, x);
                run(
                    &w,
                    vec![
                        gone("/Top/Other"),
                        dir("/Other", "id:t00000000000PINDIR032"),
                    ],
                );
                assert!(row_seq(db, x) > before, "moved out");
            }
        }
    }

    /// Found in the third review: with any pin at all, every folder move stamped its whole subtree. A move
    /// that stays outside every pinned folder stamps only the folder.
    ///
    /// Proof this test can fail: dropping the comparison of the old and new side stamps the child.
    #[test]
    fn a_move_away_from_any_pinned_folder_stamps_only_the_folder() {
        let w = world(true, true);
        run(
            &w,
            vec![
                dir("/Pinned", "id:t00000000000PINDIR041"),
                dir("/A", "id:t00000000000PINDIR042"),
                dir("/B", "id:t00000000000PINDIR043"),
                file("/A/y.txt", "id:t00000000000PINFILE41", "r1"),
            ],
        );
        let db = w.db();
        db.set_projection_pinned(db.projection_item_id("/pinned").unwrap(), true)
            .unwrap();
        let y = db.projection_item_id("/a/y.txt").unwrap();
        let before = row_seq(db, y);

        run(
            &w,
            vec![
                gone("/A/y.txt"),
                gone("/A"),
                dir("/B/A", "id:t00000000000PINDIR042"),
                file("/B/A/y.txt", "id:t00000000000PINFILE41", "r1"),
            ],
        );

        assert_eq!(row_seq(db, y), before);
    }

    /// Found when covering battery: a new revision of a pinned file, or a file added under a pinned folder,
    /// waited for power. The delta's commit names them, with the pin that covers each.
    ///
    /// Proof this test can fail: a range off by one misses the first value; not walking up misses the
    /// file under the folder; listing unpinned rows reads what nobody pinned.
    #[test]
    fn a_delta_names_the_pinned_files_it_changed() {
        let w = world(true, true);
        run(
            &w,
            vec![
                dir("/Top", "id:t00000000000PINDIR051"),
                file("/Top/a.txt", "id:t00000000000PINFILE51", "r1"),
                file("/free.txt", "id:t00000000000PINFILE52", "r1"),
            ],
        );
        let db = w.db();
        let top = db.projection_item_id("/top").unwrap();
        db.set_projection_pinned(top, true).unwrap();
        let before = seq(db);

        run(
            &w,
            vec![
                file_with("/Top/a.txt", "id:t00000000000PINFILE51", "r2", HASH_B_OTHER),
                file("/Top/new.txt", "id:t00000000000PINFILE53", "r1"),
                file_with("/free.txt", "id:t00000000000PINFILE52", "r2", HASH_B_OTHER),
            ],
        );
        let after = seq(db);
        let rows = usize::try_from(after - before).unwrap();

        assert_eq!(
            db.projection_pinned_files_stamped(after, rows).unwrap(),
            vec![
                (top, "Top/a.txt".to_string()),
                (top, "Top/new.txt".to_string())
            ]
        );
        assert!(db
            .projection_pinned_files_stamped(after, 0)
            .unwrap()
            .is_empty());
    }
}
