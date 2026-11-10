import Foundation
import notify

/// Asks the host for a file (DBSYNC-95 slice 3, ADR-0004). `src-tauri/src/file_provider_exchange.rs` is the
/// other side and describes the protocol.
///
/// Foundation and libSystem only, so `Tests/run-projection-tests.sh` can run it for real, with a real lock.

/// Suffix of the Darwin notification name, after the App Group id.
let exchangeNotificationSuffix = ".fp-request"

/// The files of one exchange.
struct ExchangePaths {
    let root: URL

    init(dataDirectory: URL) {
        root = dataDirectory.appendingPathComponent("fp-exchange")
    }

    var lock: String { root.appendingPathComponent("host.lock").path }

    func request(_ id: String) -> URL { root.appendingPathComponent("requests/\(id).json") }
    func requestTemp(_ id: String) -> URL { root.appendingPathComponent("requests/.\(id).tmp") }
    func cancelled(_ id: String) -> URL { root.appendingPathComponent("requests/\(id).cancelled") }
    func staged(_ id: String) -> URL { root.appendingPathComponent("staging/\(id).data") }
    func outgoing(_ id: String) -> URL { root.appendingPathComponent("staging/\(id).up") }
    func result(_ id: String) -> URL { root.appendingPathComponent("results/\(id).json") }
}

/// How a fetch ended.
enum ExchangeOutcome: Equatable {
    case staged(URL, contentVersion: Data)
    /// The host holds no lock: it is not running.
    case hostDown
    case offline
    case auth
    case refused
    case retry
    case cancelled
    /// A write landed, and this row is the item to return.
    case written(itemId: Int64)
    /// The name is taken.
    case collision
    /// The edit went up as a conflicted copy, and this row, Dropbox's version, is the item.
    case conflict(itemId: Int64)
    case deleted
    case deletionRejected
    /// The item was deleted on Dropbox: `noSuchItem` for it.
    case gone
    /// The folder a create or a move names was deleted on Dropbox: `noSuchItem` for that folder.
    case goneParent
    /// A pin or an unpin took.
    case done

    /// A class only: no name or path reaches the log.
    var logLabel: String {
        switch self {
        case .staged: return "staged"
        case .hostDown: return "hostDown"
        case .offline: return "offline"
        case .auth: return "auth"
        case .refused: return "refused"
        case .retry: return "retry"
        case .cancelled: return "cancelled"
        case .written: return "written"
        case .collision: return "collision"
        case .conflict: return "conflict"
        case .deleted: return "deleted"
        case .deletionRejected: return "deletionRejected"
        case .gone: return "gone"
        case .goneParent: return "goneParent"
        case .done: return "done"
        }
    }
}

/// The two Finder actions of #237, as Info.plist names them.
enum OfflineAction {
    static let makeOffline = "com.mobsanchez.dropboxsyncdesktop.fileprovider.action.make_offline"
    static let makeOnlineOnly = "com.mobsanchez.dropboxsyncdesktop.fileprovider.action.make_online_only"

    /// `true` to pin, `false` to unpin, `nil` for an action that is not one of these.
    static func pins(_ identifier: String) -> Bool? {
        switch identifier {
        case makeOffline: return true
        case makeOnlineOnly: return false
        default: return nil
        }
    }
}

/// Whether a process holds the host's exclusive lock. The kernel drops it when that process dies, so this
/// cannot be fooled by a crash. It only reads: a shared lock that the close releases at once, which the
/// host's exclusive one waits out. A missing file is a host that never ran here.
func hostIsRunning(lockPath: String) -> Bool {
    let descriptor = open(lockPath, O_RDONLY)
    guard descriptor >= 0 else { return false }
    defer { close(descriptor) }
    return flock(descriptor, LOCK_SH | LOCK_NB) != 0 && errno == EWOULDBLOCK
}

/// A result file as the host writes it. Anything unreadable or of another version is `retry`: the system
/// asks again, and a request that keeps failing shows as a failed download, not as a missing file.
func decodeExchangeResult(_ data: Data, staged: URL) -> ExchangeOutcome {
    guard let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
          object["version"] as? Int == 1,
          let outcome = object["outcome"] as? String else {
        return .retry
    }
    switch outcome {
    case "ok":
        guard let version = object["content_version"] as? String else { return .retry }
        return .staged(staged, contentVersion: Data(version.utf8))
    case "offline":
        return .offline
    case "auth":
        return .auth
    case "refused":
        return .refused
    case "written", "conflict":
        guard let itemId = (object["item_id"] as? NSNumber)?.int64Value else { return .retry }
        return outcome == "written" ? .written(itemId: itemId) : .conflict(itemId: itemId)
    case "collision":
        return .collision
    case "deleted":
        return .deleted
    case "deletion_rejected":
        return .deletionRejected
    case "gone":
        return .gone
    case "gone_parent":
        return .goneParent
    case "done":
        return .done
    default:
        return .retry
    }
}

/// The row to hand a staged file over with. `nil`: the row moved on during the download, so the file is
/// not the version the system would be told it is.
func rowToHandOver(stagedVersion: Data, presence: ItemPresence) -> ProjectionRow? {
    guard case .live(let row) = presence, row.contentVersion == stagedVersion else { return nil }
    return row
}

final class ExchangeClient {
    private let paths: ExchangePaths
    private let notification: String
    private let pollInterval: DispatchTimeInterval
    private let deadline: TimeInterval

    /// `deadline` exists for a host that holds the lock and never answers. It is longer than the host's own
    /// bound on a download, and shorter than the host's sweep.
    init(
        paths: ExchangePaths,
        notification: String,
        pollInterval: DispatchTimeInterval = .milliseconds(100),
        deadline: TimeInterval = 600
    ) {
        self.paths = paths
        self.notification = notification
        self.pollInterval = pollInterval
        self.deadline = deadline
    }

    /// Queues a request and calls `done` once, on a background queue. A host that is down answers now,
    /// before anything is queued, and one that dies during the wait answers within a poll.
    /// `viewer`: Finder asked, so a person is opening the file. The host opens a file Dropbox will not hand
    /// out on dropbox.com for that person, and for nobody else.
    func fetch(
        itemId: Int64,
        viewer: Bool = false,
        isCancelled: @escaping () -> Bool,
        done: @escaping (ExchangeOutcome) -> Void
    ) {
        send(["op": "fetch", "item_id": itemId, "viewer": viewer], outgoing: nil, isCancelled, done)
    }

    /// Asks the host to create `name` in `parentItem` (0 is the domain root). A file's bytes are cloned
    /// from `contents` into staging first: the host cannot read where the system keeps them.
    func create(
        parentItem: Int64,
        name: String,
        folder: Bool,
        contents: URL?,
        isCancelled: @escaping () -> Bool,
        done: @escaping (ExchangeOutcome) -> Void
    ) {
        send(
            ["op": "create", "parent_item": parentItem, "name": name, "folder": folder],
            outgoing: folder ? nil : contents, isCancelled, done)
    }

    /// A rename or move (`newParent` and `newName` together), new bytes, or both. `baseVersion` is the
    /// content version the edit was made against.
    func modify(
        itemId: Int64,
        baseVersion: Data,
        newPlace: (parent: Int64, name: String)?,
        contents: URL?,
        isCancelled: @escaping () -> Bool,
        done: @escaping (ExchangeOutcome) -> Void
    ) {
        var fields: [String: Any] = [
            "op": "modify", "item_id": itemId, "base_version": String(decoding: baseVersion, as: UTF8.self),
            "content": contents != nil,
        ]
        if let place = newPlace {
            fields["new_parent"] = place.parent
            fields["new_name"] = place.name
        }
        send(fields, outgoing: contents, isCancelled, done)
    }

    func delete(
        itemId: Int64,
        baseVersion: Data,
        isCancelled: @escaping () -> Bool,
        done: @escaping (ExchangeOutcome) -> Void
    ) {
        send(
            ["op": "delete", "item_id": itemId, "base_version": String(decoding: baseVersion, as: UTF8.self)],
            outgoing: nil, isCancelled, done)
    }

    /// *Make Available Offline* (`pinned`) or *Make Online-Only* on one item (#237). A pin names the
    /// domain's folder as the user sees it (`root`), where the host reads the pinned files to download them.
    func pin(
        itemId: Int64,
        pinned: Bool,
        root: String,
        isCancelled: @escaping () -> Bool,
        done: @escaping (ExchangeOutcome) -> Void
    ) {
        var fields: [String: Any] = ["op": pinned ? "pin" : "unpin", "item_id": itemId]
        if pinned { fields["root"] = root }
        send(fields, outgoing: nil, isCancelled, done)
    }

    private func send(
        _ fields: [String: Any],
        outgoing: URL?,
        _ isCancelled: @escaping () -> Bool,
        _ done: @escaping (ExchangeOutcome) -> Void
    ) {
        let id = UUID().uuidString
        DispatchQueue.global(qos: .userInitiated).async {
            guard hostIsRunning(lockPath: self.paths.lock) else {
                done(.hostDown)
                return
            }
            if let outgoing = outgoing {
                do {
                    try FileManager.default.copyItem(at: outgoing, to: self.paths.outgoing(id))
                } catch {
                    done(.retry)
                    return
                }
            }
            guard self.enqueue(id: id, fields: fields) else {
                try? FileManager.default.removeItem(at: self.paths.outgoing(id))
                done(.retry)
                return
            }
            notify_post(self.notification)
            self.wait(id: id, giveUpAt: Date().addingTimeInterval(self.deadline), isCancelled, done)
        }
    }

    /// The request's JSON, keys sorted so its bytes are stable.
    static func requestBody(_ fields: [String: Any]) -> Data? {
        var all = fields
        all["version"] = 1
        return try? JSONSerialization.data(withJSONObject: all, options: [.sortedKeys, .withoutEscapingSlashes])
    }

    private func enqueue(id: String, fields: [String: Any]) -> Bool {
        guard let body = Self.requestBody(fields) else { return false }
        do {
            try body.write(to: paths.requestTemp(id))
            try FileManager.default.moveItem(at: paths.requestTemp(id), to: paths.request(id))
            return true
        } catch {
            try? FileManager.default.removeItem(at: paths.requestTemp(id))
            return false
        }
    }

    private func wait(
        id: String,
        giveUpAt: Date,
        _ isCancelled: @escaping () -> Bool,
        _ done: @escaping (ExchangeOutcome) -> Void
    ) {
        if let data = try? Data(contentsOf: paths.result(id)) {
            try? FileManager.default.removeItem(at: paths.result(id))
            let outcome = decodeExchangeResult(data, staged: paths.staged(id))
            if case .staged = outcome, !isCancelled() {
                done(outcome)
                return
            }
            try? FileManager.default.removeItem(at: paths.staged(id))
            done(isCancelled() ? .cancelled : outcome)
            return
        }
        // The rest stop waiting. A request the host has not taken goes with them, and a host that has
        // taken one finds the marker when it finishes, and drops the bytes.
        let early: ExchangeOutcome?
        if isCancelled() {
            early = .cancelled
        } else if !hostIsRunning(lockPath: paths.lock) {
            early = .hostDown
        } else {
            early = Date() >= giveUpAt ? .retry : nil
        }
        if let early = early {
            let takenBack = (try? FileManager.default.removeItem(at: paths.request(id))) != nil
            if takenBack {
                try? FileManager.default.removeItem(at: paths.outgoing(id))
            } else if early != .hostDown {
                FileManager.default.createFile(atPath: paths.cancelled(id).path, contents: nil)
            }
            done(early)
            return
        }
        DispatchQueue.global(qos: .userInitiated).asyncAfter(deadline: .now() + pollInterval) {
            self.wait(id: id, giveUpAt: giveUpAt, isCancelled, done)
        }
    }
}
