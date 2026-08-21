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

    /// Relative paths Finder has already been given a badge for.
    ///
    /// Apple's `FinderSync.h` says to "call this method only for items that have already
    /// received a badge" when updating, so a push is only legitimate for a path in here.
    /// Everything else is Finder's to ask about when it displays the item.
    private var badgedPaths: Set<String> = []

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
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        guard let decoded = try? decoder.decode(OverlayState.self, from: data) else {
            state = nil
            lastUpdatedAt = nil
            return
        }

        // Nothing was rewritten since the last tick: no diff, no pushes, no work.
        if decoded.updatedAt == lastUpdatedAt {
            return
        }

        let previousPaths = state?.paths
        state = decoded
        lastUpdatedAt = decoded.updatedAt

        pushBadgeChanges(BadgeDiff.changes(from: previousPaths, to: decoded.paths))
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
        let pushable = BadgeDiff.pushable(changes, alreadyBadged: badgedPaths)
        guard !pushable.isEmpty, let root = syncFolderRoot() else {
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
            badgedPaths.remove(relative)
        }
    }

    private func syncFolderRoot() -> URL? {
        guard let folder = state?.syncFolder, !folder.isEmpty else { return nil }
        return URL(fileURLWithPath: folder, isDirectory: true).standardizedFileURL
    }

    /// Builds the absolute URL for a relative path, refusing anything that escapes `root`.
    ///
    /// The state file is written by the app, but a relative path containing `..` would
    /// otherwise let it badge arbitrary locations on disk, so the containment is enforced
    /// here rather than assumed.
    private func url(forRelative relative: String, under root: URL) -> URL? {
        let url = root.appendingPathComponent(relative).standardizedFileURL
        guard url.path.hasPrefix(root.path) else { return nil }
        return url
    }

    override func requestBadgeIdentifier(for url: URL) {
        guard let root = syncFolderRoot() else {
            return
        }
        let item = url.standardizedFileURL
        guard item.path.hasPrefix(root.path) else {
            return
        }

        var relative = item.path
        relative.removeFirst(root.path.count)
        if relative.hasPrefix("/") {
            relative.removeFirst()
        }

        guard let tier = state?.paths[relative] else {
            // Finder asked about something we do not track. Returning here used to leave
            // whatever badge it already had in place forever (GitHub #104); clear it
            // instead. `""` is the documented removal identifier, per `FinderSync.h`.
            if badgedPaths.contains(relative) {
                FIFinderSyncController.default().setBadgeIdentifier("", for: item)
                badgedPaths.remove(relative)
            }
            return
        }

        FIFinderSyncController.default().setBadgeIdentifier(tier, for: item)
        // Record it: from now on this path is one Finder has displayed, so pushing updates
        // for it is legitimate under Apple's rule.
        badgedPaths.insert(relative)
    }
}

private struct OverlayState: Decodable {
    let version: Int
    let updatedAt: String
    let syncFolder: String?
    /// Relative path (POSIX, no leading slash) → tier id matching registered badge ids.
    let paths: [String: String]
}
