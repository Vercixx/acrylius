#if canImport(SwiftUI)

import SwiftUI
#if canImport(UIKit)
import UIKit
#endif

/// The identifiers and introspection that used to sit on the main screen.
///
/// Nothing here is wrong to show — it is how an install that half-worked gets
/// diagnosed — but none of it is something a person uses the app *for*, and it
/// occupied half of the only screen about this phone.
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
                    // Both sides spelled as Color: `.secondary` on its own is a
                    // HierarchicalShapeStyle and the two do not unify.
                    Text(SharedContainer.isShared ? "Shared" : "Not shared")
                        .foregroundStyle(SharedContainer.isShared ? Color.secondary : Color.red)
                }
                // What the installer actually granted this build. It decides at
                // install time whether the app and its extension get one App ID
                // or two, and rewrites identifiers either way; both arrangements
                // work and they fail differently, so the useful thing is being
                // able to see which one happened rather than reasoning about
                // what it probably did.
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
                // Nothing in the app reports this failing, because nothing in
                // the app fails: it falls back to its own container and works.
                // Only the widget notices, and a widget cannot tell anyone.
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
