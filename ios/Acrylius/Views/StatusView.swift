#if canImport(SwiftUI)

import SwiftUI
#if canImport(UIKit)
import UIKit
#endif

/// What this phone is and what it can do — the screen equivalent of
/// `acryliusctl status`.
///
/// Capabilities are listed in both directions because they are not the same
/// thing. A phone advertises every capability it knows about, but it can only
/// *serve* the few its hardware supports; the rest it can only ask a computer
/// for. Showing one list would suggest a symmetry that is not there.
///
/// Everything diagnostic moved behind the Debug row. Half of this screen used
/// to be identifiers and entitlement introspection — useful perhaps twice in
/// the life of an install, and in the way every other time.
struct StatusView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        NavigationStack {
            List {
                Section {
                    LabeledContent("Status", value: model.status.text)
                    if let activity = model.activity {
                        LabeledContent("Last", value: activity)
                    }
                    LabeledContent("Paired devices", value: "\(model.peers.count)")
                }

                Section {
                    ForEach(capabilities, id: \.name) { cap in
                        LabeledContent(cap.title) {
                            Text(cap.direction).font(.caption).foregroundStyle(.secondary)
                        }
                    }
                } header: {
                    Text("Capabilities")
                }

                Section {
                    NavigationLink("Debug") { DebugView() }
                } footer: {
                    // Named plainly, because the one thing here a person may
                    // genuinely need is the fingerprint, and it is now two taps
                    // away rather than one.
                    Text("Identifiers, the fingerprint, and what Bluetooth is doing.")
                }
            }
            .navigationTitle("This \(UIDevice.current.model)")
        }
    }

    private struct Capability {
        let name: String
        let title: String
        let direction: String
    }

    private var capabilities: [Capability] {
        // Which of these the phone can act on comes from the core, which
        // already knows: it holds the plugin manifests and the effects this
        // host declared. Repeating the list here would be a second copy to
        // keep in step, and the project exists to avoid those.
        let served = Set(model.capsServed)
        return Set(model.capsIn).union(model.capsOut).sorted().map { name in
            Capability(
                name: name,
                title: Self.pretty(name),
                direction: served.contains(name) ? "Send and receive" : "Send only"
            )
        }
    }

    private static func pretty(_ cap: String) -> String {
        // "org.acrylius.clipboard/1" -> "Clipboard"
        let base = cap.split(separator: "/").first.map(String.init) ?? cap
        let leaf = base.split(separator: ".").last.map(String.init) ?? base
        return leaf.prefix(1).uppercased() + leaf.dropFirst()
    }
}

#endif
