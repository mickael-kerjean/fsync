import Foundation
import SwiftUI

struct HomeView: View {
    @EnvironmentObject var state: AppState
    @State private var connected: Bool?
    @State private var checkNow = 0

    var body: some View {
        NavigationStack {
            VStack(spacing: 0) {
                Spacer()

                StatusCircle(connected: connected) { checkNow += 1 }

                Text(label)
                    .font(.title3.weight(.semibold))
                    .padding(.top, 20)

                if !host.isEmpty {
                    Text(host)
                        .font(.footnote)
                        .foregroundStyle(.secondary)
                        .padding(.top, 4)
                }

                Spacer()

                Button {
                    Task { await DomainManager.open() }
                } label: {
                    Text("Browse")
                        .fontWeight(.semibold)
                        .frame(maxWidth: .infinity)
                }
                .buttonStyle(.borderedProminent)
                .controlSize(.large)
            }
            .padding(24)
            .background(Color(.systemGroupedBackground).ignoresSafeArea())
            .toolbar {
                ToolbarItem(placement: .topBarTrailing) {
                    Button(role: .destructive) {
                        Task { await state.logout() }
                    } label: {
                        Image(systemName: "rectangle.portrait.and.arrow.right")
                    }
                    .accessibilityLabel("Log Out")
                }
            }
        }
        .tint(.fsAccent)
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

    private var host: String {
        guard let session = state.session else { return "" }
        return URL(string: session.serverUrl)?.host ?? session.serverUrl
    }

    private var label: String {
        switch connected {
        case .some(true): return "Connected"
        case .some(false): return "Offline"
        case .none: return "Connecting…"
        }
    }
}

/// Large centered connection indicator — green when reachable, red when not.
/// Tapping it re-checks. When connected, the rings and a soft halo breathe
/// (driven per-frame by TimelineView) so the screen reads as live, not frozen.
private struct StatusCircle: View {
    let connected: Bool?
    let onTap: () -> Void

    private var fill: Color {
        switch connected {
        case .some(true): return .fsConnected
        case .some(false): return .fsOffline
        case .none: return Color(.systemGray4)
        }
    }

    private var glyph: Color {
        connected == nil ? Color(.systemGray) : .fsGlyph
    }

    var body: some View {
        Button(action: onTap) {
            TimelineView(.animation(minimumInterval: 1.0 / 30.0, paused: connected != true)) { timeline in
                let g = glow(at: timeline.date)
                ZStack {
                    Circle()
                        .fill(fill.opacity(0.14 + 0.12 * g))
                        .frame(width: 168, height: 168)
                    Circle()
                        .fill(fill.opacity(0.22 + 0.14 * g))
                        .frame(width: 146, height: 146)
                    Circle()
                        .fill(fill)
                        .frame(width: 124, height: 124)
                        .shadow(color: fill.opacity(0.7 * g), radius: 12 + 20 * g)
                    Image(systemName: "power")
                        .font(.system(size: 44, weight: .semibold))
                        .foregroundStyle(glyph)
                }
            }
        }
        .buttonStyle(.plain)
    }

    /// 0…1 breathing value; a sine over a 1.8s period. Flat when not connected.
    private func glow(at date: Date) -> Double {
        guard connected == true else { return 0 }
        let t = date.timeIntervalSinceReferenceDate
        return (sin(t * 2 * .pi / 1.8) + 1) / 2
    }
}
