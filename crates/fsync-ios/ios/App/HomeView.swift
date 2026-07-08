import SwiftUI

struct HomeView: View {
    @EnvironmentObject var state: AppState
    @State private var connected: Bool?
    @State private var checkNow = 0

    var body: some View {
        VStack {
            Spacer()
            StatusCircle(connected: connected) { checkNow += 1 }
            Text(label)
                .font(.headline)
                .foregroundStyle(.white)
                .padding(.top, 16)
            if let account {
                Text(account)
                    .font(.caption)
                    .foregroundStyle(Color.fsMuted)
                    .padding(.top, 2)
            }
            Spacer()
            Button {
                Task { await DomainManager.open() }
            } label: {
                Text("Open in Files")
                    .frame(maxWidth: .infinity, minHeight: 44)
                    .foregroundStyle(Color.fsEmphasisText)
            }
            .background(Color.fsEmphasis)
            .clipShape(RoundedRectangle(cornerRadius: 6))
            Button("Log out") {
                Task { await state.logout() }
            }
            .foregroundStyle(Color.fsMuted)
            .padding(.top, 8)
        }
        .padding(24)
        .background(Color.fsBackground.ignoresSafeArea())
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
                Circle().fill(fill.opacity(0.14)).frame(width: 168, height: 168)
                Circle().fill(fill.opacity(0.22)).frame(width: 146, height: 146)
                Circle().fill(fill).frame(width: 124, height: 124)
                Image(systemName: "power")
                    .font(.system(size: 44, weight: .semibold))
                    .foregroundStyle(connected == nil ? Color.fsMuted : Color.fsBackground)
            }
        }
        .buttonStyle(.plain)
    }
}
