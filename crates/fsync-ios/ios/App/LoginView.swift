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
            LoginWebView(base: server) { token in
                Task { await state.connect(server: server, token: token) }
            }
            .ignoresSafeArea(edges: .bottom)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Back") { self.server = nil }
                }
            }
        } else {
            VStack(spacing: 12) {
                Spacer()
                Text("Filestash")
                    .font(.largeTitle.weight(.semibold))
                    .foregroundStyle(.white)
                Text("Connect to your server")
                    .font(.subheadline)
                    .foregroundStyle(Color.fsMuted)
                TextField("https://demo.filestash.app", text: $url)
                    .keyboardType(.URL)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .padding(14)
                    .background(Color.white.opacity(0.06))
                    .clipShape(RoundedRectangle(cornerRadius: 6))
                    .foregroundStyle(.white)
                Button {
                    let base = url.contains("://") ? url : "https://\(url)"
                    server = base.hasSuffix("/") ? String(base.dropLast()) : base
                } label: {
                    Text("Connect")
                        .frame(maxWidth: .infinity, minHeight: 44)
                        .foregroundStyle(Color.fsBackground)
                }
                .background(Color.fsPrimary)
                .clipShape(RoundedRectangle(cornerRadius: 6))
                .disabled(url.isEmpty)
                .opacity(url.isEmpty ? 0.4 : 1)
                Spacer()
                Spacer()
            }
            .padding(24)
            .background(Color.fsBackground.ignoresSafeArea())
        }
    }
}
