import FileProvider

final class FileProviderExtension: NSObject, NSFileProviderReplicatedExtension {
    private let manager: NSFileProviderManager?
    private let adapter: Adapter?

    required init(domain: NSFileProviderDomain) {
        self.manager = NSFileProviderManager(for: domain)
        self.adapter = AdapterHolder.shared
        super.init()
        adapter?.recover()
    }

    func invalidate() {
        adapter?.flush(timeoutMs: 10_000)
    }

    func item(
        for identifier: NSFileProviderItemIdentifier,
        request: NSFileProviderRequest,
        completionHandler: @escaping (NSFileProviderItem?, Error?) -> Void
    ) -> Progress {
        if identifier == .rootContainer {
            completionHandler(FileProviderItem.root(), nil)
            return Progress()
        }
        if identifier == .trashContainer || identifier == .workingSet {
            completionHandler(nil, NSFileProviderError(.noSuchItem))
            return Progress()
        }
        guard let adapter else {
            completionHandler(nil, NSFileProviderError(.notAuthenticated))
            return Progress()
        }
        let path = ItemMapping.path(for: identifier)
        DispatchQueue.global(qos: .userInitiated).async {
            do {
                let entry = try adapter.stat(path: path)
                completionHandler(FileProviderItem(path: path, entry: entry), nil)
            } catch {
                completionHandler(nil, mapToProviderError(error))
            }
        }
        return Progress()
    }

    func fetchContents(
        for itemIdentifier: NSFileProviderItemIdentifier,
        version requestedVersion: NSFileProviderItemVersion?,
        request: NSFileProviderRequest,
        completionHandler: @escaping (URL?, NSFileProviderItem?, Error?) -> Void
    ) -> Progress {
        guard let adapter, let manager else {
            completionHandler(nil, nil, NSFileProviderError(.notAuthenticated))
            return Progress()
        }
        let path = ItemMapping.path(for: itemIdentifier)
        DispatchQueue.global(qos: .userInitiated).async {
            do {
                let entry = try adapter.stat(path: path)
                let destination = try manager.temporaryDirectoryURL()
                    .appendingPathComponent(UUID().uuidString)
                try adapter.fetch(path: path, destPath: destination.path)
                completionHandler(destination, FileProviderItem(path: path, entry: entry), nil)
            } catch {
                completionHandler(nil, nil, mapToProviderError(error))
            }
        }
        return Progress()
    }

    func createItem(
        basedOn itemTemplate: NSFileProviderItem,
        fields: NSFileProviderItemFields,
        contents url: URL?,
        options: NSFileProviderCreateItemOptions = [],
        request: NSFileProviderRequest,
        completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void
    ) -> Progress {
        guard let adapter else {
            completionHandler(nil, [], false, NSFileProviderError(.notAuthenticated))
            return Progress()
        }
        let isDirectory = itemTemplate.contentType == .folder
        let parent = ItemMapping.path(for: itemTemplate.parentItemIdentifier)
        let path = ItemMapping.child(of: parent, name: itemTemplate.filename, isDirectory: isDirectory)
        DispatchQueue.global(qos: .userInitiated).async {
            do {
                if isDirectory {
                    try adapter.mkdir(path: path)
                } else {
                    try adapter.created(path: path, contentsPath: url?.path)
                }
                let entry = try adapter.stat(path: path)
                completionHandler(FileProviderItem(path: path, entry: entry), [], false, nil)
            } catch {
                completionHandler(nil, [], false, mapToProviderError(error))
            }
        }
        return Progress()
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
        guard let adapter else {
            completionHandler(nil, [], false, NSFileProviderError(.notAuthenticated))
            return Progress()
        }
        var path = ItemMapping.path(for: item.itemIdentifier)
        let isDirectory = path.hasSuffix("/")
        DispatchQueue.global(qos: .userInitiated).async {
            do {
                if changedFields.contains(.filename) || changedFields.contains(.parentItemIdentifier) {
                    let parent = ItemMapping.path(for: item.parentItemIdentifier)
                    let to = ItemMapping.child(of: parent, name: item.filename, isDirectory: isDirectory)
                    try adapter.rename(from: path, to: to)
                    path = to
                }
                if changedFields.contains(.contents), let newContents {
                    try adapter.modified(path: path, contentsPath: newContents.path)
                }
                let entry = try adapter.stat(path: path)
                completionHandler(FileProviderItem(path: path, entry: entry), [], false, nil)
            } catch {
                completionHandler(nil, [], false, mapToProviderError(error))
            }
        }
        return Progress()
    }

    func deleteItem(
        identifier: NSFileProviderItemIdentifier,
        baseVersion version: NSFileProviderItemVersion,
        options: NSFileProviderDeleteItemOptions = [],
        request: NSFileProviderRequest,
        completionHandler: @escaping (Error?) -> Void
    ) -> Progress {
        guard let adapter else {
            completionHandler(NSFileProviderError(.notAuthenticated))
            return Progress()
        }
        let path = ItemMapping.path(for: identifier)
        DispatchQueue.global(qos: .userInitiated).async {
            do {
                try adapter.delete(path: path)
                completionHandler(nil)
            } catch {
                completionHandler(mapToProviderError(error))
            }
        }
        return Progress()
    }

    func enumerator(
        for containerItemIdentifier: NSFileProviderItemIdentifier,
        request: NSFileProviderRequest
    ) throws -> NSFileProviderEnumerator {
        if containerItemIdentifier == .workingSet {
            return EmptyEnumerator()
        }
        if containerItemIdentifier == .trashContainer {
            throw NSFileProviderError(.noSuchItem)
        }
        guard let adapter else {
            throw NSFileProviderError(.notAuthenticated)
        }
        return FileProviderEnumerator(adapter: adapter, path: ItemMapping.path(for: containerItemIdentifier))
    }
}

extension FileProviderExtension: NSFileProviderThumbnailing {
    func fetchThumbnails(
        for itemIdentifiers: [NSFileProviderItemIdentifier],
        requestedSize size: CGSize,
        perThumbnailCompletionHandler: @escaping (NSFileProviderItemIdentifier, Data?, Error?) -> Void,
        completionHandler: @escaping (Error?) -> Void
    ) -> Progress {
        let progress = Progress(totalUnitCount: Int64(itemIdentifiers.count))
        guard let adapter else {
            completionHandler(NSFileProviderError(.notAuthenticated))
            return progress
        }
        DispatchQueue.global(qos: .utility).async {
            for identifier in itemIdentifiers {
                do {
                    let bytes = try adapter.thumbnail(path: ItemMapping.path(for: identifier))
                    perThumbnailCompletionHandler(identifier, bytes, nil)
                } catch {
                    perThumbnailCompletionHandler(identifier, nil, mapToProviderError(error))
                }
                progress.completedUnitCount += 1
            }
            completionHandler(nil)
        }
        return progress
    }
}

func mapToProviderError(_ error: Error) -> Error {
    guard let fsError = error as? FsError else { return error }
    switch fsError {
    case .NotAuthenticated, .PermissionDenied, .InvalidCredentials:
        return NSFileProviderError(.notAuthenticated)
    case .NotFound:
        return NSFileProviderError(.noSuchItem)
    case .Network:
        return NSFileProviderError(.serverUnreachable)
    case .Other:
        return fsError
    }
}
