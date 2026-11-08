import Foundation
import SQLite3

/// Reads the host's projection out of `app.db` (DBSYNC-95 slice 2b, ADR-0004). It never writes it.
///
/// Every call opens the file read-only, runs one read transaction and closes, so the state row and the
/// rows it returns are one consistent picture and no read outlives the call. A reader that stayed open
/// would pin the WAL and starve the host's checkpoints. A read-only open of a WAL database still maps
/// `-shm`, which is why this works only inside the group container.
///
/// That open also needs `-wal` to exist: with neither file, Apple's SQLite fails every prepare with
/// `SQLITE_CANTOPEN`, even in a writable directory, while the host is not running. The host keeps both
/// files across a close (`keep_wal_files` in `src-tauri/src/storage/db.rs`), so this handle can stay
/// read-only.
struct ProjectionStore {
    let databasePath: String

    /// How long a read waits for the host. The host's own writers wait 10 s.
    private static let busyTimeoutMillis: Int32 = 5000

    /// The host's data dir inside the App Group container.
    static func dataDirectory(inContainer container: URL) -> URL {
        container.appendingPathComponent("Library/Application Support/DropboxSyncDesktop")
    }

    /// The database inside the App Group container.
    static func path(inContainer container: URL) -> String {
        dataDirectory(inContainer: container).appendingPathComponent("app.db").path
    }

    private static let rowColumns =
        "item_id, parent_item, name, is_folder, size_bytes, modified_ts, content_version, meta_version, "
        + "state, changed_seq"

    /// The head, refused like every other read: an anchor from a projection that cannot be served would
    /// expire at the next snapshot.
    func head() throws -> ProjectionHead {
        try read { db in try Self.readyHead(db) }
    }

    /// Whether there is a row with this id, and whether it is live.
    func presence(_ itemId: Int64) throws -> ItemPresence {
        try read { db in
            _ = try Self.readyHead(db)
            let row = try Self.rows(
                db, "SELECT \(Self.rowColumns) FROM fp_item WHERE item_id = ?1", [itemId]
            ).first
            guard let row = row else { return .unknown }
            return row.state == ProjectionItemState.live ? .live(row) : .notLive
        }
    }

    /// What enumerating `parent` lists: its live children after `afterItem`, at most `limit`, in
    /// `item_id` order. A live file lists itself, because the system also opens an enumerator on a single
    /// document to follow it. `nil`: there is no live row, and `presence` says what to answer. The domain
    /// root always exists.
    func children(of parent: Int64, afterItem: Int64, limit: Int) throws -> [ProjectionRow]? {
        try read { db in
            _ = try Self.readyHead(db)
            if parent != projectionRootItem {
                let container = try Self.rows(
                    db,
                    "SELECT \(Self.rowColumns) FROM fp_item WHERE item_id = ?1 AND state = 0",
                    [parent]
                ).first
                guard let container = container else { return nil }
                if !container.isFolder { return [container] }
            }
            return try Self.rows(
                db,
                "SELECT \(Self.rowColumns) FROM fp_item "
                    + "WHERE parent_item = ?1 AND state = 0 AND item_id > ?2 ORDER BY item_id LIMIT ?3",
                [parent, afterItem, Int64(limit)]
            )
        }
    }

    /// Every live row after `afterItem`, at most `limit`, in `item_id` order: the working set.
    func liveItems(afterItem: Int64, limit: Int) throws -> [ProjectionRow] {
        try read { db in
            _ = try Self.readyHead(db)
            return try Self.rows(
                db,
                "SELECT \(Self.rowColumns) FROM fp_item "
                    + "WHERE state = 0 AND item_id > ?1 ORDER BY item_id LIMIT ?2",
                [afterItem, Int64(limit)]
            )
        }
    }

    /// The head and the rows changed after `anchor`, read together, at most `limit`, in sequence order.
    /// `nil`: the anchor is expired or is not one of ours. A projection that cannot be served is refused
    /// first, so the system is not sent to rescan something that would only refuse it.
    func changes(
        from anchor: ProjectionAnchor?, limit: Int
    ) throws -> (head: ProjectionHead, rows: [ProjectionRow])? {
        try read { db in
            let head = try Self.readyHead(db)
            guard let anchor = anchor, !anchor.isExpired(against: head) else { return nil }
            let rows = try Self.rows(
                db,
                "SELECT \(Self.rowColumns) FROM fp_item "
                    + "WHERE changed_seq > ?1 ORDER BY changed_seq LIMIT ?2",
                [anchor.seq, Int64(limit)]
            )
            return (head, rows)
        }
    }

    // MARK: - one read

    /// The one way this type opens the file. The caller closes the handle, whatever the result.
    func open() -> (result: Int32, handle: OpaquePointer?) {
        var handle: OpaquePointer?
        let result = sqlite3_open_v2(databasePath, &handle, SQLITE_OPEN_READONLY, nil)
        return (result, handle)
    }

    private func read<T>(_ body: (OpaquePointer) throws -> T) throws -> T {
        let (opened, handle) = open()
        defer { sqlite3_close_v2(handle) }
        guard opened == SQLITE_OK, let db = handle else { throw ReadFailure.open(opened) }
        sqlite3_busy_timeout(db, Self.busyTimeoutMillis)
        let began = sqlite3_exec(db, "BEGIN DEFERRED", nil, nil, nil)
        guard began == SQLITE_OK else { throw ReadFailure.query(began) }
        defer { sqlite3_exec(db, "COMMIT", nil, nil, nil) }
        return try body(db)
    }

    private static func loadHead(_ db: OpaquePointer) throws -> ProjectionHead {
        let statement = try prepare(
            db,
            "SELECT schema_version, generation, seq, min_valid_seq, populated FROM fp_state WHERE id = 1"
        )
        defer { sqlite3_finalize(statement) }
        let step = sqlite3_step(statement)
        switch step {
        case SQLITE_ROW:
            return ProjectionHead(
                schemaVersion: sqlite3_column_int64(statement, 0),
                generation: sqlite3_column_int64(statement, 1),
                seq: sqlite3_column_int64(statement, 2),
                minValidSeq: sqlite3_column_int64(statement, 3),
                populated: sqlite3_column_int64(statement, 4) != 0
            )
        case SQLITE_DONE:
            throw ReadFailure.noState
        default:
            throw ReadFailure.query(step)
        }
    }

    /// The head, refusing what must not be served: a newer schema, or a projection with nothing in it.
    private static func readyHead(_ db: OpaquePointer) throws -> ProjectionHead {
        let head = try loadHead(db)
        guard head.schemaVersion <= supportedProjectionSchema else {
            throw ReadFailure.schemaTooNew(head.schemaVersion)
        }
        guard head.populated else { throw ReadFailure.notPopulated }
        return head
    }

    private static func prepare(_ db: OpaquePointer, _ sql: String) throws -> OpaquePointer {
        var statement: OpaquePointer?
        let prepared = sqlite3_prepare_v2(db, sql, -1, &statement, nil)
        guard prepared == SQLITE_OK, let ready = statement else {
            sqlite3_finalize(statement)
            throw ReadFailure.query(prepared)
        }
        return ready
    }

    private static func rows(_ db: OpaquePointer, _ sql: String, _ values: [Int64]) throws -> [ProjectionRow] {
        let statement = try prepare(db, sql)
        defer { sqlite3_finalize(statement) }
        for (index, value) in values.enumerated() {
            sqlite3_bind_int64(statement, Int32(index + 1), value)
        }
        var out: [ProjectionRow] = []
        while true {
            let step = sqlite3_step(statement)
            if step == SQLITE_DONE { return out }
            guard step == SQLITE_ROW else { throw ReadFailure.query(step) }
            out.append(row(statement))
        }
    }

    private static func row(_ statement: OpaquePointer) -> ProjectionRow {
        ProjectionRow(
            itemId: sqlite3_column_int64(statement, 0),
            parentItem: sqlite3_column_int64(statement, 1),
            name: text(statement, 2),
            isFolder: sqlite3_column_int64(statement, 3) != 0,
            sizeBytes: sqlite3_column_int64(statement, 4),
            modifiedTs: sqlite3_column_int64(statement, 5),
            contentVersion: blob(statement, 6),
            metaVersion: blob(statement, 7),
            state: sqlite3_column_int64(statement, 8),
            changedSeq: sqlite3_column_int64(statement, 9)
        )
    }

    private static func text(_ statement: OpaquePointer, _ column: Int32) -> String {
        guard let bytes = sqlite3_column_text(statement, column) else { return "" }
        return String(cString: bytes)
    }

    private static func blob(_ statement: OpaquePointer, _ column: Int32) -> Data {
        let count = Int(sqlite3_column_bytes(statement, column))
        guard count > 0, let bytes = sqlite3_column_blob(statement, column) else { return Data() }
        return Data(bytes: bytes, count: count)
    }
}
