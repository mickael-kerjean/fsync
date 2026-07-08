import Foundation

enum AdapterHolder {
    static let shared: Adapter? = {
        guard let session = SessionStore.load(),
              let container = FileManager.default.containerURL(
                forSecurityApplicationGroupIdentifier: SessionStore.appGroup)
        else { return nil }
        let data = container.appendingPathComponent("fsync", isDirectory: true)
        return try? Adapter(
            url: session.serverUrl,
            insecure: session.insecure,
            token: session.token,
            dataDir: data.path
        )
    }()
}
