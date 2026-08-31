import FileProvider
import UniformTypeIdentifiers

// Throwaway spike for DBSYNC-79. Four hardcoded items, no network, no database.
// See README.md: this target is deleted before the ticket closes.

// NSFileProviderItem is a class protocol (it refines NSObjectProtocol), so this cannot
// be a struct — the compiler rejected the struct form outright.
private final class SpikeItem: NSObject, NSFileProviderItem {
    let itemIdentifier: NSFileProviderItemIdentifier
    let parentItemIdentifier: NSFileProviderItemIdentifier
    let filename: String
    let documentSize: NSNumber?

    init(
        itemIdentifier: NSFileProviderItemIdentifier,
        parentItemIdentifier: NSFileProviderItemIdentifier,
        filename: String,
        documentSize: NSNumber?
    ) {
        self.itemIdentifier = itemIdentifier
        self.parentItemIdentifier = parentItemIdentifier
        self.filename = filename
        self.documentSize = documentSize
        super.init()
    }

    var capabilities: NSFileProviderItemCapabilities { [.allowsReading, .allowsContentEnumerating] }
    var contentType: UTType { itemIdentifier == .rootContainer ? .folder : .plainText }

    // File Provider treats a changed itemVersion as "refetch". Both components are opaque
    // Data to the system; the engine's content_hash and rev are the natural sources when
    // this stops being a spike (see the Slice 2 mapping on GitHub #141).
    var itemVersion: NSFileProviderItemVersion {
        NSFileProviderItemVersion(contentVersion: Data("v1".utf8), metadataVersion: Data("v1".utf8))
    }
}

private let spikeItems: [SpikeItem] = (1...4).map { n in
    SpikeItem(
        itemIdentifier: NSFileProviderItemIdentifier("spike-item-\(n)"),
        parentItemIdentifier: .rootContainer,
        filename: "spike-\(n).txt",
        documentSize: NSNumber(value: 12)
    )
}

private final class SpikeEnumerator: NSObject, NSFileProviderEnumerator {
    func invalidate() {}

    func enumerateItems(for observer: NSFileProviderEnumerationObserver, startingAt _: NSFileProviderPage) {
        observer.didEnumerate(spikeItems)
        observer.finishEnumerating(upTo: nil)
    }

    // A static anchor is correct for a spike and wrong for anything real: the engine's
    // remote_delta_cursor is the counterpart the production provider would advance.
    func currentSyncAnchor(completionHandler: @escaping (NSFileProviderSyncAnchor?) -> Void) {
        completionHandler(NSFileProviderSyncAnchor(Data("anchor-0".utf8)))
    }

    func enumerateChanges(for observer: NSFileProviderChangeObserver, from _: NSFileProviderSyncAnchor) {
        observer.finishEnumeratingChanges(upTo: NSFileProviderSyncAnchor(Data("anchor-0".utf8)), moreComing: false)
    }
}

@objc(FileProviderSpike)
final class FileProviderSpike: NSObject, NSFileProviderReplicatedExtension {
    required init(domain: NSFileProviderDomain) {
        super.init()
        _ = domain
    }

    func invalidate() {}

    func item(
        for identifier: NSFileProviderItemIdentifier,
        request _: NSFileProviderRequest,
        completionHandler: @escaping (NSFileProviderItem?, Error?) -> Void
    ) -> Progress {
        if identifier == .rootContainer {
            completionHandler(
                SpikeItem(
                    itemIdentifier: .rootContainer,
                    parentItemIdentifier: .rootContainer,
                    filename: "FileProviderSpike",
                    documentSize: nil
                ),
                nil
            )
        } else if let found = spikeItems.first(where: { $0.itemIdentifier == identifier }) {
            completionHandler(found, nil)
        } else {
            completionHandler(nil, NSFileProviderError(.noSuchItem))
        }
        return Progress()
    }

    func fetchContents(
        for itemIdentifier: NSFileProviderItemIdentifier,
        version _: NSFileProviderItemVersion?,
        request _: NSFileProviderRequest,
        completionHandler: @escaping (URL?, NSFileProviderItem?, Error?) -> Void
    ) -> Progress {
        guard let found = spikeItems.first(where: { $0.itemIdentifier == itemIdentifier }) else {
            completionHandler(nil, nil, NSFileProviderError(.noSuchItem))
            return Progress()
        }
        do {
            let url = FileManager.default.temporaryDirectory
                .appendingPathComponent(UUID().uuidString)
            try Data("spike bytes\n".utf8).write(to: url)
            completionHandler(url, found, nil)
        } catch {
            completionHandler(nil, nil, error)
        }
        return Progress()
    }

    // create/modify/delete are REQUIRED by NSFileProviderReplicatedExtension, not optional —
    // the compiler refuses conformance without all three. A read-only provider cannot exist:
    // the system owns the filesystem and pushes local mutations at us. This is the "direction
    // of control inverts" gap from the Slice 2 mapping (GitHub #141), made concrete.
    private var unsupported: NSError {
        NSError(domain: NSCocoaErrorDomain, code: NSFeatureUnsupportedError)
    }

    func createItem(
        basedOn _: NSFileProviderItem,
        fields _: NSFileProviderItemFields,
        contents _: URL?,
        options _: NSFileProviderCreateItemOptions = [],
        request _: NSFileProviderRequest,
        completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void
    ) -> Progress {
        completionHandler(nil, [], false, unsupported)
        return Progress()
    }

    func modifyItem(
        _: NSFileProviderItem,
        baseVersion _: NSFileProviderItemVersion,
        changedFields _: NSFileProviderItemFields,
        contents _: URL?,
        options _: NSFileProviderModifyItemOptions = [],
        request _: NSFileProviderRequest,
        completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void
    ) -> Progress {
        completionHandler(nil, [], false, unsupported)
        return Progress()
    }

    func deleteItem(
        identifier _: NSFileProviderItemIdentifier,
        baseVersion _: NSFileProviderItemVersion,
        options _: NSFileProviderDeleteItemOptions = [],
        request _: NSFileProviderRequest,
        completionHandler: @escaping (Error?) -> Void
    ) -> Progress {
        completionHandler(unsupported)
        return Progress()
    }

    func enumerator(
        for containerItemIdentifier: NSFileProviderItemIdentifier,
        request _: NSFileProviderRequest
    ) throws -> NSFileProviderEnumerator {
        _ = containerItemIdentifier
        return SpikeEnumerator()
    }
}
