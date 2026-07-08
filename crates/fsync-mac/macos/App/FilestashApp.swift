import SwiftUI

@main
struct FilestashApp: App {
    @StateObject private var state = AppState()

    var body: some Scene {
        MenuBarExtra("Filestash", systemImage: state.session == nil ? "externaldrive" : "externaldrive.fill") {
            if state.session == nil {
                LoginView().environmentObject(state)
            } else {
                HomeView().environmentObject(state)
            }
        }
        .menuBarExtraStyle(.window)
    }
}
