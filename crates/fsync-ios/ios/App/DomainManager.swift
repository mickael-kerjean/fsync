import FileProvider
import UIKit

enum DomainManager {
    private static let identifier = NSFileProviderDomainIdentifier(rawValue: "filestash")

    static func addDomain(for session: Session) async throws {
        let host = URL(string: session.serverUrl)?.host ?? session.serverUrl
        let name = session.user.isEmpty ? host : "\(session.user)@\(host)/\(session.storage)"
        try await NSFileProviderManager.add(NSFileProviderDomain(identifier: identifier, displayName: name))
    }

    static func removeDomain() async throws {
        try await NSFileProviderManager.remove(NSFileProviderDomain(identifier: identifier, displayName: "Filestash"))
    }

    @MainActor
    static func open() async {
        let domain = NSFileProviderDomain(identifier: identifier, displayName: "Filestash")
        guard let manager = NSFileProviderManager(for: domain),
              let root = try? await manager.getUserVisibleURL(for: .rootContainer),
              var components = URLComponents(url: root, resolvingAgainstBaseURL: false)
        else { return }
        components.scheme = "shareddocuments"
        if let url = components.url {
            await UIApplication.shared.open(url)
        }
    }
}
