import Foundation
import Security

struct Session {
    var serverUrl: String
    var user: String
    var storage: String
    var insecure: Bool
    var token: String
}

enum SessionStore {
    private static let service = "app.filestash.sync"
    private static let tokenAccount = "session-token"

    static func save(_ session: Session) {
        let defaults = UserDefaults.standard
        defaults.set(session.serverUrl, forKey: "serverUrl")
        defaults.set(session.user, forKey: "user")
        defaults.set(session.storage, forKey: "storage")
        defaults.set(session.insecure, forKey: "insecure")
        saveToken(session.token)
    }

    static func load() -> Session? {
        guard let last = lastKnown(), let token = loadToken() else { return nil }
        var session = last
        session.token = token
        return session
    }

    static func lastKnown() -> Session? {
        let defaults = UserDefaults.standard
        guard let serverUrl = defaults.string(forKey: "serverUrl"),
              let user = defaults.string(forKey: "user"),
              let storage = defaults.string(forKey: "storage")
        else { return nil }
        return Session(
            serverUrl: serverUrl,
            user: user,
            storage: storage,
            insecure: defaults.bool(forKey: "insecure"),
            token: ""
        )
    }

    static func clear() {
        SecItemDelete(tokenQuery() as CFDictionary)
    }

    private static func saveToken(_ token: String) {
        SecItemDelete(tokenQuery() as CFDictionary)
        var attributes = tokenQuery()
        attributes[kSecValueData as String] = Data(token.utf8)
        attributes[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlock
        SecItemAdd(attributes as CFDictionary, nil)
    }

    private static func loadToken() -> String? {
        var query = tokenQuery()
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne
        var result: AnyObject?
        guard SecItemCopyMatching(query as CFDictionary, &result) == errSecSuccess,
              let data = result as? Data
        else { return nil }
        return String(data: data, encoding: .utf8)
    }

    private static func tokenQuery() -> [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: tokenAccount,
        ]
    }
}
