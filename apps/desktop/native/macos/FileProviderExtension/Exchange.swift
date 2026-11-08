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
    func fetch(itemId: Int64, isCancelled: @escaping () -> Bool, done: @escaping (ExchangeOutcome) -> Void) {
        let id = UUID().uuidString
        DispatchQueue.global(qos: .userInitiated).async {
            guard hostIsRunning(lockPath: self.paths.lock) else {
                done(.hostDown)
                return
            }
            guard self.enqueue(id: id, itemId: itemId) else {
                done(.retry)
                return
            }
            notify_post(self.notification)
            self.wait(id: id, giveUpAt: Date().addingTimeInterval(self.deadline), isCancelled, done)
        }
    }

    private func enqueue(id: String, itemId: Int64) -> Bool {
        let body = Data("{\"version\":1,\"item_id\":\(itemId)}".utf8)
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
            if !takenBack && early != .hostDown {
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
