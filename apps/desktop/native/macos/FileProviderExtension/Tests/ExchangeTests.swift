import Foundation

/// A value two queues touch.
private final class LockIsolated {
    private let lock = NSLock()
    private var stored: String?

    var value: String? {
        lock.lock()
        defer { lock.unlock() }
        return stored
    }

    func set(_ new: String) {
        lock.lock()
        stored = new
        lock.unlock()
    }
}

extension ProjectionTests {
    /// The extension's side of the exchange, against a real directory and a real `flock`.
    static func exchangeChecks() {
    func expect(_ ok: Bool, _ label: String) { check(label, ok) }

    let base = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    let paths = ExchangePaths(dataDirectory: base)
    for dir in ["requests", "staging", "results"] {
        try! FileManager.default.createDirectory(
            at: paths.root.appendingPathComponent(dir), withIntermediateDirectories: true)
    }
    defer { try? FileManager.default.removeItem(at: base) }

    func holdLock() -> Int32 {
        FileManager.default.createFile(atPath: paths.lock, contents: nil)
        let descriptor = open(paths.lock, O_RDWR)
        expect(flock(descriptor, LOCK_EX) == 0, "the test takes the lock")
        return descriptor
    }

    func client(deadline: TimeInterval = 30) -> ExchangeClient {
        ExchangeClient(
            paths: paths, notification: "test.exchange", pollInterval: .milliseconds(5), deadline: deadline)
    }

    func requests() -> [String] {
        (try? FileManager.default.contentsOfDirectory(atPath: paths.root.appendingPathComponent("requests").path))
            ?? []
    }

    func finish(_ start: (@escaping (ExchangeOutcome) -> Void) -> Void) -> ExchangeOutcome? {
        let finished = DispatchSemaphore(value: 0)
        let box = NSLock()
        var got: ExchangeOutcome?
        start { outcome in
            box.lock()
            got = outcome
            box.unlock()
            finished.signal()
        }
        return finished.wait(timeout: .now() + 10) == .timedOut ? nil : got
    }

    /// Answers the first request that appears, as the host does: staged bytes first, then the result.
    func answerNextRequest(
        with body: String,
        staged: Bool = false,
        publishes: Bool = true,
        inspect: @escaping (String) -> Void = { _ in },
        beforeResult: @escaping (String) -> Void = { _ in }
    ) {
        DispatchQueue.global().async {
            for _ in 0..<2000 {
                if let name = requests().first(where: { $0.hasSuffix(".json") }) {
                    let id = String(name.dropLast(".json".count))
                    let sent = (try? String(contentsOf: paths.request(id), encoding: .utf8)) ?? ""
                    inspect(sent)
                    try? FileManager.default.removeItem(at: paths.request(id))
                    if staged { try? Data("hi".utf8).write(to: paths.staged(id)) }
                    beforeResult(id)
                    // As the host does once it has sent them.
                    try? FileManager.default.removeItem(at: paths.outgoing(id))
                    guard publishes else { return }
                    let temp = paths.root.appendingPathComponent("results/.\(id).tmp")
                    try? Data(body.utf8).write(to: temp)
                    try? FileManager.default.moveItem(at: temp, to: paths.result(id))
                    return
                }
                usleep(1000)
            }
        }
    }

    // A host that never ran, then one that has gone: answered at once, and nothing is queued.
    expect(!hostIsRunning(lockPath: paths.lock), "no lock file: not running")
    let neverRan = finish { client().fetch(itemId: 1, isCancelled: { false }, done: $0) }
    expect(neverRan == .hostDown, "no lock file answers hostDown")
    FileManager.default.createFile(atPath: paths.lock, contents: nil)
    expect(!hostIsRunning(lockPath: paths.lock), "an unheld lock file: not running")
    let unheld = finish { client().fetch(itemId: 1, isCancelled: { false }, done: $0) }
    expect(unheld == .hostDown, "an unheld lock answers hostDown")
    expect(requests().isEmpty, "a host that is down gets no request")

    // A check takes nothing it does not give back: it can repeat under the host's lock.
    let lock = holdLock()
    expect(hostIsRunning(lockPath: paths.lock), "a held lock: running")
    expect(hostIsRunning(lockPath: paths.lock), "the check released what it took")

    // Each answer.
    let answers: [(String, ExchangeOutcome)] = [
        (#"{"version":1,"outcome":"offline"}"#, .offline),
        (#"{"version":1,"outcome":"auth"}"#, .auth),
        (#"{"version":1,"outcome":"refused"}"#, .refused),
        (#"{"version":1,"outcome":"retry"}"#, .retry),
        (#"{"version":9,"outcome":"ok","content_version":"x"}"#, .retry),
        (#"{"version":1,"outcome":"ok"}"#, .retry),
        (#"not json"#, .retry),
    ]
    for (body, expected) in answers {
        answerNextRequest(with: body)
        let outcome = finish { client().fetch(itemId: 2, isCancelled: { false }, done: $0) }
        expect(outcome == expected, "\(body) answers \(expected.logLabel)")
    }

    // A file handed over.
    answerNextRequest(with: #"{"version":1,"outcome":"ok","content_version":"c1:abc"}"#, staged: true)
    let fetched = finish { client().fetch(itemId: 3, isCancelled: { false }, done: $0) }
    if case .staged(let url, let version)? = fetched {
        expect(version == Data("c1:abc".utf8), "the version is the host's")
        expect((try? Data(contentsOf: url)) == Data("hi".utf8), "the URL is the staged file")
        try? FileManager.default.removeItem(at: url)
    } else {
        expect(false, "ok answers staged")
    }
    let results = try? FileManager.default.contentsOfDirectory(
        atPath: paths.root.appendingPathComponent("results").path)
    expect(results == [], "the result file is consumed")

    // The deadline, with a host that holds the lock and never answers.
    let silent = finish { client(deadline: 0.05).fetch(itemId: 4, isCancelled: { false }, done: $0) }
    expect(silent == .retry, "a silent host answers retry at the deadline")
    expect(requests().isEmpty, "the deadline takes the request back")

    // Cancelled while waiting.
    let cancel = NSLock()
    var cancelled = false
    DispatchQueue.global().asyncAfter(deadline: .now() + 0.05) {
        cancel.lock()
        cancelled = true
        cancel.unlock()
    }
    let stopped = finish {
        client().fetch(
            itemId: 5,
            isCancelled: {
                cancel.lock()
                defer { cancel.unlock() }
                return cancelled
            },
            done: $0)
    }
    expect(stopped == .cancelled, "cancelling stops the wait")
    expect(requests().isEmpty, "cancelling takes the request back")

    // Cancelled with the bytes already there: they are not left in staging.
    cancelled = false
    answerNextRequest(with: #"{"version":1,"outcome":"ok","content_version":"c1:abc"}"#, staged: true) { _ in
        cancel.lock()
        cancelled = true
        cancel.unlock()
    }
    let racing = finish {
        client().fetch(
            itemId: 6,
            isCancelled: {
                cancel.lock()
                defer { cancel.unlock() }
                return cancelled
            },
            done: $0)
    }
    expect(racing == .cancelled, "a cancelled fetch hands nothing over")
    let staging = try? FileManager.default.contentsOfDirectory(
        atPath: paths.root.appendingPathComponent("staging").path)
    expect(staging == [], "a cancelled fetch leaves nothing in staging")

    // A request the host took and then is given up on: the host finds a marker.
    let takenId = LockIsolated()
    answerNextRequest(with: "", staged: false, publishes: false) { takenId.set($0) }
    let late = finish { client(deadline: 0.1).fetch(itemId: 8, isCancelled: { false }, done: $0) }
    expect(late == .retry, "a deadline after the host took the request answers retry")
    let marked = takenId.value.map { FileManager.default.fileExists(atPath: paths.cancelled($0).path) } ?? false
    expect(marked, "the host is told it was given up on")
    if let id = takenId.value { try? FileManager.default.removeItem(at: paths.cancelled(id)) }

    // A request not yet taken leaves no marker.
    let before = requests()
    _ = finish { client(deadline: 0.05).fetch(itemId: 9, isCancelled: { false }, done: $0) }
    expect(requests() == before, "a request taken back needs no marker")

    // The request itself, in the shape the host parses.
    let shape = NSLock()
    var sent: String?
    answerNextRequest(with: #"{"version":1,"outcome":"retry"}"#, inspect: { body in
        shape.lock()
        sent = body
        shape.unlock()
    })
    _ = finish { client().fetch(itemId: 7, isCancelled: { false }, done: $0) }
    shape.lock()
    expect(sent == #"{"item_id":7,"op":"fetch","version":1,"viewer":false}"#, "the request is what the host parses")
    shape.unlock()
    answerNextRequest(with: #"{"version":1,"outcome":"retry"}"#, inspect: { body in
        shape.lock()
        sent = body
        shape.unlock()
    })
    _ = finish { client().fetch(itemId: 7, viewer: true, isCancelled: { false }, done: $0) }
    shape.lock()
    expect(sent == #"{"item_id":7,"op":"fetch","version":1,"viewer":true}"#, "a double click is what the host's test parses")
    shape.unlock()

    // A create: the bytes go to staging before the request, and the answer names the row.
    let source = base.appendingPathComponent("source.txt")
    try? Data("new".utf8).write(to: source)
    var created: String?
    var outgoingBytes: Data?
    answerNextRequest(
        with: #"{"version":1,"outcome":"written","item_id":42}"#,
        inspect: { body in
            shape.lock()
            created = body
            shape.unlock()
        },
        beforeResult: { id in
            shape.lock()
            outgoingBytes = try? Data(contentsOf: paths.outgoing(id))
            shape.unlock()
        })
    let written = finish {
        client().create(
            parentItem: 3, name: "ñ \"a\"/b.txt", folder: false, contents: source, isCancelled: { false }, done: $0)
    }
    expect(written == .written(itemId: 42), "a create answers with the row")
    shape.lock()
    expect(
        created == #"{"folder":false,"name":"ñ \"a\"/b.txt","op":"create","parent_item":3,"version":1}"#,
        "a create is what the host parses, with the name escaped")
    expect(outgoingBytes == Data("new".utf8), "the bytes are in staging when the request appears")
    shape.unlock()
    answerNextRequest(with: #"{"version":1,"outcome":"collision"}"#)
    let taken = finish {
        client().create(parentItem: 3, name: "x", folder: true, contents: nil, isCancelled: { false }, done: $0)
    }
    expect(taken == .collision, "a taken name answers collision")

    // A modify and a delete, in the shapes the host parses, and their answers.
    var modified: String?
    answerNextRequest(with: #"{"version":1,"outcome":"conflict","item_id":5}"#, inspect: { body in
        shape.lock()
        modified = body
        shape.unlock()
    })
    let edited = finish {
        client().modify(
            itemId: 5, baseVersion: Data("c1:base".utf8), newPlace: (parent: 2, name: "b.txt"), contents: source,
            isCancelled: { false }, done: $0)
    }
    expect(edited == .conflict(itemId: 5), "a stale edit answers conflict")
    shape.lock()
    expect(
        modified
            == #"{"base_version":"c1:base","content":true,"item_id":5,"new_name":"b.txt","new_parent":2,"op":"modify","version":1}"#,
        "a modify is what the host parses: \(modified ?? "nil")")
    shape.unlock()
    var deleting: String?
    answerNextRequest(with: #"{"version":1,"outcome":"deletion_rejected"}"#, inspect: { body in
        shape.lock()
        deleting = body
        shape.unlock()
    })
    let rejected = finish {
        client().delete(itemId: 6, baseVersion: Data("c1:x".utf8), isCancelled: { false }, done: $0)
    }
    expect(rejected == .deletionRejected, "a delete Dropbox would lose an edit to is rejected")
    shape.lock()
    expect(
        deleting == #"{"base_version":"c1:x","item_id":6,"op":"delete","version":1}"#,
        "a delete is what the host parses: \(deleting ?? "nil")")
    shape.unlock()
    answerNextRequest(with: #"{"version":1,"outcome":"deleted"}"#)
    let deletedOutcome = finish {
        client().delete(itemId: 6, baseVersion: Data("c1:x".utf8), isCancelled: { false }, done: $0)
    }
    expect(deletedOutcome == .deleted, "a delete answers deleted")
    answerNextRequest(with: #"{"version":1,"outcome":"gone"}"#)
    let goneOutcome = finish {
        client().modify(
            itemId: 5, baseVersion: Data("c1:base".utf8), newPlace: nil, contents: source,
            isCancelled: { false }, done: $0)
    }
    expect(goneOutcome == .gone, "an edit of something deleted on Dropbox answers gone")
    answerNextRequest(with: #"{"version":1,"outcome":"gone_parent"}"#)
    let goneParent = finish {
        client().modify(
            itemId: 5, baseVersion: Data("c1:base".utf8), newPlace: (parent: 9, name: "a"), contents: nil,
            isCancelled: { false }, done: $0)
    }
    expect(goneParent == .goneParent, "a move into a folder deleted on Dropbox names the folder")

    // A create given up on before the host took it leaves no bytes behind.
    _ = finish {
        client(deadline: 0.05).create(
            parentItem: 3, name: "y", folder: false, contents: source, isCancelled: { false }, done: $0)
    }
    let leftovers = (try? FileManager.default.contentsOfDirectory(
        atPath: paths.root.appendingPathComponent("staging").path)) ?? []
    expect(leftovers.isEmpty, "a create taken back takes its bytes back: \(leftovers)")

    // The handover.
    let row = ProjectionTests.row(1)
    var current = row
    current.contentVersion = Data("c1:new".utf8)
    expect(
        rowToHandOver(stagedVersion: Data("c1:new".utf8), presence: .live(current)) == current,
        "the staged version of a live row is handed over")
    expect(
        rowToHandOver(stagedVersion: Data("c1:old".utf8), presence: .live(current)) == nil,
        "a row that moved on is not")
    expect(rowToHandOver(stagedVersion: row.contentVersion, presence: .notLive) == nil, "nor a row that went")
    expect(rowToHandOver(stagedVersion: row.contentVersion, presence: .unknown) == nil, "nor an unknown one")

    // The host dies during the wait: the wait ends, and the request goes.
    let died = finish { done in
        DispatchQueue.global().asyncAfter(deadline: .now() + 0.05) { close(lock) }
        client().fetch(itemId: 7, isCancelled: { false }, done: done)
    }
    expect(died == .hostDown, "a host that dies during the wait answers hostDown")
    expect(requests().isEmpty, "a host that died leaves no request")
}
}
