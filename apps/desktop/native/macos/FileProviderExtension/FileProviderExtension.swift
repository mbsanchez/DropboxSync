import FileProvider
import Foundation
import os
import UniformTypeIdentifiers

/// The tracer File Provider extension (DBSYNC-95 slice 1, ADR-0004).
///
/// It proves the extension loads, the domain mounts and the enumerator is called. It serves ONE static
/// folder item; nothing is read from the host, and every write or download answers with an error. Later
/// slices replace the static item with a read-only projection of the host's index.
///
/// `@objc(DropboxSyncFileProvider)` makes the runtime class name the bare literal that Info.plist names in
/// `NSExtensionPrincipalClass`. `build-appex.sh` checks the two agree.
@objc(DropboxSyncFileProvider)
final class DropboxSyncFileProvider: NSObject, NSFileProviderReplicatedExtension {
    private let domain: NSFileProviderDomain

    required init(domain: NSFileProviderDomain) {
        self.domain = domain
        super.init()
        fpLog.info("init domain=\(domain.identifier.rawValue, privacy: .public)")
    }

    func invalidate() {
        fpLog.info("invalidate")
    }

    func item(
        for identifier: NSFileProviderItemIdentifier,
        request: NSFileProviderRequest,
        completionHandler: @escaping (NSFileProviderItem?, Error?) -> Void
    ) -> Progress {
        fpLog.info("item(for:) \(identifier.rawValue, privacy: .public)")
        switch identifier {
        case .rootContainer:
            completionHandler(TracerItem.root, nil)
        case TracerItem.folder.itemIdentifier:
            completionHandler(TracerItem.folder, nil)
        default:
            completionHandler(nil, NSFileProviderError(.noSuchItem))
        }
        return Progress(totalUnitCount: 1)
    }

    func enumerator(
        for containerItemIdentifier: NSFileProviderItemIdentifier,
        request: NSFileProviderRequest
    ) throws -> NSFileProviderEnumerator {
        fpLog.info("enumerator(for:) \(containerItemIdentifier.rawValue, privacy: .public)")
        switch containerItemIdentifier {
        case .trashContainer:
            // The header's instruction for an extension that does not support trashing items.
            throw CocoaError(.featureUnsupported)
        case .rootContainer, .workingSet, TracerItem.folder.itemIdentifier:
            break
        default:
            throw NSFileProviderError(.noSuchItem)
        }
        return TracerEnumerator(container: containerItemIdentifier)
    }

    func fetchContents(
        for itemIdentifier: NSFileProviderItemIdentifier,
        version requestedVersion: NSFileProviderItemVersion?,
        request: NSFileProviderRequest,
        completionHandler: @escaping (URL?, NSFileProviderItem?, Error?) -> Void
    ) -> Progress {
        fpLog.info("fetchContents: no such item")
        // The tracer serves folders only, so no item has contents to fetch. `noSuchItem` is one of the codes
        // the header lists for this method (with NSFileReadNoPermissionError, notAuthenticated and
        // serverUnreachable); `cannotSynchronize` is not. Slice 3 keeps `noSuchItem` for an unknown identifier.
        completionHandler(nil, nil, NSFileProviderError(.noSuchItem))
        return Progress(totalUnitCount: 1)
    }

    func createItem(
        basedOn itemTemplate: NSFileProviderItem,
        fields: NSFileProviderItemFields,
        contents url: URL?,
        options: NSFileProviderCreateItemOptions = [],
        request: NSFileProviderRequest,
        completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void
    ) -> Progress {
        fpLog.info("createItem refused")
        completionHandler(nil, [], false, NSFileProviderError(.cannotSynchronize))
        return Progress(totalUnitCount: 1)
    }

    func modifyItem(
        _ item: NSFileProviderItem,
        baseVersion version: NSFileProviderItemVersion,
        changedFields: NSFileProviderItemFields,
        contents newContents: URL?,
        options: NSFileProviderModifyItemOptions = [],
        request: NSFileProviderRequest,
        completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void
    ) -> Progress {
        fpLog.info("modifyItem refused")
        completionHandler(nil, [], false, NSFileProviderError(.cannotSynchronize))
        return Progress(totalUnitCount: 1)
    }

    func deleteItem(
        identifier: NSFileProviderItemIdentifier,
        baseVersion version: NSFileProviderItemVersion,
        options: NSFileProviderDeleteItemOptions = [],
        request: NSFileProviderRequest,
        completionHandler: @escaping (Error?) -> Void
    ) -> Progress {
        fpLog.info("deleteItem refused")
        completionHandler(NSFileProviderError(.cannotSynchronize))
        return Progress(totalUnitCount: 1)
    }
}

/// Shared logger. Only static, non-user strings are logged public; identifiers here are our own constants.
let fpLog = Logger(subsystem: "com.mobsanchez.dropboxsyncdesktop.fileprovider", category: "extension")

/// The tracer's items: the domain root and one static folder under it.
final class TracerItem: NSObject, NSFileProviderItem {
    static let root = TracerItem(id: .rootContainer, parent: .rootContainer, name: "Dropbox Sync", isRoot: true)
    static let folder = TracerItem(
        id: NSFileProviderItemIdentifier("tracer-item"), parent: .rootContainer, name: "File Provider tracer",
        isRoot: false)

    private let id: NSFileProviderItemIdentifier
    private let parent: NSFileProviderItemIdentifier
    private let name: String
    private let isRoot: Bool

    private init(id: NSFileProviderItemIdentifier, parent: NSFileProviderItemIdentifier, name: String, isRoot: Bool) {
        self.id = id
        self.parent = parent
        self.name = name
        self.isRoot = isRoot
    }

    var itemIdentifier: NSFileProviderItemIdentifier { id }
    var parentItemIdentifier: NSFileProviderItemIdentifier { parent }
    var filename: String { name }
    var contentType: UTType { .folder }
    var capabilities: NSFileProviderItemCapabilities { [.allowsReading, .allowsContentEnumerating] }
    var itemVersion: NSFileProviderItemVersion {
        NSFileProviderItemVersion(contentVersion: Data("1".utf8), metadataVersion: Data("1".utf8))
    }
}

/// Enumerates the static item under the root; every other container, and the working set, is empty.
final class TracerEnumerator: NSObject, NSFileProviderEnumerator {
    /// The fixed anchor the tracer reports: nothing ever changes.
    private static let anchor = NSFileProviderSyncAnchor(Data("1".utf8))

    private let container: NSFileProviderItemIdentifier

    init(container: NSFileProviderItemIdentifier) {
        self.container = container
        super.init()
    }

    func invalidate() {}

    func enumerateItems(for observer: NSFileProviderEnumerationObserver, startingAt page: NSFileProviderPage) {
        fpLog.info("enumerateItems \(self.container.rawValue, privacy: .public)")
        if container == .rootContainer {
            observer.didEnumerate([TracerItem.folder])
        }
        observer.finishEnumerating(upTo: nil)
    }

    func enumerateChanges(for observer: NSFileProviderChangeObserver, from anchor: NSFileProviderSyncAnchor) {
        fpLog.info("enumerateChanges \(self.container.rawValue, privacy: .public)")
        observer.finishEnumeratingChanges(upTo: Self.anchor, moreComing: false)
    }

    func currentSyncAnchor(completionHandler: @escaping (NSFileProviderSyncAnchor?) -> Void) {
        completionHandler(Self.anchor)
    }
}
