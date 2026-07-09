import SwiftUI

struct HomeView: View {
    @EnvironmentObject var state: AppState

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            MenuRow("Open") { MountManager.open() }
            MenuRow("Log out") { Task { await state.logout() } }
            Divider().padding(.vertical, 2)
            MenuRow("Quit") { NSApplication.shared.terminate(nil) }
        }
        .padding(6)
        .frame(width: 180)
    }
}

private struct MenuRow: View {
    let title: String
    let action: () -> Void
    @State private var hover = false

    init(_ title: String, action: @escaping () -> Void) {
        self.title = title
        self.action = action
    }

    var body: some View {
        Button(action: action) {
            Text(title)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.vertical, 4)
                .padding(.horizontal, 8)
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .background(hover ? Color.accentColor.opacity(0.15) : Color.clear)
        .clipShape(RoundedRectangle(cornerRadius: 4))
        .onHover { hover = $0 }
    }
}
