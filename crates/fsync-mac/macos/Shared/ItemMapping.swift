import FileProvider

enum ItemMapping {
    static func path(for identifier: NSFileProviderItemIdentifier) -> String {
        identifier == .rootContainer ? "/" : identifier.rawValue
    }

    static func identifier(forPath path: String) -> NSFileProviderItemIdentifier {
        path == "/" ? .rootContainer : NSFileProviderItemIdentifier(path)
    }

    static func child(of directoryPath: String, name: String, isDirectory: Bool) -> String {
        directoryPath + name + (isDirectory ? "/" : "")
    }

    static func parent(of path: String) -> NSFileProviderItemIdentifier {
        let trimmed = path.hasSuffix("/") ? String(path.dropLast()) : path
        guard let slash = trimmed.lastIndex(of: "/") else { return .rootContainer }
        return identifier(forPath: String(trimmed[...slash]))
    }
}
