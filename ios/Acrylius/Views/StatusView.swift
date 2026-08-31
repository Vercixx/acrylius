#if canImport(SwiftUI)

import SwiftUI
#if canImport(UIKit)
import UIKit
#endif

/// What this phone is and what it can do — the screen equivalent of
/// `acryliusctl status`.
///
/// Capabilities are shown per direction because they aren't symmetric: a
/// phone advertises every capability it knows, but can only serve the few
/// its hardware supports.
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
                    LabeledContent("Build") {
                        Text(BuildInfo.current.summary)
                            .font(.caption.monospaced())
                            .foregroundStyle(.secondary)
                            .textSelection(.enabled)
                    }
                    if let version = BuildInfo.current.version {
                        LabeledContent("Version", value: version)
                    }
                } header: {
                    Text("This build")
                } footer: {
                    Text(
                        BuildInfo.current.commit == nil
                            ? "Built outside CI, so there is no commit to name."
                            : "Compare this with the commit you expected before reinstalling."
                    )
                }

                Section {
                    NavigationLink("Debug") { DebugView() }
                } footer: {
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
        // Sourced from the core, which holds the plugin manifests and declared
        // effects, rather than duplicated here.
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
