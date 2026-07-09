import Foundation
import WebKit

@MainActor
final class AppState: ObservableObject {
    @Published var session: Session?
    @Published var error: String?

    init() {
        let saved = SessionStore.load()
        session = saved
        if let saved {
            try? MountManager.mount(server: saved.serverUrl, token: saved.token)
        }
    }

    func connect(server: String, token: String) async {
        let session = Session(serverUrl: server, user: "", storage: "", insecure: false, token: token)
        SessionStore.save(session)
        do {
            try MountManager.mount(server: server, token: token)
            self.session = session
        } catch {
            self.error = error.localizedDescription
        }
    }

    func logout() async {
        MountManager.unmount()
        SessionStore.clear()
        let store = WKWebsiteDataStore.default()
        let types: Set<String> = [WKWebsiteDataTypeCookies]
        let records = await store.dataRecords(ofTypes: types)
        await store.removeData(ofTypes: types, for: records)
        session = nil
    }
}
