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
}
