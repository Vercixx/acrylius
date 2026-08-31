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
                // On the root, not a tab: off-screen tabs get no lifecycle.
                .onChange(of: scenePhase) { was, now in
                    guard now == .active, was != .active else { return }
                    Task { await model.cameToForeground() }
                }
        }
    }
}
