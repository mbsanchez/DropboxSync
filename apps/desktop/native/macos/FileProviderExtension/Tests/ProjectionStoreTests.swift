// The store against a real WAL database built from projection-schema.sql, the DDL a Rust test keeps equal to
// the host's. Run through run-projection-tests.sh.

import Foundation
import SQLite3

extension ProjectionTests {
    private static func exec(_ db: OpaquePointer?, _ sql: String) -> Int32 {
        sqlite3_exec(db, sql, nil, nil, nil)
    }

    private static func insertSql(id: Int64, parent: Int64, name: String, folder: Int, state: Int, seq: Int64) -> String {
        "INSERT INTO fp_identity(item_id, dropbox_id, created_at) VALUES (\(id), 'id:\(id)', 'now');"
            + "INSERT INTO fp_item(item_id, parent_item, path_lower, name, is_folder, size_bytes, modified_ts, "
            + "content_hash, rev, content_version, meta_version, downloaded, state, changed_seq) VALUES "
            + "(\(id), \(parent), '/p\(id)', '\(name)', \(folder), 5, 1700000000, 'h', 'r', x'0102', x'0304', 0, "
            + "\(state), \(seq));"
    }

    private static func makeDatabase(at path: String, schema: String) -> OpaquePointer? {
        var db: OpaquePointer?
        check("create", sqlite3_open(path, &db) == SQLITE_OK)
        sqlite3_busy_timeout(db, 10000)
        check("wal", exec(db, "PRAGMA journal_mode=WAL") == SQLITE_OK)
        check("schema", exec(db, schema) == SQLITE_OK)
        check("state", exec(db, """
            INSERT INTO fp_state(id, schema_version, generation, seq, min_valid_seq, populated)
            VALUES (1, 1, 7, 30, 5, 1);
            """) == SQLITE_OK)
        let rows = insertSql(id: 1, parent: 0, name: "carpeta ñ", folder: 1, state: 0, seq: 10)
            + insertSql(id: 2, parent: 1, name: "a.txt", folder: 0, state: 0, seq: 11)
            + insertSql(id: 3, parent: 1, name: "gone.txt", folder: 0, state: 2, seq: 12)
            + insertSql(id: 4, parent: 1, name: "hidden.txt", folder: 0, state: 1, seq: 13)
            + insertSql(id: 5, parent: -1, name: "orphan.txt", folder: 0, state: 3, seq: 14)
            + insertSql(id: 6, parent: 0, name: "file.txt", folder: 0, state: 0, seq: 15)
            + insertSql(id: 7, parent: 1, name: "gone folder", folder: 1, state: 2, seq: 16)
        check("rows", exec(db, rows) == SQLITE_OK)
        check("pin", exec(db, "INSERT INTO fp_pin(item_id) VALUES (1), (2)") == SQLITE_OK)
        return db
    }

    static func storeChecks() {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("fp-store-\(UUID().uuidString)")
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let schemaPath = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : ""
        guard let schema = try? String(contentsOfFile: schemaPath, encoding: .utf8) else {
            check("the schema fixture is passed in", false)
            return
        }
        let path = dir.appendingPathComponent("app.db").path
        let writer = makeDatabase(at: path, schema: schema)
        let store = ProjectionStore(databasePath: path)

        reads(store)
        writesAreRefused(store)
        refusals(writer, store)
        concurrency(writer, store)
        withoutPinTable(writer, store)
        sqlite3_close(writer)
        inheritance(dir: dir, schema: schema)
        afterTheHostClosed(store, path: path)

        let missing = ProjectionStore(databasePath: dir.appendingPathComponent("none.db").path)
        do {
            _ = try missing.head()
            check("a missing file fails", false)
        } catch ReadFailure.open {
        } catch {
            check("a missing file is an open failure", false)
        }
    }

    /// The least a read needs with the host gone: a `-wal`, even an empty one, and no `-shm`, which the
    /// read builds itself. The host leaves more (`keep_wal_files` keeps both, the `-wal` with content);
    /// a Rust test reads that through this SQLite. Without the `-wal` this read fails.
    private static func afterTheHostClosed(_ store: ProjectionStore, path: String) {
        let manager = FileManager.default
        try? manager.removeItem(atPath: path + "-shm")
        try? manager.removeItem(atPath: path + "-wal")
        check("an empty wal", manager.createFile(atPath: path + "-wal", contents: Data()))
        do {
            check("a read with the host gone", try store.head().generation == 7)
        } catch {
            check("a read with the host gone threw \(error)", false)
        }
    }

    private static func reads(_ store: ProjectionStore) {
        do {
            check("head", try store.head().seq == 30)
            let root = try store.children(of: projectionRootItem, afterItem: 0, limit: 10)
            check("root lists live children", root?.map(\.itemId) == [1, 6])
            check("names survive", root?.first?.name == "carpeta ñ")
            check("versions survive", root?.first?.contentVersion == Data([1, 2]) && root?.first?.metaVersion == Data([3, 4]))
            let folder = try store.children(of: 1, afterItem: 0, limit: 10)
            check("a folder lists state 0 only", folder?.map(\.itemId) == [2])
            check("paging", try store.children(of: projectionRootItem, afterItem: 1, limit: 10)?.map(\.itemId) == [6])
            check("the limit binds children", try store.children(of: projectionRootItem, afterItem: 0, limit: 1)?.map(\.itemId) == [1])
            check("a live file lists itself", try store.children(of: 2, afterItem: 0, limit: 10)?.map(\.itemId) == [2])
            check("a live row is live", { if case .live(let r) = try? store.presence(2) { return r.itemId == 2 } else { return false } }())
            check("a tombstone may come back", try store.presence(3) == .notLive)
            check("a hidden row may come back", try store.presence(4) == .notLive)
            check("no row is unknown", try store.presence(999) == .unknown)
            check("a deleted file is gone", try store.children(of: 3, afterItem: 0, limit: 10) == nil)
            check("a hidden file is gone", try store.children(of: 4, afterItem: 0, limit: 10) == nil)
            check("a deleted folder is gone", try store.children(of: 7, afterItem: 0, limit: 10) == nil)
            check("an unknown id is gone", try store.children(of: 99, afterItem: 0, limit: 10) == nil)
            check("working set is live rows", try store.liveItems(afterItem: 0, limit: 10).map(\.itemId) == [1, 2, 6])
            let live = try store.liveItems(afterItem: 0, limit: 10)
            check("fp_pin is read as the row's pin", live.map(\.pinned) == [true, true, false])
            check("under a pinned folder is inherited", live.map(\.inherited) == [false, true, false])
            check("working set pages", try store.liveItems(afterItem: 1, limit: 10).map(\.itemId) == [2, 6])
            check("the limit binds the working set", try store.liveItems(afterItem: 0, limit: 1).map(\.itemId) == [1])
            let changes = try store.changes(from: ProjectionAnchor(generation: 7, seq: 11), limit: 10)
            check("changes in sequence order, every state", changes?.rows.map(\.itemId) == [3, 4, 5, 6, 7])
            check("the head comes with them", changes?.head.seq == 30)
            let one = try store.changes(from: ProjectionAnchor(generation: 7, seq: 11), limit: 1)
            check("the limit binds changes", one?.rows.map(\.itemId) == [3])
            check("another generation expires", try store.changes(from: ProjectionAnchor(generation: 8, seq: 11), limit: 10) == nil)
            check("below min_valid_seq expires", try store.changes(from: ProjectionAnchor(generation: 7, seq: 4), limit: 10) == nil)
            check("not an anchor expires", try store.changes(from: nil, limit: 10) == nil)
        } catch {
            check("reads threw \(error)", false)
        }
    }

    /// A pin reaches every depth under its folder, through the memo, and stops at the root and at an
    /// orphan (found in review: the fixture only had depth 1).
    private static func inheritance(dir: URL, schema: String) {
        var db: OpaquePointer?
        let path = dir.appendingPathComponent("inherit.db").path
        check("create inherit", sqlite3_open(path, &db) == SQLITE_OK)
        defer { sqlite3_close(db) }
        check("schema inherit", exec(db, schema) == SQLITE_OK)
        let rows = "INSERT INTO fp_state(id, schema_version, generation, seq, min_valid_seq, populated) "
            + "VALUES (1, 1, 7, 30, 5, 1);"
            + insertSql(id: 1, parent: 0, name: "top", folder: 1, state: 0, seq: 10)
            + insertSql(id: 2, parent: 1, name: "mid", folder: 1, state: 0, seq: 11)
            + insertSql(id: 3, parent: 2, name: "deep.txt", folder: 0, state: 0, seq: 12)
            + insertSql(id: 4, parent: 2, name: "deep2.txt", folder: 0, state: 0, seq: 13)
            + insertSql(id: 5, parent: 0, name: "other.txt", folder: 0, state: 0, seq: 14)
            + insertSql(id: 6, parent: -1, name: "orphan.txt", folder: 0, state: 3, seq: 15)
            + "INSERT INTO fp_pin(item_id) VALUES (1);"
        check("rows inherit", exec(db, rows) == SQLITE_OK)
        let store = ProjectionStore(databasePath: path)
        let live = (try? store.liveItems(afterItem: 0, limit: 10)) ?? []
        check("inherited at every depth", live.map(\.inherited) == [false, true, true, true, false])
        let changes = try? store.changes(from: ProjectionAnchor(generation: 7, seq: 14), limit: 10)
        check("an orphan inherits nothing", changes?.rows.map(\.inherited) == [false])
    }

    /// A new extension can run before the host that creates `fp_pin` (found in review): it still reads.
    private static func withoutPinTable(_ writer: OpaquePointer?, _ store: ProjectionStore) {
        check("drop fp_pin", exec(writer, "DROP TABLE fp_pin") == SQLITE_OK)
        // The concurrency check added rows from 1001 on.
        let live = try? store.liveItems(afterItem: 0, limit: 3)
        check("reads work without fp_pin", live?.map(\.itemId) == [1, 2, 6])
        check("and nothing is pinned", live?.allSatisfy { !$0.pinned && !$0.inherited } == true)
    }

    /// Writes are refused by the handle itself, not by the store being careful.
    private static func writesAreRefused(_ store: ProjectionStore) {
        let (opened, db) = store.open()
        check("the open works", opened == SQLITE_OK)
        defer { sqlite3_close(db) }
        check("a write fails", exec(db, "UPDATE fp_state SET seq = 1") == SQLITE_READONLY)
    }

    private static func refusals(_ writer: OpaquePointer?, _ store: ProjectionStore) {
        let reads: [(String, () throws -> Void)] = [
            ("head", { _ = try store.head() }),
            ("presence", { _ = try store.presence(1) }),
            ("children", { _ = try store.children(of: projectionRootItem, afterItem: 0, limit: 10) }),
            ("working set", { _ = try store.liveItems(afterItem: 0, limit: 10) }),
            ("changes", { _ = try store.changes(from: ProjectionAnchor(generation: 7, seq: 11), limit: 10) }),
            // Refused rather than expired: an expiry here would send the system to rescan for nothing.
            ("an expired anchor", { _ = try store.changes(from: ProjectionAnchor(generation: 8, seq: 11), limit: 10) }),
        ]
        func refused(_ expected: ReadFailure) {
            for (label, read) in reads {
                do {
                    try read()
                    check("\(label) is not served: \(expected)", false)
                } catch {
                    check("\(label) says \(expected)", (error as? ReadFailure) == expected)
                }
            }
        }
        check("unpopulate", exec(writer, "UPDATE fp_state SET populated = 0") == SQLITE_OK)
        refused(.notPopulated)
        check("repopulate", exec(writer, "UPDATE fp_state SET populated = 1, schema_version = 2") == SQLITE_OK)
        refused(.schemaTooNew(2))
        check("restore", exec(writer, "UPDATE fp_state SET schema_version = 1") == SQLITE_OK)
        check("no state row", exec(writer, "DELETE FROM fp_state") == SQLITE_OK)
        refused(.noState)
        check("state row again", exec(writer, """
            INSERT INTO fp_state(id, schema_version, generation, seq, min_valid_seq, populated)
            VALUES (1, 1, 7, 30, 5, 1);
            """) == SQLITE_OK)
    }

    /// Readers against a writer holding the file busy: every read succeeds, and each sees the state row
    /// and the rows of one transaction.
    private static func concurrency(_ writer: OpaquePointer?, _ store: ProjectionStore) {
        let done = DispatchSemaphore(value: 0)
        let lock = NSLock()
        var readFailures = 0
        var torn = 0
        let stop = UnsafeMutablePointer<Int32>.allocate(capacity: 1)
        stop.initialize(to: 0)
        defer { stop.deallocate() }
        let readers = 4
        for _ in 0..<readers {
            DispatchQueue.global().async {
                while OSAtomicAdd32(0, stop) == 0 {
                    do {
                        let from = ProjectionAnchor(generation: 7, seq: 16)
                        guard let read = try store.changes(from: from, limit: 100_000) else {
                            lock.lock(); readFailures += 1; lock.unlock()
                            continue
                        }
                        if read.rows.count != Int(read.head.seq) - 1_000 && read.head.seq >= 1_000 {
                            lock.lock(); torn += 1; lock.unlock()
                        }
                    } catch {
                        lock.lock(); readFailures += 1; lock.unlock()
                    }
                }
                done.signal()
            }
        }
        var writeFailures = 0
        check("sequence base", exec(writer, "UPDATE fp_state SET seq = 1000") == SQLITE_OK)
        for n in Int64(1)...1500 {
            let id = 1000 + n
            let sql = "BEGIN IMMEDIATE;"
                + insertSql(id: id, parent: 1, name: "s\(id).txt", folder: 0, state: 0, seq: id)
                + "UPDATE fp_state SET seq = \(id); COMMIT;"
            if exec(writer, sql) != SQLITE_OK { writeFailures += 1 }
        }
        OSAtomicIncrement32(stop)
        for _ in 0..<readers { done.wait() }
        check("every write committed (\(writeFailures) failed)", writeFailures == 0)
        check("no read failed (\(readFailures))", readFailures == 0)
        check("no read mixed two transactions (\(torn))", torn == 0)
    }
}
