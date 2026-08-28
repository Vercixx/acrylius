import SwiftUI

@main
struct AcryliusApp: App {
    @State private var model = AppModel()
    @Environment(\.scenePhase) private var scenePhase

    var body: some Scene {
        WindowGroup {
            RootView()
                .environment(model)
                .task { await model.start() }
                // Here rather than on a view inside the app, because a tab
                // that is not on screen gets no lifecycle at all — and the
                // whole point is to run when the *process* comes back.
                .onChange(of: scenePhase) { was, now in
                    guard now == .active, was != .active else { return }
                    Task { await model.cameToForeground() }
                }
        }
    }
}
