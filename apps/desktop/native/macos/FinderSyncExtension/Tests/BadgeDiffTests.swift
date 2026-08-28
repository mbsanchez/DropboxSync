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
                  displayed: [], observedDirectories: []),
              changed: [:], removed: [])

        check("a displayed path is pushable",
              BadgeDiff.pushable(
                  BadgeDiff.Changes(changed: ["seen.txt": "synced"], removed: []),
                  displayed: ["seen.txt"], observedDirectories: []),
              changed: ["seen.txt": "synced"], removed: [])

        // DBSYNC-84 symptom 1, and the case whose absence let a green run hide the hole.
        // A `.cloudsc` placeholder is drawn before the state file catches up: Finder asks,
        // there is no tier yet, so nothing is badged — but the path IS displayed. When the
        // tier arrives it must be pushed, because Finder will not ask a second time.
        check("a DISPLAYED but never-badged path is still pushable",
              BadgeDiff.pushable(
                  BadgeDiff.Changes(changed: ["new.cloudsc": "cloud_only"], removed: []),
                  displayed: ["new.cloudsc"], observedDirectories: []),
              changed: ["new.cloudsc": "cloud_only"], removed: [])

        check("mixed displayed and undisplayed keeps only the displayed",
              BadgeDiff.pushable(
                  BadgeDiff.Changes(changed: ["seen.txt": "synced", "never.txt": "syncing"],
                                    removed: ["gone-seen.txt", "gone-never.txt"]),
                  displayed: ["seen.txt", "gone-seen.txt"], observedDirectories: []),
              changed: ["seen.txt": "synced"], removed: ["gone-seen.txt"])

        check("removals are filtered too",
              BadgeDiff.pushable(
                  BadgeDiff.Changes(changed: [:], removed: ["never-seen.txt"]),
                  displayed: [], observedDirectories: []),
              changed: [:], removed: [])

        print("BadgeDiff.pushable — files born in an open directory (DBSYNC-87)")

        // THE BUG. Hydration downloads `foo.txt` and deletes `foo.txt.cloudsc`, so the tier
        // arrives against a path Finder never asked about — it only asks while enumerating,
        // and the folder was already open. Before this, the push was dropped and the file
        // stayed bare until the user left the directory and came back.
        check("a path never asked about IS pushable when its directory is on screen",
              BadgeDiff.pushable(
                  BadgeDiff.Changes(changed: ["hydrated.txt": "synced"], removed: []),
                  displayed: [], observedDirectories: [""]),
              changed: ["hydrated.txt": "synced"], removed: [])

        check("the same path in a subfolder on screen is pushable too",
              BadgeDiff.pushable(
                  BadgeDiff.Changes(changed: ["sub/hydrated.txt": "synced"], removed: []),
                  displayed: [], observedDirectories: ["sub"]),
              changed: ["sub/hydrated.txt": "synced"], removed: [])

        // The widening must stay narrow. Finder showing `sub` does not put the contents of
        // `sub/deep` on screen, and badging them would be the thing Apple's header warns
        // against — the opposite mistake to the one being fixed.
        check("a file DEEPER than the observed directory is not pushable",
              BadgeDiff.pushable(
                  BadgeDiff.Changes(changed: ["sub/deep/buried.txt": "synced"], removed: []),
                  displayed: [], observedDirectories: ["sub"]),
              changed: [:], removed: [])

        check("an observed directory does not make a SIBLING directory pushable",
              BadgeDiff.pushable(
                  BadgeDiff.Changes(changed: ["other/a.txt": "synced"], removed: []),
                  displayed: [], observedDirectories: ["sub"]),
              changed: [:], removed: [])

        // A file deleted from a folder that is open must lose its badge for the same reason
        // it would gain one: the user is looking at it.
        check("removals in an observed directory are pushable",
              BadgeDiff.pushable(
                  BadgeDiff.Changes(changed: [:], removed: ["gone.txt"]),
                  displayed: [], observedDirectories: [""]),
              changed: [:], removed: ["gone.txt"])

        // Closing the window must stop the pushes; `endObservingDirectory` empties the set.
        check("once the directory is no longer observed, pushes stop",
              BadgeDiff.pushable(
                  BadgeDiff.Changes(changed: ["hydrated.txt": "synced"], removed: []),
                  displayed: [], observedDirectories: []),
              changed: [:], removed: [])

        // Belt and braces: the two admission routes are independent, not a replacement.
        check("displayed and observed both admit, in one call",
              BadgeDiff.pushable(
                  BadgeDiff.Changes(
                      changed: ["asked.txt": "synced", "sub/born.txt": "cloud_only",
                                "elsewhere/no.txt": "syncing"],
                      removed: []),
                  displayed: ["asked.txt"], observedDirectories: ["sub"]),
              changed: ["asked.txt": "synced", "sub/born.txt": "cloud_only"], removed: [])

        // The first-load guard still has the last word: nothing to push means nothing pushed,
        // however many directories are open. This is what keeps 50,000 files from storming.
        check("an open directory does not defeat the first-load guard",
              BadgeDiff.pushable(
                  BadgeDiff.changes(from: nil, to: ["a.txt": "synced", "b.txt": "cloud_only"]),
                  displayed: [], observedDirectories: [""]),
              changed: [:], removed: [])

        print("BadgeDiff.parentDirectory")

        checkOptional("a file in the sync root has the root as its parent",
                      BadgeDiff.parentDirectory(of: "a.txt"), "")
        checkOptional("a file in a subfolder yields that subfolder",
                      BadgeDiff.parentDirectory(of: "sub/a.txt"), "sub")
        checkOptional("only the last separator is cut",
                      BadgeDiff.parentDirectory(of: "sub/deep/a.txt"), "sub/deep")

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
            BadgeDiff.changes(from: ["a.txt": "synced"], to: [:]),
            displayed: displayed, observedDirectories: [])
        check("on the way out, the badge is cleared",
              leaving, changed: [:], removed: ["a.txt"])

        let returning = BadgeDiff.pushable(
            BadgeDiff.changes(from: [:], to: ["a.txt": "synced"]),
            displayed: displayed, observedDirectories: [])
        check("on the way back, the badge is set again",
              returning, changed: ["a.txt": "synced"], removed: [])

        print("end to end: hydrating a .cloudsc with the folder open (DBSYNC-87)")

        // The reported bug, as a single composition rather than as separate pieces.
        //
        // The user is looking at the sync root. `report.pdf.cloudsc` is on screen and badged
        // `cloud_only`, so Finder has asked about it. They open it; the app downloads
        // `report.pdf` and deletes the placeholder, so ONE snapshot swaps one path for
        // another. The old path must lose its badge and the new one must gain the green
        // check — without the user leaving the directory, because leaving and coming back is
        // the gesture that hid this bug for two tickets.
        let beforeHydration = ["report.pdf.cloudsc": "cloud_only"]
        let afterHydration = ["report.pdf": "synced"]

        check("the hydrated file is badged and the placeholder is cleared, in place",
              BadgeDiff.pushable(
                  BadgeDiff.changes(from: beforeHydration, to: afterHydration),
                  displayed: ["report.pdf.cloudsc"],
                  observedDirectories: [""]),
              changed: ["report.pdf": "synced"], removed: ["report.pdf.cloudsc"])

        // And the proof that the observed set is what carries it: with the folder closed,
        // the same snapshot pair produces exactly the old broken behaviour — the placeholder
        // is cleared because Finder asked about it, and the hydrated file stays bare.
        check("with the folder closed, only the placeholder is touched",
              BadgeDiff.pushable(
                  BadgeDiff.changes(from: beforeHydration, to: afterHydration),
                  displayed: ["report.pdf.cloudsc"],
                  observedDirectories: []),
              changed: [:], removed: ["report.pdf.cloudsc"])

        print("OverlayState.decode (DBSYNC-73)")

        // The payload the ticket was written against, with the underscores that matter.
        let json = Data("""
        {
          "version": 1,
          "updated_at": "2026-08-25T12:00:00Z",
          "sync_folder": "/Users/x/DropboxSync",
          "paths": {
            "docs/my_report.pdf": "synced",
            "a_b_c/x_y.md": "cloud_only",
            "plain.txt": "syncing"
          }
        }
        """.utf8)

        // `try?` with a named failure rather than `try!`: a trap reports as
        // "Trace/BPT trap: 5" with no indication of which check died.
        guard let decoded = try? OverlayState.decode(from: json) else {
            print("  FAIL OverlayState.decode threw on a valid payload")
            failures += 1
            exit(1)
        }

        // THE BUG. `.convertFromSnakeCase` applied to every key the decoder saw, including
        // the keys of `paths` — which are file paths. On macOS 12-14 Foundation,
        // "docs/my_report.pdf" arrived as "docs/myReport.pdf", matched nothing, and that
        // file silently rendered no badge.
        checkBool("underscored path keys survive decoding",
                  decoded.paths["docs/my_report.pdf"] == "synced", true)
        checkBool("underscored path keys are not camel-cased",
                  decoded.paths["docs/myReport.pdf"] == nil, true)
        checkBool("a path with several underscores survives too",
                  decoded.paths["a_b_c/x_y.md"] == "cloud_only", true)

        // The two properties that DO need mapping. If these regress, the decode throws
        // rather than silently yielding a wrong value — which is the point of naming them.
        checkOptional("updated_at maps to updatedAt", decoded.updatedAt, "2026-08-25T12:00:00Z")
        checkOptional("sync_folder maps to syncFolder", decoded.syncFolder, "/Users/x/DropboxSync")

        // A tier VALUE, on a key with no underscores at all. The previous version of this
        // check asserted the same key/value pair as the one three lines above, so no input
        // could redden one without the other — six checks over five properties. Pointing it
        // at `plain.txt` makes it earn its name: it isolates the value position from the
        // key position entirely.
        checkOptional("a tier value on an underscore-free key is untouched",
                      decoded.paths["plain.txt"], "syncing")

        print("logSafeDescription (DBSYNC-73)")

        // THE LEAK THIS EXISTS TO PREVENT. `DecodingError.codingPath` inside `paths`
        // contains the user's relative file paths, and NSLog writes to the PUBLIC unified
        // log. `String(describing:)` on this error yields:
        //     typeMismatch … Path: paths.`Clients/AcmeCorp_NDA_signed.pdf`
        // Making the decode failure audible must not make it a privacy leak.
        let leaky = Data("""
        {
          "version": 1,
          "updated_at": "2026-08-26T00:00:00Z",
          "sync_folder": "/Users/x/DropboxSync",
          "paths": { "Clients/AcmeCorp_NDA_signed.pdf": 42 }
        }
        """.utf8)

        do {
            _ = try OverlayState.decode(from: leaky)
            print("  FAIL a type-mismatched tier should not decode")
            failures += 1
        } catch {
            let safe = logSafeDescription(of: error)
            checkBool("a decode error never names the user's file",
                      safe.contains("AcmeCorp_NDA_signed"), false)
            checkBool("a decode error never leaks the coding path",
                      safe.lowercased().contains("clients/"), false)
            checkBool("but it still says what kind of failure it was",
                      safe.contains("typeMismatch"), true)
            // The naive version this replaced would fail the first two.
            checkBool("the raw description WOULD have leaked it (guard is load-bearing)",
                      String(describing: error).contains("AcmeCorp_NDA_signed"), true)
        }

        // The failure both DBSYNC-73 mutations produce. Its key comes from our own schema,
        // never from `paths`, so keeping it is safe and is what makes the log useful.
        let missingKey = Data("""
        {"version": 1, "sync_folder": null, "paths": {}}
        """.utf8)
        do {
            _ = try OverlayState.decode(from: missingKey)
            print("  FAIL a payload missing updated_at should not decode")
            failures += 1
        } catch {
            checkBool("a missing schema key is named, because it is ours",
                      logSafeDescription(of: error).contains("updated_at"), true)
        }

        print("OverlayState version guard (DBSYNC-91)")

        // The case a best-effort reader accepts happily: EVERYTHING is valid except the
        // version. Same shape as the payload at the top of this section, same keys, same
        // tiers — so nothing but `version` can be responsible for the outcome.
        //
        // The sensitive-looking key is a TRIPWIRE, and it is worth being precise about what
        // it is and is not. `OverlayStateError` carries an `Int` and nothing else, so
        // `String(describing:)` on it is "unsupportedVersion(2)" — there is no path in the
        // type to leak, and no mutation of today's code can redden the check below.
        //
        // That makes it weaker evidence than the DecodingError privacy check above, which
        // earns its keep through the paired control at "the raw description WOULD have
        // leaked it". No such control is possible here; a first draft of this section
        // claimed the two cleared the same bar, and review caught that they do not.
        //
        // Kept for what it can actually catch, which is narrower than the first two attempts
        // at this comment claimed: a String added to `.unsupportedVersion` itself, or to its
        // branch in `logSafeDescription` — say a "near <path>" context that seemed helpful.
        // That reddens.
        //
        // It does NOT catch a future `OverlayStateError` case carrying a path, which is what
        // the previous version of this comment promised: nothing in this suite constructs
        // such a case, so adding one leaves every check green. If a case like that is added,
        // it needs its own check; this one will not notice.
        //
        // Nor is it independent even within its own scope — a leak added to
        // `.unsupportedVersion` reddens the exact `checkOptional` below it too.
        let futureVersion = Data("""
        {
          "version": 2,
          "updated_at": "2026-08-27T12:00:00Z",
          "sync_folder": "/Users/x/DropboxSync",
          "paths": { "Clients/AcmeCorp_NDA_signed.pdf": "synced" }
        }
        """.utf8)

        do {
            _ = try OverlayState.decode(from: futureVersion)
            print("  FAIL an unknown version should be refused, not applied as v1")
            failures += 1
        } catch {
            // Reaching this branch at all IS the refusal check — the `do` side above
            // increments `failures` if the decode succeeds. A `checkBool(true, true)` here
            // would print an extra "ok" line that no input could ever redden.
            // Exact, not `contains("2")`: that also passes on `unsupportedVersion(12)` and
            // on `unsupportedVersion(-2)`, so it was looser than its own label. Exact is no
            // weaker against any mutation.
            let safe = logSafeDescription(of: error)
            checkOptional("the refusal is reported exactly, naming the version found",
                          safe, "unsupportedVersion(2)")
            checkBool("a refusal never names the user's file (tripwire — see above)",
                      safe.contains("AcmeCorp_NDA_signed"), false)
            // The verb the caller logs, on the error `decode` ACTUALLY threw rather than a
            // hand-built literal — which is what this checked in its first draft, sitting
            // orphaned between two unrelated blocks. A refused file parsed fine, so "cannot
            // decode" would send the reader after corruption or after DBSYNC-76's sandbox
            // problems. The other direction is checked at the malformed-bytes control below;
            // both are needed, because a `logVerb` stuck on either constant passes one.
            checkOptional("a refusal is called a refusal, not a decode failure",
                          logVerb(for: error), "refusing")
        }

        // NO CHECK HERE FOR THE ACCEPT DIRECTION, and the omission is deliberate.
        //
        // A `checkBool("the supported version is still accepted", decode(json) != nil)`
        // stood here and was green by construction: `json` is decoded at the top of the
        // DBSYNC-73 section by a `guard` that calls `exit(1)`, so merely reaching this line
        // proves the decode succeeded. Its comment claimed the opposite — that a guard
        // written the wrong way round would leave "every check above" passing. Inverting the
        // guard shows what actually happens:
        //
        //     OverlayState.decode (DBSYNC-73)
        //       FAIL OverlayState.decode threw on a valid payload
        //
        // and the run exits before this section prints at all. So the accept direction IS
        // covered, by that `guard let decoded = try? OverlayState.decode(from: json)`, and a
        // check on THAT payload after it can never add anything — the guard has already
        // proved the decode succeeded. A check using a DIFFERENT v1 payload would be live;
        // it is omitted as redundant rather than as impossible.
        //
        // Recorded rather than silently deleted because "a check that cannot fail is not
        // evidence" is this project's rule, and this file shipped a violation of it in a
        // change whose entire argument was mutation-tested evidence.

        // THE CASE THE GUARD ABOVE CANNOT REACH (DBSYNC-91 slice 2). `paths` is an object
        // where v1 expects a string, so the decode throws before any version is available
        // to check — and this is the SHAPE a real breaking change would most likely take.
        // Reported as `typeMismatch` before the fallback existed, which sends whoever reads
        // the log looking for corruption instead of a mismatched install.
        let reshapedFuture = Data("""
        {
          "version": 2,
          "updated_at": "2026-08-27T12:00:00Z",
          "sync_folder": "/Users/x/DropboxSync",
          "paths": { "Clients/AcmeCorp_NDA_signed.pdf": { "tier": "synced" } }
        }
        """.utf8)

        do {
            _ = try OverlayState.decode(from: reshapedFuture)
            print("  FAIL a v2 payload should be refused even when v1 cannot parse it")
            failures += 1
        } catch {
            let safe = logSafeDescription(of: error)
            checkBool("an unreadable v2 is reported as a version problem",
                      safe.contains("unsupportedVersion"), true)
            // Diagnostic, not independent coverage: nothing can redden this without also
            // reddening the check above it, since that would need a description containing
            // both strings. It earns its line by naming the wrong answer explicitly.
            checkBool("and not as the parse failure that happened first (diagnostic)",
                      safe.contains("typeMismatch"), false)
            // The privacy tripwire that stood here was a duplicate: both paths throw
            // `.unsupportedVersion` and hit the same `logSafeDescription` branch, so no
            // input could redden one without the other. Worth knowing that this holds only
            // while `OverlayStateError` has one case — if the fallback ever throws a
            // different case from the guard, this path needs its own assertion back.
        }

        // NEGATIVE CONTROL. Bytes with no readable version at all must not acquire one.
        // Without this, a fallback that fired unconditionally would look green on every
        // check above it.
        do {
            _ = try OverlayState.decode(from: Data("{ not json at all".utf8))
            print("  FAIL malformed bytes should not decode")
            failures += 1
        } catch {
            checkBool("malformed bytes are still reported as corruption",
                      logSafeDescription(of: error).contains("dataCorrupted"), true)
            // The verb the caller will log. A refusal parsed fine, so calling it "cannot
            // decode" sends the reader after corruption or after DBSYNC-76's sandbox
            // problems; a genuine parse failure IS a decode failure and must keep saying so.
            // Both directions checked, because a `logVerb` stuck on either value passes one
            // of them.
            checkOptional("a real parse failure is still called a decode failure",
                          logVerb(for: error), "cannot decode")
        }

        // SECOND NEGATIVE CONTROL, and it costs nothing: the `leaky` payload above is
        // `version: 1` with a type-mismatched tier — a genuine v1 failure. Its DBSYNC-73
        // check ("but it still says what kind of failure it was") asserts `typeMismatch`,
        // and it is what goes red if the fallback stops checking `version != supported`.
        // Recorded here so the next reader knows that check is doing double duty.

        if failures == 0 {
            print("BadgeDiff: all checks passed")
            exit(0)
        } else {
            print("BadgeDiff: \(failures) check(s) FAILED")
            exit(1)
        }
    }
}
