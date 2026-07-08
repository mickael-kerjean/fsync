import SwiftUI

struct HomeView: View {
    @EnvironmentObject var state: AppState
    @State private var connected: Bool?
    @State private var checkNow = 0

    var body: some View {
        VStack(spacing: 8) {
            StatusCircle(connected: connected) { checkNow += 1 }
                .padding(.top, 12)
            Text(label)
                .font(.headline)
                .foregroundStyle(.white)
            if let account {
                Text(account).font(.caption).foregroundStyle(Color.fsMuted)
            }
            Divider().padding(.vertical, 4)
            Button {
                Task { await DomainManager.open() }
            } label: {
                Text("Open in Finder")
                    .frame(maxWidth: .infinity, minHeight: 28)
                    .foregroundStyle(Color.fsEmphasisText)
            }
            .buttonStyle(.plain)
            .background(Color.fsEmphasis)
            .clipShape(RoundedRectangle(cornerRadius: 5))
            HStack {
                Button("Log out") {
                    Task { await state.logout() }
                }
                .foregroundStyle(Color.fsMuted)
                Spacer()
                Button("Quit") { NSApplication.shared.terminate(nil) }
            }
        }
        .padding(16)
        .frame(width: 250)
        .background(Color.fsBackground)
        .task(id: checkNow) {
            guard let session = state.session else { return }
            while !Task.isCancelled {
                let up = await Task.detached {
                    ping(url: session.serverUrl, insecure: session.insecure, token: session.token)
                }.value
                await MainActor.run { connected = up }
                try? await Task.sleep(for: .seconds(10))
            }
        }
    }

    private var label: String {
        switch connected {
        case .some(true): return "Connected"
        case .some(false): return "Offline"
        case .none: return "Connecting…"
        }
    }

    private var account: String? {
        guard let session = state.session else { return nil }
        let host = URL(string: session.serverUrl)?.host ?? session.serverUrl
        return session.user.isEmpty ? host : "\(session.user) @ \(host)/\(session.storage)"
    }
}

private struct StatusCircle: View {
    let connected: Bool?
    let onTap: () -> Void

    private var fill: Color {
        switch connected {
        case .some(true): return .fsSuccess
        case .some(false): return .fsError
        case .none: return Color.white.opacity(0.12)
        }
    }

    var body: some View {
        Button(action: onTap) {
            ZStack {
                Circle().fill(fill.opacity(0.14)).frame(width: 96, height: 96)
                Circle().fill(fill.opacity(0.22)).frame(width: 82, height: 82)
                Circle().fill(fill).frame(width: 68, height: 68)
                Image(systemName: "power")
                    .font(.system(size: 24, weight: .semibold))
                    .foregroundStyle(connected == nil ? Color.fsMuted : Color.fsBackground)
            }
        }
        .buttonStyle(.plain)
    }
}
