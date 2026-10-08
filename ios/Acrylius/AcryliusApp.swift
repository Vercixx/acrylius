import SwiftUI

@main
struct AcryliusApp: App {
    @UIApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
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

final class AppDelegate: NSObject, UIApplicationDelegate {
    func application(
        _ application: UIApplication,
        supportedInterfaceOrientationsFor window: UIWindow?
    ) -> UIInterfaceOrientationMask {
        UIDevice.current.userInterfaceIdiom == .pad ? .all : AppOrientation.allowed
    }
}
