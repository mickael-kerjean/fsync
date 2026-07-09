import SwiftUI

struct LoginView: View {
    @EnvironmentObject var state: AppState
    @State private var url: String
    @State private var server: String?
    @FocusState private var fieldFocused: Bool

    init() {
        _url = State(initialValue: SessionStore.lastKnown()?.serverUrl ?? "")
    }

    var body: some View {
        NavigationStack {
            if let server {
                LoginWebView(base: server) { token in
                    Task { await state.connect(server: server, token: token) }
                }
                .ignoresSafeArea(edges: .bottom)
                .navigationTitle("Sign In")
                .navigationBarTitleDisplayMode(.inline)
                .toolbar {
                    ToolbarItem(placement: .topBarLeading) {
                        Button("Cancel") { self.server = nil }
                    }
                }
            } else {
                connectForm
            }
        }
        .tint(.fsAccent)
    }

    private var connectForm: some View {
        VStack(spacing: 18) {
            Text("Filestash")
                .font(.largeTitle.bold())

            HStack(spacing: 10) {
                Image(systemName: "globe")
                    .foregroundStyle(.secondary)
                TextField("demo.filestash.app", text: $url)
                    .textContentType(.URL)
                    .keyboardType(.URL)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .submitLabel(.go)
                    .focused($fieldFocused)
                    .onSubmit(connect)
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 14)
            .background(Color(.secondarySystemGroupedBackground))
            .clipShape(RoundedRectangle(cornerRadius: 12, style: .continuous))

            Button(action: connect) {
                Text("Connect")
                    .fontWeight(.semibold)
                    .frame(maxWidth: .infinity)
            }
            .buttonStyle(.borderedProminent)
            .controlSize(.large)
            .disabled(url.isEmpty)
        }
        .padding(.horizontal, 24)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color(.systemGroupedBackground).ignoresSafeArea())
    }

    private func connect() {
        guard !url.isEmpty else { return }
        fieldFocused = false
        let base = url.contains("://") ? url : "https://\(url)"
        server = base.hasSuffix("/") ? String(base.dropLast()) : base
    }
}
