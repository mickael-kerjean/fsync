import SwiftUI

@main
struct FilestashApp: App {
    @StateObject private var state = AppState()

    var body: some Scene {
        WindowGroup {
            if state.session == nil {
                LoginView().environmentObject(state)
            } else {
                HomeView().environmentObject(state)
            }
        }
    }
}
