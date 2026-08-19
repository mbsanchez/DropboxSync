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

    /// Resolves the same path the Rust side writes in `db::app_data_dir()`:
    /// `~/Library/Application Support/DropboxSyncDesktop/overlay_state.json`.
    /// The extension is not sandboxed, so `.userDomainMask` yields the plain
    /// per-user Application Support directory rather than a container path.
    private func overlayStateURL() -> URL {
        let appSupport = FileManager.default
            .urls(for: .applicationSupportDirectory, in: .userDomainMask)
            .first
            ?? FileManager.default.homeDirectoryForCurrentUser
                .appendingPathComponent("Library/Application Support")
        return appSupport
            .appendingPathComponent("DropboxSyncDesktop/overlay_state.json")
    }

    private func monitoredDirectoryURLs() -> Set<URL> {
        guard let folder = state?.syncFolder, !folder.isEmpty else {
            return []
        }
        return [URL(fileURLWithPath: folder, isDirectory: true)]
    }

    private func registerBadgeImages() {
        let c = FIFinderSyncController.default()
        let triples: [(String, String)] = [
            ("synced", "cloud-check"),
            ("out_of_sync", "cloud-alert"),
            ("syncing", "cloud-sync"),
        ]
        for (id, base) in triples {
            guard let path = Bundle.main.path(forResource: base, ofType: "png") else {
                NSLog("DropboxSync FinderSync: missing \(base).png in bundle")
                continue
            }
            let image = NSImage(contentsOfFile: path)!
            c.setBadgeImage(image, label: "", forBadgeIdentifier: id)
        }
    }

    private func reloadState() {
        let url = overlayStateURL()
        guard let data = try? Data(contentsOf: url) else {
            state = nil
            return
        }
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        state = try? decoder.decode(OverlayState.self, from: data)
    }

    override func requestBadgeIdentifier(for url: URL) {
        guard let folder = state?.syncFolder, !folder.isEmpty else {
            return
        }
        let root = URL(fileURLWithPath: folder, isDirectory: true).standardizedFileURL
        let item = url.standardizedFileURL
        guard item.path.hasPrefix(root.path) else {
            return
        }

        let rootPath = root.path
        var relative = item.path
        if relative.hasPrefix(rootPath) {
            relative.removeFirst(rootPath.count)
            if relative.hasPrefix("/") {
                relative.removeFirst()
            }
        }

        guard let tier = state?.paths[relative] else {
            return
        }
        FIFinderSyncController.default().setBadgeIdentifier(tier, for: item)
    }
}

private struct OverlayState: Decodable {
    let version: Int
    let updatedAt: String
    let syncFolder: String?
    /// Relative path (POSIX, no leading slash) → tier id matching registered badge ids.
    let paths: [String: String]
}
