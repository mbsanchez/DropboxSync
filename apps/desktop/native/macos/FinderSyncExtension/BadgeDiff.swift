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

    /// Narrows `changes` to the items Finder has **already badged**.
    ///
    /// Apple's own guidance, in `FinderSync.h` alongside `setBadgeIdentifier:forURL:`:
    ///
    /// > Avoid adding badges to items that the Finder hasn't displayed yet. When setting the
    /// > initial badge, call this method from your Finder Sync extension's
    /// > `requestBadgeIdentifierForURL:` method. When updating badges, call this method only
    /// > for items that have already received a badge.
    ///
    /// So a push is only legitimate for a path Finder previously asked about. Anything else
    /// is handled by `requestBadgeIdentifier(for:)` when Finder gets round to displaying it.
    /// Without this filter a file that changes tier while its folder is closed would be
    /// pushed at Finder for an item it has never drawn.
    static func pushable(_ changes: Changes, alreadyBadged: Set<String>) -> Changes {
        Changes(
            changed: changes.changed.filter { alreadyBadged.contains($0.key) },
            removed: changes.removed.filter { alreadyBadged.contains($0) }
        )
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
    /// `alreadyBadged` is not lost in that reset, so the displayed items can be refreshed
    /// directly. Every path here has already received a badge, so pushing to it is
    /// legitimate under Apple's rule.
    static func resync(to new: [String: String], alreadyBadged: Set<String>) -> Changes {
        var changed: [String: String] = [:]
        for path in alreadyBadged {
            if let tier = new[path] { changed[path] = tier }
        }
        return Changes(
            changed: changed,
            removed: alreadyBadged.filter { new[$0] == nil }.sorted()
        )
    }
}

