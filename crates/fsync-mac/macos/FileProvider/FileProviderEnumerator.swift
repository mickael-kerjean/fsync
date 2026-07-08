import FileProvider

final class FileProviderEnumerator: NSObject, NSFileProviderEnumerator {
    private let adapter: Adapter
    private let path: String

    init(adapter: Adapter, path: String) {
        self.adapter = adapter
        self.path = path
    }

    func enumerateItems(for observer: NSFileProviderEnumerationObserver, startingAt page: NSFileProviderPage) {
        let adapter = adapter
        let path = path
        DispatchQueue.global(qos: .userInitiated).async {
            do {
                if path.hasSuffix("/") {
                    let entries = try adapter.ls(path: path)
                    let items = entries.map { entry in
                        FileProviderItem(
                            path: ItemMapping.child(of: path, name: entry.name, isDirectory: entry.kind == .directory),
                            entry: entry
                        )
                    }
                    DispatchQueue.main.async {
                        observer.didEnumerate(items)
                        observer.finishEnumerating(upTo: nil)
                    }
                } else {
                    let entry = try adapter.stat(path: path)
                    DispatchQueue.main.async {
                        observer.didEnumerate([FileProviderItem(path: path, entry: entry)])
                        observer.finishEnumerating(upTo: nil)
                    }
                }
            } catch {
                DispatchQueue.main.async {
                    observer.finishEnumeratingWithError(mapToProviderError(error))
                }
            }
        }
    }

    func invalidate() {}
}

final class EmptyEnumerator: NSObject, NSFileProviderEnumerator {
    func enumerateItems(for observer: NSFileProviderEnumerationObserver, startingAt page: NSFileProviderPage) {
        observer.finishEnumerating(upTo: nil)
    }

    func invalidate() {}
}
