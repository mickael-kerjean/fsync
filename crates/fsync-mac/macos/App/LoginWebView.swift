import SwiftUI
import WebKit

struct LoginWebView: NSViewRepresentable {
    let base: String
    let onToken: (String) -> Void

    func makeCoordinator() -> Coordinator {
        Coordinator(base: base, onToken: onToken)
    }

    func makeNSView(context: Context) -> WKWebView {
        let web = WKWebView()
        context.coordinator.watch(web)
        if let url = URL(string: "\(base)/login") {
            web.load(URLRequest(url: url))
        }
        return web
    }

    func updateNSView(_ web: WKWebView, context: Context) {}

    final class Coordinator: NSObject {
        private let base: String
        private let onToken: (String) -> Void
        private var observation: NSKeyValueObservation?
        private var done = false

        init(base: String, onToken: @escaping (String) -> Void) {
            self.base = base
            self.onToken = onToken
        }

        func watch(_ web: WKWebView) {
            observation = web.observe(\.url) { [weak self, weak web] _, _ in
                guard let self, let web, !self.done,
                      let path = web.url?.path, path.hasPrefix("/files")
                else { return }
                self.capture(web)
            }
        }

        private func capture(_ web: WKWebView) {
            let host = URL(string: base)?.host
            web.configuration.websiteDataStore.httpCookieStore.getAllCookies { [weak self] cookies in
                guard let self, !self.done else { return }
                let token = sessionToken(cookies: cookies, host: host)
                if !token.isEmpty {
                    self.done = true
                    self.onToken(token)
                }
            }
        }
    }
}

func sessionToken(cookies: [HTTPCookie], host: String?) -> String {
    cookies
        .filter { cookie in
            cookie.name.range(of: "^auth\\d*$", options: .regularExpression) != nil
                && (host == nil || cookie.domain.contains(host!))
        }
        .sorted { (Int($0.name.dropFirst(4)) ?? 0) < (Int($1.name.dropFirst(4)) ?? 0) }
        .map(\.value)
        .joined()
}
