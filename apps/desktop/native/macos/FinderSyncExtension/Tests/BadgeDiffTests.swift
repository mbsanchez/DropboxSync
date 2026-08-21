// Executable checks for BadgeDiff (DBSYNC-84).
//
// There is no Swift test target in this project and adding one is out of scope
// (DBSYNC-72). This follows the precedent set by DBSYNC-72 Slice 1, which proved path
// equivalence by running Swift directly rather than by grepping. Run it with:
//
//     ./Tests/run-badge-diff-tests.sh
//
// It compiles the REAL BadgeDiff.swift, not a copy of its logic. A test that restated the
// algorithm would pass while the shipped code was wrong, which is worse than no test.
//
// `@main` rather than top-level statements: `swiftc` only allows top-level code in a file
// literally named `main.swift`, and `swift a.swift b.swift` compiles the second file
// without putting it in scope for the first. Both were tried; this is what works while
// keeping the file's name descriptive.
//
// Exits 0 on success, 1 if any check fails, naming what broke.

import Foundation

@main
struct BadgeDiffTests {
    static var failures = 0

    static func check(
        _ label: String,
        _ actual: BadgeDiff.Changes,
        changed: [String: String],
        removed: [String]
    ) {
        let expected = BadgeDiff.Changes(changed: changed, removed: removed)
        if actual == expected {
            print("  ok   \(label)")
        } else {
            print("  FAIL \(label)")
            print("       expected changed=\(changed.sorted { $0.key < $1.key }) removed=\(removed)")
            print("       actual   changed=\(actual.changed.sorted { $0.key < $1.key }) removed=\(actual.removed)")
            failures += 1
        }
    }

    static func checkBool(_ label: String, _ actual: Bool, _ expected: Bool) {
        if actual == expected {
            print("  ok   \(label)")
        } else {
            print("  FAIL \(label) — expected \(expected), got \(actual)")
            failures += 1
        }
    }

    static func checkOptional(_ label: String, _ actual: String?, _ expected: String?) {
        if actual == expected {
            print("  ok   \(label)")
        } else {
            print("  FAIL \(label) — expected \(expected as Any), got \(actual as Any)")
            failures += 1
        }
    }

    static func checkOutcome(
        _ label: String,
        _ actual: BadgeDiff.RequestOutcome,
        displayedPath: String?,
        badgeIdentifier: String?
    ) {
        let expected = BadgeDiff.RequestOutcome(
            displayedPath: displayedPath, badgeIdentifier: badgeIdentifier)
        if actual == expected {
            print("  ok   \(label)")
        } else {
            print("  FAIL \(label)")
            print("       expected \(expected)")
            print("       actual   \(actual)")
            failures += 1
        }
    }

    static func main() {
        print("BadgeDiff")

        // The first-load guard. This is what protects a 50,000-file store from a push storm
        // at startup, so it is checked first and explicitly.
        check("nil old snapshot yields nothing, however large the new one",
              BadgeDiff.changes(from: nil, to: ["a.txt": "synced", "b.txt": "cloud_only"]),
              changed: [:], removed: [])

        check("empty to empty",
              BadgeDiff.changes(from: [:], to: [:]),
              changed: [:], removed: [])

        // An empty dictionary is NOT the same as nil: the store was read and was genuinely
        // empty, so newly appearing paths are real changes and must be pushed.
        check("empty old snapshot still reports additions",
              BadgeDiff.changes(from: [:], to: ["a.txt": "synced"]),
              changed: ["a.txt": "synced"], removed: [])

        check("a new path is a change",
              BadgeDiff.changes(from: ["a.txt": "synced"],
                                to: ["a.txt": "synced", "b.txt": "cloud_only"]),
              changed: ["b.txt": "cloud_only"], removed: [])

        // The bug this ticket exists for: a file finishes syncing and its tier flips.
        check("a changed tier is reported",
              BadgeDiff.changes(from: ["a.txt": "syncing"], to: ["a.txt": "synced"]),
              changed: ["a.txt": "synced"], removed: [])

        // The 2-second poll runs constantly; an unchanged map must produce no work at all.
        check("identical snapshots report nothing",
              BadgeDiff.changes(from: ["a.txt": "synced", "b.txt": "cloud_only"],
                                to: ["a.txt": "synced", "b.txt": "cloud_only"]),
              changed: [:], removed: [])

        check("a removed path is reported",
              BadgeDiff.changes(from: ["a.txt": "synced", "b.txt": "cloud_only"],
                                to: ["a.txt": "synced"]),
              changed: [:], removed: ["b.txt"])

        check("everything removed at once",
              BadgeDiff.changes(from: ["a.txt": "synced", "b.txt": "cloud_only"], to: [:]),
              changed: [:], removed: ["a.txt", "b.txt"])

        check("additions, changes and removals together",
              BadgeDiff.changes(
                  from: ["keep.txt": "synced", "flip.txt": "syncing", "gone.txt": "synced"],
                  to: ["keep.txt": "synced", "flip.txt": "synced", "new.txt": "cloud_only"]),
              changed: ["flip.txt": "synced", "new.txt": "cloud_only"], removed: ["gone.txt"])

        print("BadgeDiff.pushable")

        // Apple's header: "call this method only for items that have already received a
        // badge". A tier change in a folder Finder never opened must NOT be pushed.
        check("a path Finder has never displayed is not pushable",
              BadgeDiff.pushable(
                  BadgeDiff.Changes(changed: ["never-seen.txt": "synced"], removed: []),
                  displayed: []),
              changed: [:], removed: [])

        check("a displayed path is pushable",
              BadgeDiff.pushable(
                  BadgeDiff.Changes(changed: ["seen.txt": "synced"], removed: []),
                  displayed: ["seen.txt"]),
              changed: ["seen.txt": "synced"], removed: [])

        // DBSYNC-84 symptom 1, and the case whose absence let a green run hide the hole.
        // A `.cloudsc` placeholder is drawn before the state file catches up: Finder asks,
        // there is no tier yet, so nothing is badged — but the path IS displayed. When the
        // tier arrives it must be pushed, because Finder will not ask a second time.
        check("a DISPLAYED but never-badged path is still pushable",
              BadgeDiff.pushable(
                  BadgeDiff.Changes(changed: ["new.cloudsc": "cloud_only"], removed: []),
                  displayed: ["new.cloudsc"]),
              changed: ["new.cloudsc": "cloud_only"], removed: [])

        check("mixed displayed and undisplayed keeps only the displayed",
              BadgeDiff.pushable(
                  BadgeDiff.Changes(changed: ["seen.txt": "synced", "never.txt": "syncing"],
                                    removed: ["gone-seen.txt", "gone-never.txt"]),
                  displayed: ["seen.txt", "gone-seen.txt"]),
              changed: ["seen.txt": "synced"], removed: ["gone-seen.txt"])

        check("removals are filtered too",
              BadgeDiff.pushable(
                  BadgeDiff.Changes(changed: [:], removed: ["never-seen.txt"]),
                  displayed: []),
              changed: [:], removed: [])

        print("BadgeDiff.isContained")

        let root = "/Users/x/DropboxSync"

        checkBool("a file inside the root is contained",
                  BadgeDiff.isContained("/Users/x/DropboxSync/a.txt", in: root), true)
        checkBool("the root itself is contained",
                  BadgeDiff.isContained(root, in: root), true)
        // The bug the first version of this change shipped: a plain hasPrefix passes this.
        checkBool("a SIBLING whose name extends the root is NOT contained",
                  BadgeDiff.isContained("/Users/x/DropboxSyncEvil/secret.txt", in: root), false)
        checkBool("an unrelated absolute path is not contained",
                  BadgeDiff.isContained("/etc/passwd", in: root), false)
        checkBool("a trailing separator on the root does not change the answer",
                  BadgeDiff.isContained("/Users/x/DropboxSync/a.txt", in: root + "/"), true)
        checkBool("a prefix of the root is not contained",
                  BadgeDiff.isContained("/Users/x", in: root), false)

        print("BadgeDiff.relativePath")

        checkOptional("strips the root and the separator",
                      BadgeDiff.relativePath(of: "/Users/x/DropboxSync/sub/a.txt", under: root),
                      "sub/a.txt")
        checkOptional("the root itself is the empty relative path",
                      BadgeDiff.relativePath(of: root, under: root), "")
        // The old inline version returned "Evil/secret.txt" here and looked it up as if real.
        checkOptional("a sibling yields nil, not a nonsense relative path",
                      BadgeDiff.relativePath(of: "/Users/x/DropboxSyncEvil/secret.txt", under: root),
                      nil)
        checkOptional("an unrelated path yields nil",
                      BadgeDiff.relativePath(of: "/etc/passwd", under: root), nil)

        print("BadgeDiff.resync")

        check("resync refreshes only the displayed paths",
              BadgeDiff.resync(to: ["a.txt": "synced", "b.txt": "cloud_only"],
                               displayed: ["a.txt"]),
              changed: ["a.txt": "synced"], removed: [])

        check("resync clears displayed paths that are gone from the new snapshot",
              BadgeDiff.resync(to: ["a.txt": "synced"], displayed: ["a.txt", "gone.txt"]),
              changed: ["a.txt": "synced"], removed: ["gone.txt"])

        check("resync with nothing displayed does nothing",
              BadgeDiff.resync(to: ["a.txt": "synced"], displayed: []),
              changed: [:], removed: [])

        print("BadgeDiff.requestOutcome")

        let tracked = ["a.txt": "synced"]

        checkOutcome("a tracked file is displayed and badged with its tier",
                     BadgeDiff.requestOutcome(for: "/Users/x/DropboxSync/a.txt",
                                              root: root, paths: tracked),
                     displayedPath: "a.txt", badgeIdentifier: "synced")

        // THE REGRESSION GUARD. Recording the path only after a tier was found passed 27
        // green checks and a clean build; this is the check that would have failed.
        checkOutcome("an UNTRACKED file inside the root still counts as displayed",
                     BadgeDiff.requestOutcome(for: "/Users/x/DropboxSync/new.cloudsc",
                                              root: root, paths: tracked),
                     displayedPath: "new.cloudsc", badgeIdentifier: "")

        checkOutcome("with no state at all, the path is still displayed",
                     BadgeDiff.requestOutcome(for: "/Users/x/DropboxSync/a.txt",
                                              root: root, paths: nil),
                     displayedPath: "a.txt", badgeIdentifier: "")

        checkOutcome("a sibling directory is neither displayed nor badged",
                     BadgeDiff.requestOutcome(for: "/Users/x/DropboxSyncEvil/secret.txt",
                                              root: root, paths: tracked),
                     displayedPath: nil, badgeIdentifier: nil)

        checkOutcome("the sync folder itself is not an item",
                     BadgeDiff.requestOutcome(for: root, root: root, paths: tracked),
                     displayedPath: nil, badgeIdentifier: nil)

        print("round trip: removed then tracked again")

        // A file leaves the state map and comes back. Finder has shown it the whole time,
        // so it must be pushable on the way out AND on the way back in.
        //
        // Honest about what this does and does not guard: it pins the composition of
        // `changes` and `pushable`. It cannot catch someone dropping the path from the
        // displayed set inside `SyncExtension.swift` — that is a property of a line that no
        // longer exists, documented on `displayedPaths` instead. The reason `displayedPaths`
        // never shrinks is written there, not enforced here.
        let displayed: Set<String> = ["a.txt"]

        let leaving = BadgeDiff.pushable(
            BadgeDiff.changes(from: ["a.txt": "synced"], to: [:]), displayed: displayed)
        check("on the way out, the badge is cleared",
              leaving, changed: [:], removed: ["a.txt"])

        let returning = BadgeDiff.pushable(
            BadgeDiff.changes(from: [:], to: ["a.txt": "synced"]), displayed: displayed)
        check("on the way back, the badge is set again",
              returning, changed: ["a.txt": "synced"], removed: [])

        if failures == 0 {
            print("BadgeDiff: all checks passed")
            exit(0)
        } else {
            print("BadgeDiff: \(failures) check(s) FAILED")
            exit(1)
        }
    }
}
