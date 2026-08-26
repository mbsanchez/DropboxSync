import Cocoa
import FinderSync

/// Reads `overlay_state.json` written by the Tauri app (see `src-tauri/src/overlay_state.rs`)
/// and applies Finder badge images for each tracked file under the sync folder.
///
/// Add this file to a **Finder Sync Extension** target in Xcode, and add the three PNGs
/// from `Assets/` to the extension target (Copy Bundle Resources).
@objc(DropboxSyncFinderSync)
final class DropboxSyncFinderSync: FIFinderSync {
    private var state: OverlayState?
    private var reloadTimer: Timer?

    /// Relative paths Finder is **displaying** — every path it has asked about, whether or
    /// not we had a tier to give it at the time.
    ///
    /// Apple's `FinderSync.h` warns against badging "items that the Finder hasn't displayed
    /// yet", so a push is only legitimate for a path in here. It deliberately holds
    /// asked-about paths rather than badged ones: a `.cloudsc` placeholder is drawn before
    /// the state file catches up, so it gets no badge on the first ask, and if it were left
    /// out of this set the tier arriving moments later would be filtered away and the file
    /// would never be badged at all. Finder does not ask twice.
    ///
    /// **This set only ever grows, and that is deliberate.** Clearing a badge does not mean
    /// Finder stopped showing the item — the file is still on screen with no status to
    /// display. Dropping a path here when its badge is cleared means a file that leaves the
    /// state map and comes back is filtered out of the push and never badged again.
    ///
    /// The cost of never shrinking is one string per distinct path Finder has shown inside
    /// the sync folder — a few MB at the 50,000 files DBSYNC-80 measured. Accepted.
    ///
    /// It is not the whole story on its own: Finder only asks while enumerating, so a file
    /// created in an already-open folder never appears here. [`observedDirectories`] covers
    /// that case (DBSYNC-87).
    private var displayedPaths: Set<String> = []

    /// Absolute paths of the directories Finder is showing right now (DBSYNC-87).
    ///
    /// Unlike [`displayedPaths`] this one shrinks, because `endObservingDirectory(at:)` is a
    /// genuine "no longer on screen" signal — the only one this API provides.
    ///
    /// **Absolute rather than relative on purpose.** These callbacks can fire before the
    /// first `overlay_state.json` read lands, and at that moment there is no sync root to
    /// make a path relative to. Storing what Finder actually hands us keeps the record
    /// correct regardless of when state arrives; the conversion to relative paths happens at
    /// push time, when the root is known or the push is skipped anyway.
    private var observedDirectories: Set<String> = []

    /// `updated_at` of the snapshot currently in `state`. The Rust writer stamps every
    /// snapshot (`overlay_state.rs`), so an unchanged value means an unchanged file and the
    /// whole diff can be skipped — which matters because this runs every 2 seconds forever.
    private var lastUpdatedAt: String?

    override init() {
        super.init()
        registerBadgeImages()
        reloadState()
        scheduleReload()

        FIFinderSyncController.default().directoryURLs = monitoredDirectoryURLs()
    }

    private func scheduleReload() {
        reloadTimer?.invalidate()
        reloadTimer = Timer.scheduledTimer(withTimeInterval: 2.0, repeats: true) { [weak self] _ in
            self?.reloadState()
            FIFinderSyncController.default().directoryURLs = self?.monitoredDirectoryURLs() ?? []
        }
    }

    /// The real home directory, bypassing the sandbox container.
    ///
    /// This extension IS sandboxed — Finder refuses to register it otherwise (DBSYNC-76) — and
    /// inside a sandbox every Foundation home API (`homeDirectoryForCurrentUser`, `NSHomeDirectory()`,
    /// `.userDomainMask` searches) is redirected to `~/Library/Containers/<bundle-id>/Data/`. That
    /// container's `Library/Application Support` holds only Apple's own symlinks (AddressBook,
    /// SyncServices, iCloud), so resolving through it silently yields a path that never exists.
    /// `getpwuid` reads the passwd database directly and is not redirected.
    private func realHomeDirectory() -> URL {
        if let pw = getpwuid(getuid()), let dir = pw.pointee.pw_dir {
            return URL(fileURLWithPath: String(cString: dir), isDirectory: true)
        }
        return FileManager.default.homeDirectoryForCurrentUser
    }

    /// Resolves the same path the Rust side writes in `db::app_data_dir()`:
    /// `~/Library/Application Support/DropboxSyncDesktop/overlay_state.json`.
    ///
    /// Reading it from the sandbox is what
    /// `com.apple.security.temporary-exception.files.home-relative-path.read-only` grants in
    /// DropboxSyncFinderSync.entitlements — and that entitlement is relative to the REAL home,
    /// which is why the path must be built from [`realHomeDirectory`].
    private func overlayStateURL() -> URL {
        realHomeDirectory()
            .appendingPathComponent("Library/Application Support/DropboxSyncDesktop/overlay_state.json")
    }

    private func monitoredDirectoryURLs() -> Set<URL> {
        guard let folder = state?.syncFolder, !folder.isEmpty else {
            return []
        }
        return [URL(fileURLWithPath: folder, isDirectory: true)]
    }

    private func registerBadgeImages() {
        let c = FIFinderSyncController.default()
        // `label` is the accessibility description VoiceOver reads for the badge; an empty
        // string leaves the status unannounced.
        let badges: [(id: String, resource: String, label: String)] = [
            ("synced", "cloud-check", "Synced"),
            ("out_of_sync", "cloud-alert", "Out of sync"),
            ("syncing", "cloud-sync", "Syncing"),
            // DBSYNC-80. Grey rather than a status colour: "online only" is not a
            // problem to flag, it is a fact about where the bytes live, and it should not
            // compete with the green/blue/red the other three carry. A download arrow
            // rather than a cloud: Finder draws these at 16pt in list view, where a cloud
            // silhouette collapses into an unreadable blob — and the arrow matches what
            // iCloud Drive shows for a file that is not downloaded.
            ("cloud_only", "cloud-only", "Online only"),
        ]
        for badge in badges {
            // `image(forResource:)`, not `NSImage(contentsOfFile:)`: only the former pairs
            // cloud-check.png with cloud-check@2x.png. Loading the 1x file by path yields a
            // single 24px representation that Finder upscales on Retina.
            guard let image = Bundle.main.image(forResource: badge.resource) else {
                NSLog("DropboxSync FinderSync: %@.png missing from the bundle — no '%@' badge.",
                      badge.resource, badge.id)
                continue
            }
            c.setBadgeImage(image, label: badge.label, forBadgeIdentifier: badge.id)
        }
    }

    /// Logged transitions only: `reloadState()` runs every 2s, so an unconditional NSLog would
    /// spam the system log. Silence here is what made the sandbox-container bug (DBSYNC-76) so
    /// expensive to find — an unreadable state file looks exactly like "no files tracked yet".
    private var lastLoadFailed = false

    /// Same transition-logging discipline as [`lastLoadFailed`], for the decode step
    /// (DBSYNC-73). Separate flag: the file can be readable and undecodable, and conflating
    /// them would swallow one of the two messages.
    private var lastDecodeFailed = false

    private func reloadState() {
        let url = overlayStateURL()
        guard let data = try? Data(contentsOf: url) else {
            if !lastLoadFailed {
                NSLog("DropboxSync FinderSync: cannot read %@ — no badges will be shown. "
                    + "Check the sandbox entitlement covers this path.", url.path)
                lastLoadFailed = true
            }
            state = nil
            // Must clear alongside `state`: leaving a stale stamp here would make the
            // short-circuit below skip the diff when the file comes back with the same
            // `updated_at`, and no badge would ever be pushed again.
            lastUpdatedAt = nil
            return
        }
        if lastLoadFailed {
            NSLog("DropboxSync FinderSync: reading %@ again.", url.path)
            lastLoadFailed = false
        }
        // Decoding lives in `OverlayState`, declared in the Foundation-only BadgeDiff.swift
        // so the check script can reach it, and it throws so this failure can be reported
        // (DBSYNC-73).
        let decoded: OverlayState
        do {
            decoded = try OverlayState.decode(from: data)
        } catch {
            // NOT silent. This used to be a bare `try?` with no log, while the read above
            // it logged carefully — and the consequence is worse than the read failing:
            // `state = nil` drops `syncFolder`, so `monitoredDirectoryURLs()` returns [],
            // so Finder deregisters the extension's directories and stops calling it. Every
            // badge vanishes with nothing written anywhere. That is the shape of DBSYNC-76,
            // which this file's own comments record as expensive to find.
            //
            // Logged on the transition only, like the read path: this runs every 2 seconds.
            if !lastDecodeFailed {
                NSLog("DropboxSync FinderSync: cannot decode %@ — no badges will be shown. %@",
                      url.path, logSafeDescription(of: error))
                lastDecodeFailed = true
            }
            state = nil
            lastUpdatedAt = nil
            return
        }
        if lastDecodeFailed {
            NSLog("DropboxSync FinderSync: decoding %@ again.", url.path)
            lastDecodeFailed = false
        }

        // Nothing was rewritten since the last tick: no diff, no pushes, no work.
        if decoded.updatedAt == lastUpdatedAt {
            return
        }

        let previousPaths = state?.paths
        state = decoded
        lastUpdatedAt = decoded.updatedAt

        if previousPaths == nil && !displayedPaths.isEmpty {
            // The snapshot was lost — the state file was unreadable for a moment — but
            // Finder is still showing badges we know about. `changes(from: nil, …)` is
            // empty by design, so without this every visible badge would stay frozen until
            // the user left the directory and came back: this ticket's bug, narrower door.
            pushBadgeChanges(BadgeDiff.resync(to: decoded.paths, displayed: displayedPaths))
        } else {
            pushBadgeChanges(BadgeDiff.changes(from: previousPaths, to: decoded.paths))
        }
    }

    /// Tells Finder about badges that changed since the previous snapshot (DBSYNC-84).
    ///
    /// Finder calls `requestBadgeIdentifier(for:)` **once** per item and then caches the
    /// answer, so without this the badge a file was first drawn with is the badge it keeps
    /// until the user leaves the directory and comes back. `setBadgeIdentifier` outside a
    /// request is the documented way to push an update — `FinderSync.h` says so directly:
    /// "When updating badges, call this method only for items that have already received a
    /// badge." Hence the `pushable` filter rather than pushing everything that changed.
    private func pushBadgeChanges(_ changes: BadgeDiff.Changes) {
        guard let root = syncFolderRoot() else {
            return
        }
        // Converted here rather than stored relative: see `observedDirectories`. Anything
        // outside the root drops out, which is also the containment guard for this input.
        let observedRelatives = Set(
            observedDirectories.compactMap { BadgeDiff.relativePath(of: $0, under: root.path) }
        )
        let pushable = BadgeDiff.pushable(
            changes, displayed: displayedPaths, observedDirectories: observedRelatives
        )
        guard !pushable.isEmpty else {
            return
        }
        let controller = FIFinderSyncController.default()

        for (relative, tier) in pushable.changed {
            guard let url = url(forRelative: relative, under: root) else { continue }
            controller.setBadgeIdentifier(tier, for: url)
        }

        // Clearing a badge is `setBadgeIdentifier("")` — documented in `FinderSync.h`:
        // "Setting the identifier to an empty string (@\"\") removes the badge."
        // Checked against the SDK header rather than taken from folklore (GitHub #104).
        for relative in pushable.removed {
            guard let url = url(forRelative: relative, under: root) else { continue }
            controller.setBadgeIdentifier("", for: url)
        }
    }

    private func syncFolderRoot() -> URL? {
        guard let folder = state?.syncFolder, !folder.isEmpty else { return nil }
        return URL(fileURLWithPath: folder, isDirectory: true).standardizedFileURL
    }

    /// Builds the absolute URL for a relative path, refusing anything that escapes `root`.
    ///
    /// The state file is written by the app, but a relative path containing `..` would
    /// otherwise let it badge arbitrary locations on disk. The containment test lives in
    /// [`BadgeDiff.isContained`] so that it is covered by `Tests/BadgeDiffTests.swift`: the
    /// first version of this check used a bare `hasPrefix` and let a sibling directory
    /// through, and it survived review because it sat where no test could reach it.
    private func url(forRelative relative: String, under root: URL) -> URL? {
        let url = root.appendingPathComponent(relative).standardizedFileURL
        guard BadgeDiff.isContained(url.path, in: root.path) else { return nil }
        return url
    }

    /// Finder started showing a directory inside the sync folder (DBSYNC-87).
    ///
    /// This is what makes a badge reach a file that did not exist when the folder was opened.
    /// Hydration creates such a file every time — `foo.txt` appears while `foo.txt.cloudsc`
    /// disappears — and Finder never asks about it, because it only asks while enumerating.
    override func beginObservingDirectory(at url: URL) {
        observedDirectories.insert(url.standardizedFileURL.path)
    }

    /// Finder stopped showing a directory. Pushing into it from here on would be badging
    /// items that are no longer on screen, which is the thing `FinderSync.h` warns against.
    override func endObservingDirectory(at url: URL) {
        observedDirectories.remove(url.standardizedFileURL.path)
    }

    override func requestBadgeIdentifier(for url: URL) {
        guard let root = syncFolderRoot() else {
            return
        }
        let item = url.standardizedFileURL
        let outcome = BadgeDiff.requestOutcome(
            for: item.path, root: root.path, paths: state?.paths
        )
        if let displayed = outcome.displayedPath {
            displayedPaths.insert(displayed)
        }
        if let identifier = outcome.badgeIdentifier {
            FIFinderSyncController.default().setBadgeIdentifier(identifier, for: item)
        }
    }
}
