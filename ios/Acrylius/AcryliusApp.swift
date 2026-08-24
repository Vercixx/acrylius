import SwiftUI

@main
struct AcryliusApp: App {
    @State private var model = AppModel()

    var body: some Scene {
        WindowGroup {
            DeviceListView()
                .environment(model)
                .task { await model.start() }
        }
    }
}
