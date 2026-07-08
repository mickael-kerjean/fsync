import SwiftUI

struct LoginView: View {
    @EnvironmentObject var state: AppState
    @State private var url: String
    @State private var server: String?

    init() {
        _url = State(initialValue: SessionStore.lastKnown()?.serverUrl ?? "")
    }

    var body: some View {
        if let server {
            VStack(spacing: 0) {
                HStack {
                    Button("Back") { self.server = nil }
                    Spacer()
                }
                .padding(8)
                LoginWebView(base: server) { token in
                    Task { await state.connect(server: server, token: token) }
                }
            }
            .frame(width: 380, height: 520)
            .background(Color.fsBackground)
        } else {
            VStack(alignment: .leading, spacing: 10) {
                Text("Filestash")
                    .font(.headline)
                    .foregroundStyle(.white)
                Text("Connect to your server")
                    .font(.caption)
                    .foregroundStyle(Color.fsMuted)
                TextField("https://demo.filestash.app", text: $url)
                    .textFieldStyle(.roundedBorder)
                if let error = state.error {
                    Text(error).foregroundStyle(Color.fsError).font(.caption)
                }
                HStack {
                    Button("Quit") { NSApplication.shared.terminate(nil) }
                    Spacer()
                    Button("Connect") {
                        let base = url.contains("://") ? url : "https://\(url)"
                        server = base.hasSuffix("/") ? String(base.dropLast()) : base
                    }
                    .keyboardShortcut(.defaultAction)
                    .disabled(url.isEmpty)
                }
            }
            .padding(16)
            .frame(width: 300)
            .background(Color.fsBackground)
        }
    }
}
