import Foundation

/// The decisions the File Provider extension makes about the host's projection (DBSYNC-95 slice 2b).
///
/// Foundation only, so `Tests/run-projection-tests.sh` can compile it without FileProvider.framework. The
/// host writes the tables (`src-tauri/src/storage/db/projection.rs`); `Tests/projection-schema.sql` says
/// what the columns mean.

/// The schema this reader understands. A newer one is refused rather than guessed at.
let supportedProjectionSchema: Int64 = 1

/// How many rows one page or one change batch carries.
let projectionBatchSize = 200

/// `fp_item.state`.
enum ProjectionItemState {
    static let live: Int64 = 0
}

/// `fp_item.parent_item` of a child of the domain root.
let projectionRootItem: Int64 = 0

/// One `fp_item` row.
struct ProjectionRow: Equatable {
    var itemId: Int64
    var parentItem: Int64
    var name: String
    var isFolder: Bool
    var sizeBytes: Int64
    var modifiedTs: Int64
    var contentVersion: Data
    var metaVersion: Data
    var state: Int64
    var changedSeq: Int64
    /// Pinned by *Make Available Offline* (#237): `fp_pin` holds the item.
    var pinned: Bool = false
    /// A folder above it is pinned: it is kept offline through that folder.
    var inherited: Bool = false
}

/// Whether the projection knows an item. A row that is not live may come back with the same identifier
/// (ADR-0004, *A rename split in two*), so it is not the same answer as an identifier with no row:
/// only the second is `noSuchItem`, which tells the system to delete the item from disk.
enum ItemPresence: Equatable {
    case live(ProjectionRow)
    case notLive
    case unknown
}

/// What a create marked `mayAlreadyExist` becomes. The system sends it when it re-imports what is on disk
/// (a domain added again, a reimport), so the item is usually one the projection already holds.
/// Creating it again would collide, and the system would keep both under a new name.
enum Reimport: Equatable {
    /// The item is this row: answer it, and the system keeps its identifier.
    case existing(ProjectionRow)
    /// A file with no content on disk that matches nothing: answer no item, and the system removes the
    /// empty placeholder. The header's rule for a reimport that cannot be matched.
    case refuse
    /// Real content that is not known here: an ordinary create.
    case create

    static func decide(existing: ProjectionRow?, folder: Bool, hasContents: Bool) -> Reimport {
        if let row = existing, row.isFolder == folder, folder || !hasContents {
            return .existing(row)
        }
        return !folder && !hasContents ? .refuse : .create
    }
}

/// The `fp_state` row.
struct ProjectionHead: Equatable {
    var schemaVersion: Int64
    var generation: Int64
    var seq: Int64
    var minValidSeq: Int64
    var populated: Bool
}

/// Why a read produced nothing, logged by class. Names and paths never go in here.
enum ReadFailure: Error, Equatable {
    case noContainer
    case open(Int32)
    case query(Int32)
    case noState
    case schemaTooNew(Int64)
    case notPopulated

    /// What the system is told, in the terms of `NSFileProviderReplicatedExtension.h`.
    ///
    /// SQLite being busy or failing an I/O can pass by itself, so the system retries it. Everything else
    /// needs the host, and retrying would only repeat the answer: a missing or damaged file, or a
    /// projection not ready. The host lifts that throttling when it starts and when it finishes
    /// populating a projection. `noContainer` and `schemaTooNew` end with a new build, not with a retry.
    var answer: ReadAnswer {
        switch self {
        case .open(let code), .query(let code):
            return Self.passesByItself.contains(code & 0xff) ? .retry(code: code) : .waitForSignal
        case .noContainer, .noState, .schemaTooNew, .notPopulated:
            return .waitForSignal
        }
    }

    /// `SQLITE_BUSY`, `SQLITE_LOCKED`, `SQLITE_IOERR`, as primary codes: extended ones keep them in the
    /// low byte.
    private static let passesByItself: Set<Int32> = [5, 6, 10]
}

/// The two ways the system can treat a failed read.
enum ReadAnswer: Equatable {
    /// `NSFileProviderErrorServerUnreachable`: an error is shown and the system backs off until signalled.
    case waitForSignal
    /// Any other error is transient and retried. `code` is the SQLite result, for the underlying error.
    case retry(code: Int32)

    /// The header's form for an error no existing code represents. The underlying error is a plain
    /// `NSError`, because the whole value crosses XPC.
    static func transientError(code: Int32) -> NSError {
        NSError(
            domain: NSCocoaErrorDomain,
            code: NSXPCConnectionReplyInvalid,
            userInfo: [
                NSUnderlyingErrorKey: NSError(
                    domain: "com.mobsanchez.dropboxsyncdesktop.projection", code: Int(code))
            ])
    }
}

/// `i<n>`: the identifier the system sees. It comes from an opaque id, never from a path.
enum ItemIdCodec {
    static func encode(_ itemId: Int64) -> String {
        "i\(itemId)"
    }

    /// The id, or `nil` for anything `encode` cannot have produced.
    static func decode(_ raw: String) -> Int64? {
        guard raw.first == "i" else { return nil }
        let digits = raw.dropFirst()
        guard let value = Int64(digits), value > 0, String(value) == digits else { return nil }
        return value
    }
}

/// What the system caches about an item beyond its row: today, its capabilities. The system keeps an
/// item's capabilities until the item reaches it again with a different version, so a build that
/// changes them must change this number. It is the anchor's prefix, so every anchor of an older build
/// expires into a rescan, and it is part of every metadata version, so that rescan replaces each item
/// instead of keeping the cached one. Slice 3 served read-only items under format 1, and Finder went on
/// refusing to delete them after slice 4 allowed it. Format 3 adds trashing.
///
/// A pin changes what the system caches too (the content policy and `userInfo`), so a pinned item carries
/// a `p.` in its version, and one under a pinned folder an `i.`. Other items keep the format-3 version they
/// had: their policy is the default and the actions read a missing key as not pinned, so no rescan is
/// needed.
enum ItemFormat {
    static let current = 3

    static func metadataVersion(_ rowVersion: Data, pinned: Bool = false, inherited: Bool = false) -> Data {
        Data("f\(current).\(pinned ? "p." : "")\(inherited ? "i." : "")".utf8) + rowVersion
    }
}

/// A position in the change sequence, as `v<ItemFormat.current>.<generation>.<seq>`.
struct ProjectionAnchor: Equatable {
    var generation: Int64
    var seq: Int64

    var encoded: Data {
        Data("v\(ItemFormat.current).\(generation).\(seq)".utf8)
    }

    /// `nil` for anything that is not an anchor of this format, such as the tracer's `1` or an anchor of an
    /// older item format.
    init?(encoded: Data) {
        guard let text = String(data: encoded, encoding: .utf8) else { return nil }
        let parts = text.split(separator: ".", omittingEmptySubsequences: false)
        guard parts.count == 3, parts[0] == "v\(ItemFormat.current)",
              let generation = Int64(parts[1]), let seq = Int64(parts[2]) else { return nil }
        self.generation = generation
        self.seq = seq
    }

    init(generation: Int64, seq: Int64) {
        self.generation = generation
        self.seq = seq
    }

    init(head: ProjectionHead) {
        self.init(generation: head.generation, seq: head.seq)
    }

    /// The same rule as `anchor_expired` in the host: nothing else expires an anchor, because an expiry
    /// makes the system rescan the whole domain.
    func isExpired(against head: ProjectionHead) -> Bool {
        generation != head.generation || seq < head.minValidSeq || seq > head.seq
    }
}

/// The page of an item listing: the last `item_id` served. The system's own initial pages, and anything
/// else that is not ours, start from the beginning.
enum PageCodec {
    private static let prefix = "p"

    static func encode(afterItem itemId: Int64) -> Data {
        Data("\(prefix)\(itemId)".utf8)
    }

    static func afterItem(_ page: Data) -> Int64 {
        guard let text = String(data: page, encoding: .utf8), text.hasPrefix(prefix),
              let value = Int64(text.dropFirst(prefix.count)), value > 0 else { return 0 }
        return value
    }

    /// What follows a page of `rows`: the next page, or `nil` when `rows` was the last one. A store read
    /// asks for one row more than a batch, so a full batch followed by nothing is not one more empty page.
    static func next(rows: [ProjectionRow], batch: Int) -> (page: [ProjectionRow], next: Data?) {
        guard rows.count > batch else { return (rows, nil) }
        let page = Array(rows.prefix(batch))
        return (page, page.last.map { encode(afterItem: $0.itemId) })
    }
}

/// What one `enumerateChanges` reports.
struct ChangePlan: Equatable {
    var updated: [ProjectionRow]
    var deletedItemIds: [Int64]
    var upTo: ProjectionAnchor
    var moreComing: Bool

    /// `rows` are in `changed_seq` order, asked for with one more than `batch`. Every state but live is
    /// a deletion: hidden by selective sync, a tombstone, or an orphan that was listed before.
    ///
    /// A partial batch stops at its last row's sequence, so the next call resumes exactly there. The
    /// last batch goes up to the head: the head can be past the last row, because an expiry or a clear
    /// takes a sequence value that no row carries.
    static func make(rows: [ProjectionRow], batch: Int, head: ProjectionHead) -> ChangePlan {
        let moreComing = rows.count > batch
        let page = Array(rows.prefix(batch))
        let live = page.filter { $0.state == ProjectionItemState.live }
        let gone = page.filter { $0.state != ProjectionItemState.live }.map(\.itemId)
        let upTo = moreComing
            ? ProjectionAnchor(generation: head.generation, seq: page.last?.changedSeq ?? head.seq)
            : ProjectionAnchor(head: head)
        return ChangePlan(updated: live, deletedItemIds: gone, upTo: upTo, moreComing: moreComing)
    }
}
