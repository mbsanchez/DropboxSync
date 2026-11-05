-- The File Provider projection schema (DBSYNC-95, slice 2a), as the extension's tests build it.
--
-- The host owns the real DDL (`src-tauri/src/storage/db/projection.rs`, `SCHEMA_SQL`) and is the only
-- writer; the extension opens `app.db` read-only. This file is a copy for tests that need a database
-- without the host, and a Rust test (`the_schema_fixture_matches_the_real_schema`) fails when the two
-- differ, so edit them together.
--
-- Not in here: the single `fp_state` row. The host seeds it with a random `generation`; a test inserts
-- its own, for example:
--   INSERT INTO fp_state(id, schema_version, generation, seq, min_valid_seq, populated)
--   VALUES (1, 1, 7, 0, 0, 1);
--
-- Meaning of the columns the extension reads:
--   fp_item.parent_item   0 is the domain root; -1 only on an orphan.
--   fp_item.state         0 live, 1 hidden by selective sync, 2 deleted (tombstone), 3 orphan.
--                         The extension lists state 0 only and reports 1 and 2 as deleted.
--   fp_item.content_version, meta_version
--                         Opaque bytes, at most 128 each, used as-is for NSFileProviderItemVersion.
--   fp_item.changed_seq   Unique per observable change; pages the working-set changes.
--   fp_state              Anchor = "v1.<generation>.<seq>". Expired iff the generation differs,
--                         seq < min_valid_seq or seq > the current seq. Nothing else expires it.

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
CREATE TABLE IF NOT EXISTS fp_state (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    schema_version INTEGER NOT NULL,
    generation INTEGER NOT NULL,
    seq INTEGER NOT NULL,
    min_valid_seq INTEGER NOT NULL,
    populated INTEGER NOT NULL CHECK (populated IN (0, 1))
);
