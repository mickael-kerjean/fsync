import Foundation
import WebKit

@MainActor
final class AppState: ObservableObject {
    @Published var session: Session? = SessionStore.load()

    func connect(server: String, token: String) async {
        let session = Session(serverUrl: server, user: "", storage: "", insecure: false, token: token)
        SessionStore.save(session)
        // Domain registration is best-effort; the session stands on its own.
        try? await DomainManager.addDomain(for: session)
        self.session = session
    }

    func logout() async {
        if let session {
            Task.detached { endSession(url: session.serverUrl, insecure: session.insecure, token: session.token) }
        }
        try? await DomainManager.removeDomain()
        SessionStore.clear()
        let store = WKWebsiteDataStore.default()
        let types: Set<String> = [WKWebsiteDataTypeCookies]
        let records = await store.dataRecords(ofTypes: types)
        await store.removeData(ofTypes: types, for: records)
        session = nil
    }
}
