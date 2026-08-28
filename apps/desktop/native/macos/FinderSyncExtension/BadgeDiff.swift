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

/// The shape of `overlay_state.json`, and the only place it is decoded (DBSYNC-73).
///
/// **It lives here rather than in `SyncExtension.swift` for one reason: this file is
/// Foundation-only, so `Tests/run-badge-diff-tests.sh` can compile it with `swiftc` and CI
/// already runs that script.** The original ticket recorded this decode as impossible to
/// test; it was not, and two defects have already survived review in this project by
/// sitting where the check script could not reach them.
struct OverlayState: Decodable {
    let version: Int
    let updatedAt: String
    let syncFolder: String?
    /// Relative path (POSIX, no leading slash) → tier id matching registered badge ids.
    let paths: [String: String]

    /// Explicit, and never `keyDecodingStrategy = .convertFromSnakeCase`.
    ///
    /// That strategy applies to **every** key the decoder sees, and on the Foundation
    /// shipped with macOS 12-14 that included the keys of a `[String: T]` dictionary.
    /// `paths` is exactly such a dictionary and its keys are file paths, so
    /// `docs/my_report.pdf` decoded as `docs/myReport.pdf`, matched nothing in
    /// `requestBadgeIdentifier(for:)`, and that file silently rendered no badge.
    ///
    /// It does not reproduce on macOS 26 — the swift-foundation rewrite stopped converting
    /// dictionary keys — but `JSONDecoder` comes from the USER's Foundation, and
    /// `minimumSystemVersion` is 12.0. Correct for the developer, silently wrong for a
    /// subset of users on a subset of their files.
    ///
    /// **These keys do more than avoid the strategy: they make it unreachable.** With the
    /// wire names spelled out, `.convertFromSnakeCase` rewrites `updated_at` to
    /// `updatedAt`, then looks for a `CodingKey` whose `stringValue` is `"updated_at"`,
    /// and throws `keyNotFound` — on every Foundation and every OS. Reintroducing it fails
    /// loudly here and in CI instead of quietly on someone else's machine.
    enum CodingKeys: String, CodingKey {
        case version
        case updatedAt = "updated_at"
        case syncFolder = "sync_folder"
        case paths
    }

    /// The only contract version this reader understands (DBSYNC-91).
    ///
    /// Named rather than written inline so that it is greppable from the writer's end:
    /// `overlay_state.rs` documents the bump rule, and this is the constant that rule is
    /// actually talking about.
    static let supportedVersion = 1

    /// Decodes with a plain `JSONDecoder`. Throws rather than returning `nil` so the caller
    /// can log what went wrong — a silent decode failure deregisters the extension's
    /// directories and every badge disappears with nothing written anywhere.
    ///
    /// **Refuses any version other than [`supportedVersion`] (DBSYNC-91.)** Before this,
    /// `version` was decoded and never read: a file written with a bumped version and an
    /// incompatible schema was applied exactly as if it were v1, using whatever fields
    /// still lined up and ignoring the rest. That made the field documentation rather than
    /// a mechanism.
    ///
    /// Refusing rather than best-effort follows this project's own rule, settled in
    /// DBSYNC-84: a badge that is confidently wrong is worse than no badge. A user on a
    /// mismatched pair sees badges missing — which they report — instead of badges that
    /// state a sync status nobody checked.
    ///
    /// It is not a hypothetical pairing. DBSYNC-86 measured that every reinstall registers
    /// a NEW plug-in instance and the old ones linger, so an old appex can genuinely be the
    /// one Finder has loaded while a newer app writes the file.
    static func decode(from data: Data) throws -> OverlayState {
        let decoded: OverlayState
        do {
            decoded = try JSONDecoder().decode(OverlayState.self, from: data)
        } catch {
            // The version check below is unreachable when the decode itself fails, and the
            // most plausible breaking change is exactly that case: reshape `paths` and a v1
            // decoder throws `typeMismatch` long before it has a version to look at. A
            // guard that only fires when the rest of the document still parses is the same
            // half-mechanism this ticket exists to remove, moved one step later.
            //
            // So: if the bytes are still readable enough to yield a version, and that
            // version is not ours, say so. Otherwise rethrow untouched.
            //
            // `version != supportedVersion` is not a formality. Without it, every ordinary
            // v1 failure — the type-mismatched tier from DBSYNC-73 included — would be
            // relabelled a version problem, which is a worse lie than the silence this
            // ticket started from.
            if let header = try? JSONDecoder().decode(VersionHeader.self, from: data),
               header.version != supportedVersion {
                throw OverlayStateError.unsupportedVersion(header.version)
            }
            throw error
        }
        guard decoded.version == supportedVersion else {
            throw OverlayStateError.unsupportedVersion(decoded.version)
        }
        return decoded
    }
}

/// Just enough of `overlay_state.json` to read its version when the rest will not parse
/// (DBSYNC-91).
///
/// **Only used from the failure path.** Decoding this first, then the full struct, would
/// read better — but `JSONDecoder` parses the whole document to extract one field, so
/// header-first means two full parses of a file that can hold thousands of path entries, on
/// every tick of `reloadState()`'s 2-second timer, in the case where nothing is wrong.
/// Retrying inside the `catch` keeps the healthy path at one parse and pays the second only
/// when something has already failed.
///
/// Private so that nothing outside the decoder starts using it as a cheap way to peek at the
/// file: it is not cheap, and it answers only one question.
private struct VersionHeader: Decodable {
    /// No `CodingKeys` needed — `version` is the one key in this contract that was never
    /// snake-cased. See `OverlayState.CodingKeys` for why that matters here.
    let version: Int
}

/// A refusal to use `overlay_state.json`, distinct from a failure to parse it (DBSYNC-91).
///
/// Carries the version found because the log line is required to name it: "no badges" with
/// no reason reads to a user like the sandbox or permissions problems this extension has
/// already had (DBSYNC-76), and those are debugged in a completely different direction.
enum OverlayStateError: Error {
    case unsupportedVersion(Int)
}

/// A description of a decoding failure that is safe to put in the system log (DBSYNC-73).
///
/// **`String(describing:)` on a `DecodingError` leaks the user's file paths.** The error
/// carries a `codingPath`, and inside `OverlayState.paths` those components ARE relative
/// paths from the sync folder:
///
///     typeMismatch … Path: paths.`Clients/AcmeCorp_NDA_signed.pdf`
///
/// `NSLog` writes to the public unified log, readable via `log show` and captured in any
/// sysdiagnose. Making the decode failure audible — which is the other half of this ticket
/// — must not turn it into a privacy leak.
///
/// So this reports the *kind* of failure and, at most, a key name from the struct's own
/// schema. It never emits `codingPath`, and never a value. What is lost is which entry
/// failed; what is kept is enough to tell a schema mismatch from malformed bytes, which is
/// the distinction anyone debugging actually needs.
///
/// Not reachable from today's writer — `HashMap<String, OverlayTier>` can only emit strings,
/// and truncation fails earlier as `dataCorrupted`. It is guarded anyway because
/// `overlay_state.json` is a versioned contract with two readers, and a newer writer
/// reshaping `paths` against an installed older appex is precisely the gap that
/// [`OverlayState.supportedVersion`] now closes.
func logSafeDescription(of error: Error) -> String {
    // Handled BEFORE the `DecodingError` cast below, which would otherwise fall through to
    // the type-name branch and report a bare "OverlayStateError" — losing the one detail
    // this error exists to carry (DBSYNC-91). A schema version is an integer from our own
    // contract, so unlike a `codingPath` there is nothing here to redact.
    if case .unsupportedVersion(let found)? = error as? OverlayStateError {
        return "unsupportedVersion(\(found))"
    }
    guard let decoding = error as? DecodingError else {
        // Not a decoding failure — a read error, already scoped to a path we logged
        // ourselves. Still summarised rather than dumped.
        return "\(type(of: error))"
    }
    switch decoding {
    case .keyNotFound(let key, _):
        // A key name from OUR schema (`updated_at`, `sync_folder`), never a dictionary key
        // from `paths`: a missing-key failure is raised against the struct's keyed
        // container. This is also what both DBSYNC-73 mutations produce, so it is the one
        // detail worth keeping.
        return "keyNotFound(\(key.stringValue))"
    case .typeMismatch(let expected, _):
        return "typeMismatch(expected \(expected))"
    case .valueNotFound(let expected, _):
        return "valueNotFound(expected \(expected))"
    case .dataCorrupted:
        // Malformed or truncated JSON. The context here describes the syntax problem, not
        // the content, but it is dropped anyway — "the bytes are not JSON" is the whole
        // actionable message.
        return "dataCorrupted (not valid JSON)"
    @unknown default:
        return "DecodingError (unrecognised case)"
    }
}
