import FileProvider
import Foundation
import os
import UniformTypeIdentifiers

/// The File Provider extension (DBSYNC-95, ADR-0004).
///
/// It lists the host's projection of the Dropbox tree and changes nothing: it has no network and no token,
/// it only reads `app.db`, and every write answers with an error (slice 4). A download is asked of the host
/// through the exchange in the container (`Exchange.swift`).
///
/// `@objc(DropboxSyncFileProvider)` makes the runtime class name the bare literal that Info.plist names in
/// `NSExtensionPrincipalClass`. `build-appex.sh` checks the two agree.
@objc(DropboxSyncFileProvider)
final class DropboxSyncFileProvider: NSObject, NSFileProviderReplicatedExtension {
    private let domain: NSFileProviderDomain
    private let store: ProjectionStore?
    private let exchange: ExchangeClient?
    /// What the system has applied, which a folder delete carries (DBSYNC-122).
    private let confirmed = ConfirmedAnchor()

    required init(domain: NSFileProviderDomain) {
        self.domain = domain
        let (store, exchange) = Self.openContainer()
        self.store = store
        self.exchange = exchange
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
        switch identifier {
        case .rootContainer:
            completionHandler(RootItem(), nil)
        default:
            guard let itemId = ItemIdCodec.decode(identifier.rawValue) else {
                completionHandler(nil, noSuchItem(identifier))
                return Progress(totalUnitCount: 1)
            }
            do {
                switch try requireStore(store).presence(itemId) {
                case .live(let row):
                    completionHandler(ProjectionItem(row: row), nil)
                case .notLive:
                    completionHandler(nil, ReadAnswer.transientError(code: 0))
                case .unknown:
                    completionHandler(nil, noSuchItem(identifier))
                }
            } catch {
                completionHandler(nil, readError(error, during: "item"))
            }
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
        case .workingSet:
            return ProjectionEnumerator(store: store, scope: .workingSet, confirmed: confirmed)
        case .rootContainer:
            return ProjectionEnumerator(store: store, scope: .folder(projectionRootItem), confirmed: confirmed)
        default:
            guard let itemId = ItemIdCodec.decode(containerItemIdentifier.rawValue) else {
                throw noSuchItem(containerItemIdentifier)
            }
            return ProjectionEnumerator(store: store, scope: .folder(itemId), confirmed: confirmed)
        }
    }

    func fetchContents(
        for itemIdentifier: NSFileProviderItemIdentifier,
        version requestedVersion: NSFileProviderItemVersion?,
        request: NSFileProviderRequest,
        completionHandler: @escaping (URL?, NSFileProviderItem?, Error?) -> Void
    ) -> Progress {
        let progress = Progress(totalUnitCount: 1)
        guard let itemId = ItemIdCodec.decode(itemIdentifier.rawValue) else {
            completionHandler(nil, nil, noSuchItem(itemIdentifier))
            return progress
        }
        do {
            switch try requireStore(store).presence(itemId) {
            case .unknown:
                completionHandler(nil, nil, noSuchItem(itemIdentifier))
                return progress
            case .notLive:
                completionHandler(nil, nil, ReadAnswer.transientError(code: 0))
                return progress
            case .live:
                break
            }
        } catch {
            completionHandler(nil, nil, readError(error, during: "fetchContents"))
            return progress
        }
        guard let exchange = exchange else {
            completionHandler(nil, nil, NSFileProviderError(.serverUnreachable))
            return progress
        }
        let store = self.store
        exchange.fetch(
            itemId: itemId, viewer: request.isFileViewerRequest, isCancelled: { progress.isCancelled }
        ) { outcome in
            fpLog.info("fetchContents \(itemId, privacy: .public): \(outcome.logLabel, privacy: .public)")
            switch outcome {
            case .staged(let url, let contentVersion):
                do {
                    let current = try requireStore(store).presence(itemId)
                    guard let row = rowToHandOver(stagedVersion: contentVersion, presence: current) else {
                        // The row moved on during the download. The system asks again for the new version.
                        try? FileManager.default.removeItem(at: url)
                        completionHandler(nil, nil, ReadAnswer.transientError(code: 0))
                        return
                    }
                    progress.completedUnitCount = 1
                    completionHandler(url, ProjectionItem(row: row), nil)
                } catch {
                    try? FileManager.default.removeItem(at: url)
                    completionHandler(nil, nil, readError(error, during: "fetchContents"))
                }
            case .hostDown, .offline:
                completionHandler(nil, nil, NSFileProviderError(.serverUnreachable))
            case .auth:
                completionHandler(nil, nil, NSFileProviderError(.notAuthenticated))
            case .refused:
                // The header's answer for content the user cannot have: not retried. The system shows
                // nothing for it, so the host opens the file on dropbox.com for a Finder request.
                completionHandler(nil, nil, CocoaError(.fileReadNoPermission))
            case .retry, .written, .collision, .conflict, .deleted, .deletionRejected, .gone, .goneParent, .done:
                completionHandler(nil, nil, ReadAnswer.transientError(code: 0))
            case .cancelled:
                completionHandler(nil, nil, CocoaError(.userCancelled))
            }
        }
        return progress
    }

    func createItem(
        basedOn itemTemplate: NSFileProviderItem,
        fields: NSFileProviderItemFields,
        contents url: URL?,
        options: NSFileProviderCreateItemOptions = [],
        request: NSFileProviderRequest,
        completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void
    ) -> Progress {
        let progress = Progress(totalUnitCount: 1)
        let store = self.store
        let parent: Int64
        switch itemTemplate.parentItemIdentifier {
        case .rootContainer:
            parent = projectionRootItem
        case .trashContainer:
            completionHandler(nil, [], false, CocoaError(.featureUnsupported))
            return progress
        default:
            guard let decoded = ItemIdCodec.decode(itemTemplate.parentItemIdentifier.rawValue) else {
                completionHandler(nil, [], false, noSuchItem(itemTemplate.parentItemIdentifier))
                return progress
            }
            parent = decoded
        }
        let type = itemTemplate.contentType ?? .data
        // A symbolic link or an alias has no Dropbox counterpart this app writes, and neither has a package
        // (an .rtfd, a .pages bundle): a directory the host would have to send as one file.
        guard !type.conforms(to: .symbolicLink), !type.conforms(to: .aliasFile),
              type.conforms(to: .folder) || !type.conforms(to: .package) else {
            completionHandler(nil, [], false, CocoaError(.featureUnsupported))
            return progress
        }
        let folder = type.conforms(to: .folder)
        fpLog.info("createItem options=\(options.rawValue, privacy: .public) folder=\(folder, privacy: .public)")
        if options.contains(.mayAlreadyExist) {
            let existing: ProjectionRow?
            do {
                existing = try requireStore(store).child(of: parent, named: itemTemplate.filename)
            } catch {
                completionHandler(nil, [], false, readError(error, during: "createItem reimport"))
                return progress
            }
            switch Reimport.decide(existing: existing, folder: folder, hasContents: url != nil) {
            case .existing(let row):
                progress.completedUnitCount = 1
                completionHandler(ProjectionItem(row: row), [], false, nil)
                return progress
            case .refuse:
                progress.completedUnitCount = 1
                completionHandler(nil, [], false, nil)
                return progress
            case .create:
                break
            }
        }
        guard folder || url != nil else {
            completionHandler(nil, [], false, ReadAnswer.transientError(code: 0))
            return progress
        }
        guard let exchange = exchange else {
            completionHandler(nil, [], false, NSFileProviderError(.serverUnreachable))
            return progress
        }
        let name = itemTemplate.filename
        exchange.create(
            parentItem: parent, name: name, folder: folder, contents: url,
            isCancelled: { progress.isCancelled }
        ) { outcome in
            fpLog.info("createItem: \(outcome.logLabel, privacy: .public)")
            switch outcome {
            case .written(let itemId):
                guard case .live(let row)? = try? requireStore(store).presence(itemId) else {
                    completionHandler(nil, [], false, ReadAnswer.transientError(code: 0))
                    return
                }
                progress.completedUnitCount = 1
                completionHandler(ProjectionItem(row: row), [], false, nil)
            case .collision:
                if let existing = try? requireStore(store).child(of: parent, named: name) {
                    completionHandler(
                        nil, [], false, NSError.fileProviderErrorForCollision(with: ProjectionItem(row: existing)))
                } else {
                    completionHandler(nil, [], false, ReadAnswer.transientError(code: 0))
                }
            case .hostDown, .offline:
                completionHandler(nil, [], false, NSFileProviderError(.serverUnreachable))
            case .auth:
                completionHandler(nil, [], false, NSFileProviderError(.notAuthenticated))
            case .cancelled:
                completionHandler(nil, [], false, CocoaError(.userCancelled))
            case .gone, .goneParent:
                // The folder was deleted on Dropbox: the system re-creates it, then this item.
                completionHandler(nil, [], false, noSuchItem(itemTemplate.parentItemIdentifier))
            case .refused:
                // Selective sync would hide it: it would reach Dropbox and never show here.
                completionHandler(nil, [], false, NSFileProviderError(.cannotSynchronize))
            case .staged, .retry, .conflict, .deleted, .deletionRejected, .done:
                completionHandler(nil, [], false, ReadAnswer.transientError(code: 0))
            }
        }
        return progress
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
        let progress = Progress(totalUnitCount: 1)
        guard let itemId = ItemIdCodec.decode(item.itemIdentifier.rawValue) else {
            completionHandler(nil, [], false, noSuchItem(item.itemIdentifier))
            return progress
        }
        var newPlace: (parent: Int64, name: String)?
        if !changedFields.isDisjoint(with: [.filename, .parentItemIdentifier]) {
            switch item.parentItemIdentifier {
            case .rootContainer:
                newPlace = (projectionRootItem, item.filename)
            case .trashContainer:
                completionHandler(nil, [], false, CocoaError(.featureUnsupported))
                return progress
            default:
                guard let parent = ItemIdCodec.decode(item.parentItemIdentifier.rawValue) else {
                    completionHandler(nil, [], false, noSuchItem(item.parentItemIdentifier))
                    return progress
                }
                newPlace = (parent, item.filename)
            }
        }
        let contents = changedFields.contains(.contents) ? newContents : nil
        if let contents = contents,
           (try? contents.resourceValues(forKeys: [.isDirectoryKey]))?.isDirectory == true {
            // A package's content: see createItem.
            completionHandler(nil, [], false, CocoaError(.featureUnsupported))
            return progress
        }
        let store = self.store
        guard newPlace != nil || contents != nil else {
            // Dates, tags and the like: nothing Dropbox stores. The item is what it was.
            if case .live(let row)? = try? requireStore(store).presence(itemId) {
                completionHandler(ProjectionItem(row: row), [], false, nil)
            } else {
                completionHandler(nil, [], false, ReadAnswer.transientError(code: 0))
            }
            return progress
        }
        guard let exchange = exchange else {
            completionHandler(nil, [], false, NSFileProviderError(.serverUnreachable))
            return progress
        }
        exchange.modify(
            itemId: itemId, baseVersion: version.contentVersion, newPlace: newPlace, contents: contents,
            isCancelled: { progress.isCancelled }
        ) { outcome in
            fpLog.info("modifyItem \(itemId, privacy: .public): \(outcome.logLabel, privacy: .public)")
            switch outcome {
            case .written(let id), .conflict(let id):
                guard case .live(let row)? = try? requireStore(store).presence(id) else {
                    completionHandler(nil, [], false, ReadAnswer.transientError(code: 0))
                    return
                }
                progress.completedUnitCount = 1
                // A conflict leaves Dropbox's version as the item; the system fetches it again, and the
                // edit is the conflicted copy beside it.
                let refetch: Bool
                if case .conflict = outcome { refetch = true } else { refetch = false }
                completionHandler(ProjectionItem(row: row), [], refetch, nil)
            case .collision:
                let existing = newPlace.flatMap { try? requireStore(store).child(of: $0.parent, named: $0.name) }
                completionHandler(
                    nil, [], false,
                    existing.map { NSError.fileProviderErrorForCollision(with: ProjectionItem(row: $0)) }
                        ?? ReadAnswer.transientError(code: 0))
            case .hostDown, .offline:
                completionHandler(nil, [], false, NSFileProviderError(.serverUnreachable))
            case .auth:
                completionHandler(nil, [], false, NSFileProviderError(.notAuthenticated))
            case .cancelled:
                completionHandler(nil, [], false, CocoaError(.userCancelled))
            case .gone:
                // Deleted on Dropbox: the system deletes it from disk, or re-creates it if it holds an edit.
                completionHandler(nil, [], false, noSuchItem(item.itemIdentifier))
            case .goneParent:
                // The folder it was moved into was deleted on Dropbox: the system re-creates that folder.
                completionHandler(nil, [], false, noSuchItem(item.parentItemIdentifier))
            case .refused:
                // Selective sync would hide it where it was moved.
                completionHandler(nil, [], false, NSFileProviderError(.cannotSynchronize))
            case .staged, .retry, .deleted, .deletionRejected, .done:
                completionHandler(nil, [], false, ReadAnswer.transientError(code: 0))
            }
        }
        return progress
    }

    func deleteItem(
        identifier: NSFileProviderItemIdentifier,
        baseVersion version: NSFileProviderItemVersion,
        options: NSFileProviderDeleteItemOptions = [],
        request: NSFileProviderRequest,
        completionHandler: @escaping (Error?) -> Void
    ) -> Progress {
        let progress = Progress(totalUnitCount: 1)
        guard let itemId = ItemIdCodec.decode(identifier.rawValue) else {
            completionHandler(noSuchItem(identifier))
            return progress
        }
        let store = self.store
        var isFolder = false
        if case .live(let row)? = try? requireStore(store).presence(itemId) {
            isFolder = row.isFolder
        }
        // A delete that is not recursive must not take children the system has not seen yet, such as
        // files added on Dropbox since the folder was last listed: Dropbox deletes a folder whole.
        if !options.contains(.recursive), isFolder,
           let children = try? requireStore(store).children(of: itemId, afterItem: 0, limit: 1),
           !children.isEmpty {
            completionHandler(NSFileProviderError(.directoryNotEmpty))
            return progress
        }
        guard let exchange = exchange else {
            completionHandler(NSFileProviderError(.serverUnreachable))
            return progress
        }
        // A folder has no content version: the anchor the system confirmed stands in for it (DBSYNC-122).
        // Sent for every delete, so a failed read above cannot drop it; the host reads it for folders only.
        let seen = confirmed.value
        exchange.delete(
            itemId: itemId, baseVersion: version.contentVersion, seen: seen, isCancelled: { progress.isCancelled }
        ) {
            outcome in
            fpLog.info("deleteItem \(itemId, privacy: .public): \(outcome.logLabel, privacy: .public)")
            switch outcome {
            case .deleted, .gone:
                progress.completedUnitCount = 1
                completionHandler(nil)
            case .goneParent:
                completionHandler(ReadAnswer.transientError(code: 0))
            case .deletionRejected:
                if case .live(let row)? = try? requireStore(store).presence(itemId) {
                    completionHandler(NSError.fileProviderErrorForRejectedDeletion(of: ProjectionItem(row: row)))
                } else {
                    completionHandler(ReadAnswer.transientError(code: 0))
                }
            case .hostDown, .offline:
                completionHandler(NSFileProviderError(.serverUnreachable))
            case .auth:
                completionHandler(NSFileProviderError(.notAuthenticated))
            case .cancelled:
                completionHandler(CocoaError(.userCancelled))
            case .staged, .refused, .retry, .written, .conflict, .collision, .done:
                completionHandler(ReadAnswer.transientError(code: 0))
            }
        }
        return progress
    }
}

/// *Make Available Offline* and *Make Online-Only* (#237). The system has no pin of its own for a
/// third-party domain, so these are the extension's actions, as the Dropbox client's are: the host records
/// the pin, the item comes back with a content policy that keeps it downloaded, and an unpinned item is
/// evicted. The policy only schedules a background download, which waits while the Mac is on battery
/// (measured), so a pin names the domain's folder and the host reads the pinned files there, which
/// downloads them now. Make Online-Only also evicts what is downloaded and not pinned:
/// Info.plist hides the system's Download Now and Remove Download, which read as the same two actions.
extension DropboxSyncFileProvider: NSFileProviderCustomAction {
    func performAction(
        identifier actionIdentifier: NSFileProviderExtensionActionIdentifier,
        onItemsWithIdentifiers itemIdentifiers: [NSFileProviderItemIdentifier],
        completionHandler: @escaping (Error?) -> Void
    ) -> Progress {
        let progress = Progress(totalUnitCount: Int64(itemIdentifiers.count))
        guard let pinned = OfflineAction.pins(actionIdentifier.rawValue) else {
            completionHandler(CocoaError(.featureUnsupported))
            return progress
        }
        guard let exchange = exchange else {
            completionHandler(NSFileProviderError(.serverUnreachable))
            return progress
        }
        let manager = NSFileProviderManager(for: domain)
        let items = itemIdentifiers.compactMap { identifier in
            ItemIdCodec.decode(identifier.rawValue).map { (identifier, $0) }
        }
        var failure: Error?
        var root = ""
        func next(_ index: Int) {
            guard index < items.count else {
                completionHandler(progress.isCancelled ? CocoaError(.userCancelled) : failure)
                return
            }
            let (identifier, itemId) = items[index]
            // A multiple selection reaches here whole, even the items the action is not offered on. One
            // under a pinned folder is offline through it: skipped, as its menu would (found in review:
            // unpinning it did nothing, and its eviction failed the whole action).
            if case .live(let row)? = try? requireStore(store).presence(itemId), row.inherited {
                fpLog.info("\(pinned ? "pin" : "unpin", privacy: .public) \(itemId, privacy: .public): under a pinned folder, skipped")
                progress.completedUnitCount += 1
                next(index + 1)
                return
            }
            exchange.pin(itemId: itemId, pinned: pinned, root: root, isCancelled: { progress.isCancelled }) { outcome in
                fpLog.info("\(pinned ? "pin" : "unpin", privacy: .public) \(itemId, privacy: .public): \(outcome.logLabel, privacy: .public)")
                switch outcome {
                case .done:
                    progress.completedUnitCount += 1
                    if pinned {
                        next(index + 1)
                    } else {
                        Self.evict(identifier, manager: manager, attempt: 0) { error in
                            if let error = error, failure == nil { failure = error }
                            next(index + 1)
                        }
                    }
                case .gone:
                    // Deleted on Dropbox meanwhile: nothing left to pin.
                    progress.completedUnitCount += 1
                    next(index + 1)
                case .hostDown, .offline:
                    failure = failure ?? NSFileProviderError(.serverUnreachable)
                    next(index + 1)
                case .cancelled:
                    completionHandler(CocoaError(.userCancelled))
                default:
                    failure = failure ?? ReadAnswer.transientError(code: 0)
                    next(index + 1)
                }
            }
        }
        guard pinned, let manager = manager else {
            next(0)
            return progress
        }
        manager.getUserVisibleURL(for: .rootContainer) { url, error in
            if let error = error {
                fpLog.error("no visible root: \(String(describing: error), privacy: .public)")
            }
            root = url?.path ?? ""
            next(0)
        }
        return progress
    }

    /// Evicts an item the host just unpinned. The system may not have read the item's new policy yet, and
    /// refuses to evict while it still holds the pinned one: asked again for a few seconds.
    private static func evict(
        _ identifier: NSFileProviderItemIdentifier,
        manager: NSFileProviderManager?,
        attempt: Int,
        done: @escaping (Error?) -> Void
    ) {
        guard let manager = manager else {
            done(nil)
            return
        }
        manager.evictItem(identifier: identifier) { error in
            let code = (error as NSError?).flatMap { $0.domain == NSFileProviderErrorDomain ? $0.code : nil }
            let notYet = code == NSFileProviderError.Code.nonEvictable.rawValue
                || code == NSFileProviderError.Code.nonEvictableChildren.rawValue
            if notYet && attempt < 10 {
                DispatchQueue.global().asyncAfter(deadline: .now() + .milliseconds(500)) {
                    evict(identifier, manager: manager, attempt: attempt + 1, done: done)
                }
                return
            }
            if let error = error {
                fpLog.error("evict \(identifier.rawValue, privacy: .public): \(String(describing: error), privacy: .public)")
            }
            done(error)
        }
    }
}

extension DropboxSyncFileProvider {
    /// The store and the exchange in the App Group container, or `nil` without a group: a build without a
    /// team, which then fails every read with `ReadFailure.noContainer` and logs why.
    private static func openContainer() -> (ProjectionStore?, ExchangeClient?) {
        let groupID = (Bundle.main.object(forInfoDictionaryKey: "DBSyncAppGroupID") as? String) ?? ""
        guard !groupID.isEmpty,
              let container = FileManager.default.containerURL(
                  forSecurityApplicationGroupIdentifier: groupID) else {
            fpLog.error("no App Group container")
            return (nil, nil)
        }
        let exchange = ExchangePaths(
            dataDirectory: ProjectionStore.dataDirectory(inContainer: container))
        return (
            ProjectionStore(databasePath: ProjectionStore.path(inContainer: container)),
            ExchangeClient(paths: exchange, notification: groupID + exchangeNotificationSuffix)
        )
    }
}

/// The store, or the failure that says why there is none.
func requireStore(_ store: ProjectionStore?) throws -> ProjectionStore {
    guard let store = store else { throw ReadFailure.noContainer }
    return store
}

/// What the system gets for a failed read (`ReadFailure.answer`). Logged by class only: no name or path
/// reaches the log.
func readError(_ error: Error, during operation: StaticString) -> Error {
    fpLog.error("read failed during \(operation.description, privacy: .public): \(String(describing: error), privacy: .public)")
    switch (error as? ReadFailure)?.answer ?? .retry(code: 0) {
    case .waitForSignal:
        return NSFileProviderError(.serverUnreachable)
    case .retry(let code):
        return ReadAnswer.transientError(code: code)
    }
}

/// The header's form of `noSuchItem`. The system takes it to mean the item was deleted and attempts to
/// delete it from disk, so it answers only for an identifier with no live row.
func noSuchItem(_ identifier: NSFileProviderItemIdentifier) -> Error {
    NSError.fileProviderErrorForNonExistentItem(withIdentifier: identifier)
}

/// Shared logger. Identifiers (`i<n>`) and counts are logged public; a name or a path never is.
let fpLog = Logger(subsystem: "com.mobsanchez.dropboxsyncdesktop.fileprovider", category: "extension")

/// The domain root. It is not a row of the projection.
final class RootItem: NSObject, NSFileProviderItem {
    var itemIdentifier: NSFileProviderItemIdentifier { .rootContainer }
    var parentItemIdentifier: NSFileProviderItemIdentifier { .rootContainer }
    var filename: String { "Dropbox Sync" }
    var contentType: UTType { .folder }
    var capabilities: NSFileProviderItemCapabilities {
        [.allowsReading, .allowsContentEnumerating, .allowsAddingSubItems]
    }
    var itemVersion: NSFileProviderItemVersion {
        NSFileProviderItemVersion(contentVersion: Data("1".utf8), metadataVersion: ItemFormat.metadataVersion(Data()))
    }
}

/// One row of the projection.
final class ProjectionItem: NSObject, NSFileProviderItem {
    private let row: ProjectionRow

    init(row: ProjectionRow) {
        self.row = row
        super.init()
    }

    var itemIdentifier: NSFileProviderItemIdentifier {
        NSFileProviderItemIdentifier(ItemIdCodec.encode(row.itemId))
    }

    var parentItemIdentifier: NSFileProviderItemIdentifier {
        row.parentItem == projectionRootItem
            ? .rootContainer
            : NSFileProviderItemIdentifier(ItemIdCodec.encode(row.parentItem))
    }

    var filename: String { row.name }

    var contentType: UTType {
        if row.isFolder { return .folder }
        let suffix = (row.name as NSString).pathExtension
        return UTType(filenameExtension: suffix) ?? .data
    }

    /// Trashing only from macOS 15, where the host tells the domain not to sync a trash: Finder then
    /// moves the item to the user's Trash and sends `deleteItem`. Before that, trashing would mean a
    /// domain trash this extension does not keep.
    var capabilities: NSFileProviderItemCapabilities {
        var moving: NSFileProviderItemCapabilities = [.allowsRenaming, .allowsReparenting, .allowsDeleting]
        if #available(macOS 15, *) {
            moving.insert(.allowsTrashing)
        }
        return row.isFolder
            ? moving.union([.allowsReading, .allowsContentEnumerating, .allowsAddingSubItems])
            : moving.union([.allowsReading, .allowsWriting])
    }

    var documentSize: NSNumber? { row.isFolder ? nil : NSNumber(value: row.sizeBytes) }

    var contentModificationDate: Date? {
        row.modifiedTs > 0 ? Date(timeIntervalSince1970: TimeInterval(row.modifiedTs)) : nil
    }

    var itemVersion: NSFileProviderItemVersion {
        NSFileProviderItemVersion(
            contentVersion: row.contentVersion,
            metadataVersion: ItemFormat.metadataVersion(
                row.metaVersion, pinned: row.pinned, inherited: row.inherited))
    }

    /// A pinned item is downloaded and kept downloaded, and a pinned folder passes that on to what is
    /// under it, whose own policy is inherited (#237).
    var contentPolicy: NSFileProviderContentPolicy {
        row.pinned ? .downloadEagerlyAndKeepDownloaded : .inherited
    }

    /// What the Finder actions in Info.plist test: which of them to offer. An item under a pinned folder is
    /// offered neither: it is offline through the folder, and the system would refuse to evict it.
    var userInfo: [AnyHashable: Any]? { ["pinned": row.pinned, "inherited": row.inherited] }
}

/// Lists a folder or the working set from the projection, one page per call.
///
/// The changes are the whole domain's whichever enumerator is asked: a replicated extension only signals
/// the working set, and an item's identifier means the same thing everywhere.
final class ProjectionEnumerator: NSObject, NSFileProviderEnumerator {
    enum Scope {
        case workingSet
        case folder(Int64)
    }

    private let store: ProjectionStore?
    private let scope: Scope
    private let confirmed: ConfirmedAnchor

    init(store: ProjectionStore?, scope: Scope, confirmed: ConfirmedAnchor) {
        self.store = store
        self.scope = scope
        self.confirmed = confirmed
        super.init()
    }

    func invalidate() {}

    func enumerateItems(for observer: NSFileProviderEnumerationObserver, startingAt page: NSFileProviderPage) {
        let after = PageCodec.afterItem(page.rawValue)
        // One row more than a batch: it tells a last full page from one with a next.
        let limit = projectionBatchSize + 1
        do {
            let store = try requireStore(self.store)
            let rows: [ProjectionRow]
            switch scope {
            case .workingSet:
                rows = try store.liveItems(afterItem: after, limit: limit)
            case .folder(let parent):
                guard let children = try store.children(of: parent, afterItem: after, limit: limit) else {
                    let known = try store.presence(parent) != .unknown
                    observer.finishEnumeratingWithError(
                        known
                            ? ReadAnswer.transientError(code: 0)
                            : noSuchItem(NSFileProviderItemIdentifier(ItemIdCodec.encode(parent))))
                    return
                }
                rows = children
            }
            let (served, next) = PageCodec.next(rows: rows, batch: projectionBatchSize)
            fpLog.info("enumerateItems rows=\(served.count, privacy: .public)")
            observer.didEnumerate(served.map(ProjectionItem.init(row:)))
            observer.finishEnumerating(upTo: next.map(NSFileProviderPage.init(_:)))
        } catch {
            observer.finishEnumeratingWithError(readError(error, during: "enumerateItems"))
        }
    }

    func enumerateChanges(for observer: NSFileProviderChangeObserver, from anchor: NSFileProviderSyncAnchor) {
        do {
            let store = try requireStore(self.store)
            let from = ProjectionAnchor(encoded: anchor.rawValue)
            guard let read = try store.changes(from: from, limit: projectionBatchSize + 1) else {
                fpLog.info("enumerateChanges: anchor expired")
                observer.finishEnumeratingWithError(NSFileProviderError(.syncAnchorExpired))
                return
            }
            // Read, so not expired: the system applied everything up to it (DBSYNC-122). Only the working
            // set's anchor says that for the whole domain; a folder's covers that folder's listing.
            let workingSet: Bool
            if case .workingSet = scope { workingSet = true } else { workingSet = false }
            if workingSet, let from = from {
                confirmed.confirm(from)
            }
            let plan = ChangePlan.make(rows: read.rows, batch: projectionBatchSize, head: read.head)
            fpLog.info("enumerateChanges updated=\(plan.updated.count, privacy: .public) deleted=\(plan.deletedItemIds.count, privacy: .public)")
            if !plan.updated.isEmpty {
                observer.didUpdate(plan.updated.map(ProjectionItem.init(row:)))
            }
            if !plan.deletedItemIds.isEmpty {
                observer.didDeleteItems(withIdentifiers: plan.deletedItemIds.map {
                    NSFileProviderItemIdentifier(ItemIdCodec.encode($0))
                })
            }
            observer.finishEnumeratingChanges(
                upTo: NSFileProviderSyncAnchor(plan.upTo.encoded), moreComing: plan.moreComing)
        } catch {
            observer.finishEnumeratingWithError(readError(error, during: "enumerateChanges"))
        }
    }

    /// `nil` for any failed read, on a healthy projection too. The header allows it and does not say what
    /// the system does with it.
    func currentSyncAnchor(completionHandler: @escaping (NSFileProviderSyncAnchor?) -> Void) {
        do {
            let head = try requireStore(store).head()
            completionHandler(NSFileProviderSyncAnchor(ProjectionAnchor(head: head).encoded))
        } catch {
            _ = readError(error, during: "currentSyncAnchor")
            completionHandler(nil)
        }
    }
}
