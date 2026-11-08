// Executable checks for ProjectionModel and ProjectionStore (DBSYNC-95 slice 2b). See
// run-projection-tests.sh. There is no Swift test target in this project, so this follows
// FinderSyncExtension/Tests/BadgeDiffTests.swift: it compiles the REAL sources and exits non-zero when a
// check fails. `@main` because `swiftc` only allows top-level code in a file named `main.swift`.

import Foundation
import SQLite3

@main
struct ProjectionTests {
    static var failures = 0

    static func check(_ label: String, _ ok: Bool) {
        if !ok {
            failures += 1
            print("FAILED: \(label)")
        }
    }

    static func row(_ id: Int64, state: Int64 = 0, seq: Int64 = 0) -> ProjectionRow {
        ProjectionRow(
            itemId: id, parentItem: 0, name: "n\(id)", isFolder: false, sizeBytes: 1, modifiedTs: 1,
            contentVersion: Data([1]), metaVersion: Data([2]), state: state, changedSeq: seq)
    }

    static func head(generation: Int64 = 7, seq: Int64 = 100, minValid: Int64 = 50) -> ProjectionHead {
        ProjectionHead(
            schemaVersion: 1, generation: generation, seq: seq, minValidSeq: minValid, populated: true)
    }

    static func itemIds() {
        check("id round trip", ItemIdCodec.decode(ItemIdCodec.encode(42)) == 42)
        for bad in ["", "i", "i0", "i-1", "i+1", "i01", "i1x", "42", "tracer-item", "NSFileProviderRootContainerItemIdentifier"] {
            check("not an id: \(bad)", ItemIdCodec.decode(bad) == nil)
        }
    }

    static func anchors() {
        let anchor = ProjectionAnchor(generation: 7, seq: 60)
        check("anchor format", String(data: anchor.encoded, encoding: .utf8) == "v1.7.60")
        check("anchor round trip", ProjectionAnchor(encoded: anchor.encoded) == anchor)
        // The tracer's anchor must expire into a rescan, not be read as a position.
        for bad in ["1", "v1.7", "v2.7.60", "v1.x.60", "v1.7.", "v1.7.60.1", ""] {
            check("not an anchor: \(bad)", ProjectionAnchor(encoded: Data(bad.utf8)) == nil)
        }
        // The cases of `anchor_expired` in the host.
        check("valid", !anchor.isExpired(against: head()))
        check("at min_valid_seq", !ProjectionAnchor(generation: 7, seq: 50).isExpired(against: head()))
        check("at the head", !ProjectionAnchor(generation: 7, seq: 100).isExpired(against: head()))
        check("other generation", anchor.isExpired(against: head(generation: 8)))
        check("below min_valid_seq", ProjectionAnchor(generation: 7, seq: 49).isExpired(against: head()))
        check("past the head", ProjectionAnchor(generation: 7, seq: 101).isExpired(against: head()))
    }

    static func pages() {
        // NSFileProviderInitialPageSortedByName and ...ByDate, as printed from the SDK.
        for system in ["FPPageSortedByName\0", "FPPageSortedByDate\0"] {
            check("system first page \(system)", PageCodec.afterItem(Data(system.utf8)) == 0)
        }
        for other in ["", "9", "x9", "p", "p0", "p-3", "px"] {
            check("not a page of ours: \(other)", PageCodec.afterItem(Data(other.utf8)) == 0)
        }
        check("our page", PageCodec.afterItem(PageCodec.encode(afterItem: 9)) == 9)

        let one = PageCodec.next(rows: [row(1), row(2)], batch: 2)
        check("a full last page has no next", one.next == nil && one.page.count == 2)
        let two = PageCodec.next(rows: [row(1), row(2), row(3)], batch: 2)
        check("the extra row is not served", two.page == [row(1), row(2)])
        check("next starts after the last served", two.next.map(PageCodec.afterItem) == 2)
    }

    static func plans() {
        let mixed = ChangePlan.make(
            rows: [row(1, state: 0, seq: 60), row(2, state: 1, seq: 61), row(3, state: 2, seq: 62), row(4, state: 3, seq: 63)],
            batch: 10, head: head())
        check("live is updated", mixed.updated.map(\.itemId) == [1])
        check("every other state is deleted", mixed.deletedItemIds == [2, 3, 4])
        check("the last batch reaches the head", mixed.upTo.seq == 100 && !mixed.moreComing)

        let partial = ChangePlan.make(
            rows: [row(1, seq: 60), row(2, seq: 61), row(3, seq: 62)], batch: 2, head: head())
        check("a partial batch stops at its last row", partial.upTo.seq == 61 && partial.moreComing)
        check("the extra row is not reported", partial.updated.map(\.itemId) == [1, 2])

        let exact = ChangePlan.make(rows: [row(1, seq: 60), row(2, seq: 61)], batch: 2, head: head())
        check("an exact batch is the last one", !exact.moreComing && exact.upTo.seq == 100)

        let idle = ChangePlan.make(rows: [], batch: 2, head: head())
        check("nothing new", idle.updated.isEmpty && idle.deletedItemIds.isEmpty && idle.upTo.seq == 100)
    }

    static func readAnswers() {
        // What can pass by itself retries; the rest waits for the host's signal.
        check("busy retries", ReadFailure.query(5).answer == .retry(code: 5))
        check("locked retries", ReadFailure.query(6).answer == .retry(code: 6))
        check("an I/O error retries", ReadFailure.open(10).answer == .retry(code: 10))
        check("an extended code retries", ReadFailure.query(522).answer == .retry(code: 522))
        for code: Int32 in [1, 11, 14, 26] {
            check("\(code) waits", ReadFailure.open(code).answer == .waitForSignal)
            check("\(code) waits", ReadFailure.query(code).answer == .waitForSignal)
        }
        for waiting in [ReadFailure.noContainer, .noState, .schemaTooNew(2), .notPopulated] {
            check("\(waiting) waits for a signal", waiting.answer == .waitForSignal)
        }
        let transient = ReadAnswer.transientError(code: 14)
        check("the header's form", transient.domain == NSCocoaErrorDomain && transient.code == NSXPCConnectionReplyInvalid)
        check("the code survives", (transient.userInfo[NSUnderlyingErrorKey] as? NSError)?.code == 14)
        let archived = try? NSKeyedArchiver.archivedData(withRootObject: transient, requiringSecureCoding: true)
        let back = archived.flatMap { try? NSKeyedUnarchiver.unarchivedObject(ofClass: NSError.self, from: $0) }
        check("it can cross XPC", back?.code == NSXPCConnectionReplyInvalid)
    }

    static func main() {
        itemIds()
        anchors()
        pages()
        plans()
        readAnswers()
        storeChecks()
        exchangeChecks()
        if failures == 0 {
            print("Projection: all checks passed")
            exit(0)
        }
        print("Projection: \(failures) check(s) FAILED")
        exit(1)
    }
}
