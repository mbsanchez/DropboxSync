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
            return ProjectionEnumerator(store: store, scope: .workingSet)
        case .rootContainer:
            return ProjectionEnumerator(store: store, scope: .folder(projectionRootItem))
        default:
            guard let itemId = ItemIdCodec.decode(containerItemIdentifier.rawValue) else {
                throw noSuchItem(containerItemIdentifier)
            }
            return ProjectionEnumerator(store: store, scope: .folder(itemId))
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
            case .retry:
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
    var capabilities: NSFileProviderItemCapabilities { [.allowsReading, .allowsContentEnumerating] }
    var itemVersion: NSFileProviderItemVersion {
        NSFileProviderItemVersion(contentVersion: Data("1".utf8), metadataVersion: Data("1".utf8))
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

    var capabilities: NSFileProviderItemCapabilities {
        row.isFolder ? [.allowsReading, .allowsContentEnumerating] : [.allowsReading]
    }

    var documentSize: NSNumber? { row.isFolder ? nil : NSNumber(value: row.sizeBytes) }

    var contentModificationDate: Date? {
        row.modifiedTs > 0 ? Date(timeIntervalSince1970: TimeInterval(row.modifiedTs)) : nil
    }

    var itemVersion: NSFileProviderItemVersion {
        NSFileProviderItemVersion(contentVersion: row.contentVersion, metadataVersion: row.metaVersion)
    }
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

    init(store: ProjectionStore?, scope: Scope) {
        self.store = store
        self.scope = scope
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
