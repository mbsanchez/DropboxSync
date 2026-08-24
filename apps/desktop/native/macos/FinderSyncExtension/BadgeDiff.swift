import Foundation

/// Works out which badges Finder needs to be told about, given two consecutive
/// `overlay_state.json` snapshots (DBSYNC-84).
///
/// Deliberately imports **Foundation only**. It must not reference `FinderSync` or
/// `FIFinderSyncController`: a file that imports FinderSync cannot be compiled as a script
/// by `xcrun swift`, and running it that way is the only automated coverage available here
/// — there is no Swift test target, and adding one is out of scope per DBSYNC-72. Keeping
/// this file pure is what makes `Tests/BadgeDiffTests.swift` possible, not a style choice.
enum BadgeDiff {
    /// What changed between two snapshots of the relative-path → tier map.
    struct Changes: Equatable {
        /// Paths whose badge must be set, with the tier to set it to.
        let changed: [String: String]
        /// Paths that were tracked and no longer are; their badge must be cleared.
        let removed: [String]

        var isEmpty: Bool { changed.isEmpty && removed.isEmpty }
    }

    /// Compares `old` against `new` and returns only what Finder has to be told.
    ///
    /// **A `nil` `old` yields no changes at all**, on purpose. On the first load every path
    /// would otherwise count as changed, and Finder is about to ask for each visible item
    /// anyway — so pushing would be redundant work at exactly the worst moment. DBSYNC-80
    /// measured the enumeration at 50,000 files, so a first-load push storm is a measured
    /// risk rather than a hypothetical one. Encoding it here rather than at the call site
    /// means a future caller cannot forget it.
    static func changes(from old: [String: String]?, to new: [String: String]) -> Changes {
        guard let old else {
            return Changes(changed: [:], removed: [])
        }

        var changed: [String: String] = [:]
        for (path, tier) in new where old[path] != tier {
            changed[path] = tier
        }

        let removed = old.keys.filter { new[$0] == nil }.sorted()

        return Changes(changed: changed, removed: removed)
    }

    /// Narrows `changes` to the items Finder is **displaying**.
    ///
    /// Apple's own guidance, in `FinderSync.h` alongside `setBadgeIdentifier:forURL:`:
    ///
    /// > Avoid adding badges to items that the Finder hasn't displayed yet. When setting the
    /// > initial badge, call this method from your Finder Sync extension's
    /// > `requestBadgeIdentifierForURL:` method. When updating badges, call this method only
    /// > for items that have already received a badge.
    ///
    /// The operative constraint is the first sentence: **displayed**. `displayed` therefore
    /// holds every path Finder has asked about, not only the ones that came back with a
    /// badge.
    ///
    /// Reading "already received a badge" literally is a trap, and this filter fell into it
    /// once: a `.cloudsc` placeholder appears, Finder asks about it before the state file
    /// has caught up, so nothing is badged — and when the tier arrives moments later the
    /// path is filtered out and never badged at all. Finder does not ask twice. That is
    /// symptom 1 of DBSYNC-84, reintroduced by the very filter meant to respect Apple's rule.
    ///
    /// `observedDirectories` closes a second hole in the same idea (DBSYNC-87). Finder only
    /// asks about an item while **enumerating** a directory, so a file that is *created*
    /// while its folder is already open is never asked about and never enters `displayed`.
    /// Hydration does exactly that: it downloads `foo.txt` and deletes `foo.txt.cloudsc`, so
    /// the tier arrives against a path Finder has never named. Every push for it was dropped
    /// here, and only re-entering the directory — which forces a fresh enumeration — showed
    /// the badge.
    ///
    /// An item in a directory Finder is currently observing **is** displayed, which is the
    /// condition Apple's first sentence actually states. Widening to "asked about, or sitting
    /// in a folder on screen" is therefore closer to the documented rule than the narrower
    /// test it replaces, not a relaxation of it.
    static func pushable(
        _ changes: Changes,
        displayed: Set<String>,
        observedDirectories: Set<String>
    ) -> Changes {
        func isPushable(_ path: String) -> Bool {
            displayed.contains(path) || observedDirectories.contains(parentDirectory(of: path))
        }
        return Changes(
            changed: changes.changed.filter { isPushable($0.key) },
            removed: changes.removed.filter(isPushable)
        )
    }

    /// The directory part of a relative path: `"sub/a.txt"` → `"sub"`, `"a.txt"` → `""`.
    ///
    /// `""` is the sync folder itself, which is a real answer rather than a missing one —
    /// files sitting directly in the sync root are the common case, and their parent has to
    /// compare equal to the relative path of the root that [`relativePath`] produces.
    ///
    /// Only the LAST separator is cut, so a file in a subfolder of an observed directory does
    /// not match that directory. `"sub/deep/a.txt"` yields `"sub/deep"`, not `"sub"` — and it
    /// should, because Finder showing `sub` does not put the contents of `sub/deep` on screen.
    static func parentDirectory(of relative: String) -> String {
        guard let separator = relative.lastIndex(of: "/") else { return "" }
        return String(relative[relative.startIndex..<separator])
    }

    /// Whether `path` is `root` itself or lives inside it.
    ///
    /// A plain `path.hasPrefix(root)` is **not** a containment test and was shipped as one
    /// in the first version of this change: with a root of `/Users/x/DropboxSync`, the path
    /// `/Users/x/DropboxSyncEvil/secret.txt` passes a string-prefix check while living in a
    /// different directory entirely. Comparing against `root + "/"` is what makes the
    /// sibling-with-a-longer-name case fail as it should.
    ///
    /// This lives in `BadgeDiff` rather than next to its callers so that it is reachable
    /// from `Tests/BadgeDiffTests.swift`. That is the point: the string-prefix bug survived
    /// review precisely because it sat in a file the test script cannot compile.
    static func isContained(_ path: String, in root: String) -> Bool {
        if path == root { return true }
        let rootWithSeparator = root.hasSuffix("/") ? root : root + "/"
        return path.hasPrefix(rootWithSeparator)
    }

    /// The portion of `path` below `root`, or `nil` if `path` is not inside `root`.
    ///
    /// Returns `""` for `root` itself. The old inline version sliced off `root.count`
    /// characters after a bare prefix check, so `/Users/x/DropboxSyncEvil/f` yielded the
    /// nonsensical relative path `Evil/f` and was then looked up in the state map.
    static func relativePath(of path: String, under root: String) -> String? {
        guard isContained(path, in: root) else { return nil }
        if path == root { return "" }
        let rootWithSeparator = root.hasSuffix("/") ? root : root + "/"
        return String(path.dropFirst(rootWithSeparator.count))
    }

    /// What to push when the previous snapshot was lost but Finder is still showing badges.
    ///
    /// `reloadState()` drops its snapshot whenever the state file cannot be read. Without
    /// this, the next successful read produces no diff — `changes(from: nil, …)` is empty by
    /// design — and every visible badge stays frozen until the user leaves the directory and
    /// comes back. Which is the exact bug this ticket exists to fix, reached through a
    /// narrower door.
    ///
    /// `displayed` is not lost in that reset, so those items can be refreshed directly.
    /// Every path here is one Finder is showing, so pushing to it is legitimate.
    static func resync(to new: [String: String], displayed: Set<String>) -> Changes {
        var changed: [String: String] = [:]
        for path in displayed {
            if let tier = new[path] { changed[path] = tier }
        }
        return Changes(
            changed: changed,
            removed: displayed.filter { new[$0] == nil }.sorted()
        )
    }

    /// What to do when Finder asks about `path`.
    ///
    /// This exists because the placement of a single line was load-bearing and untestable.
    /// Twice now a defect has lived in `SyncExtension.swift` where the check script cannot
    /// reach it: the string-prefix containment bug, and then recording a path as displayed
    /// only *after* a tier was found. The second one passed 27 green checks and a clean
    /// build. Moving the decision here is what makes it a fact under test rather than a
    /// property of where a statement happens to sit.
    struct RequestOutcome: Equatable {
        /// The relative path Finder is now displaying, or `nil` if it is outside the root.
        ///
        /// **Non-nil even when there is no tier to set.** Finder asking about an item means
        /// it is on screen, and it will not ask twice — so a `.cloudsc` drawn before the
        /// state file catches up must still count as displayed, or the tier arriving a
        /// moment later is filtered out of the push and the file stays bare forever.
        let displayedPath: String?
        /// The identifier to hand to `setBadgeIdentifier`: a tier, or `""` to clear.
        /// `nil` means do nothing at all.
        let badgeIdentifier: String?
    }

    static func requestOutcome(
        for path: String,
        root: String,
        paths: [String: String]?
    ) -> RequestOutcome {
        guard let relative = relativePath(of: path, under: root), !relative.isEmpty else {
            return RequestOutcome(displayedPath: nil, badgeIdentifier: nil)
        }
        // `""` clears the badge — documented in `FinderSync.h`. An item we do not track
        // must not keep a badge from a previous state.
        return RequestOutcome(displayedPath: relative, badgeIdentifier: paths?[relative] ?? "")
    }
}

