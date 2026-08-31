#if canImport(SwiftUI)

import SwiftUI
#if canImport(UIKit)
import UIKit
#endif

/// Identifiers and introspection for diagnosing a half-working install.
struct DebugView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        List {
            Section {
                LabeledContent("Device ID") {
                    Text(model.deviceId).font(.caption.monospaced())
                }
            } header: {
                Text("Identity")
            }

            Section {
                Text(model.fingerprint)
                    .font(.caption.monospaced())
                    .textSelection(.enabled)
            } header: {
                Text("Fingerprint")
            }

            Section {
                LabeledContent("Widget data") {
                    // Both spelled as Color: bare `.secondary` is a
                    // HierarchicalShapeStyle and the branches do not unify.
                    Text(SharedContainer.isShared ? "Shared" : "Not shared")
                        .foregroundStyle(SharedContainer.isShared ? Color.secondary : Color.red)
                }
                // What the installer actually granted: one App ID or two, and
                // the two arrangements fail differently.
                ForEach(SharedContainer.report(), id: \.0) { row in
                    LabeledContent(row.0) {
                        Text(row.1)
                            .font(.caption.monospaced())
                            .multilineTextAlignment(.trailing)
                            .textSelection(.enabled)
                    }
                }
            } header: {
                Text("App Group")
            } footer: {
                // Only the widget notices a missing App Group; the app falls
                // back to its own container and works.
                Text(
                    SharedContainer.isShared
                        ? "The widget can see this app's data."
                        : "This build has no App Group, widget will not work."
                )
            }

            Section {
                NavigationLink("Bluetooth") { BluetoothView() }
            }
        }
        .navigationTitle("Debug")
        .navigationBarTitleDisplayMode(.inline)
    }
}

#endif
