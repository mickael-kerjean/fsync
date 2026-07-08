import FileProvider
import UniformTypeIdentifiers

final class FileProviderItem: NSObject, NSFileProviderItem {
    private let path: String
    private let entry: Entry

    init(path: String, entry: Entry) {
        self.path = path
        self.entry = entry
    }

    static func root() -> FileProviderItem {
        FileProviderItem(path: "/", entry: Entry(name: "", kind: .directory, size: nil, mtimeMs: nil))
    }

    var itemIdentifier: NSFileProviderItemIdentifier {
        ItemMapping.identifier(forPath: path)
    }

    var parentItemIdentifier: NSFileProviderItemIdentifier {
        ItemMapping.parent(of: path)
    }

    var filename: String {
        path == "/" ? "Filestash" : entry.name
    }

    var capabilities: NSFileProviderItemCapabilities {
        if entry.kind == .directory {
            return [
                .allowsReading, .allowsContentEnumerating, .allowsAddingSubItems,
                .allowsDeleting, .allowsRenaming, .allowsReparenting,
            ]
        }
        return [
            .allowsReading, .allowsWriting,
            .allowsDeleting, .allowsRenaming, .allowsReparenting,
        ]
    }

    var contentType: UTType {
        if entry.kind == .directory { return .folder }
        let ext = (entry.name as NSString).pathExtension
        return UTType(filenameExtension: ext) ?? .data
    }

    var documentSize: NSNumber? {
        entry.size.map { NSNumber(value: $0) }
    }

    var contentModificationDate: Date? {
        entry.mtimeMs.map { Date(timeIntervalSince1970: Double($0) / 1000) }
    }

    var itemVersion: NSFileProviderItemVersion {
        let version = Data("\(entry.mtimeMs ?? 0):\(entry.size ?? 0)".utf8)
        return NSFileProviderItemVersion(contentVersion: version, metadataVersion: version)
    }
}
