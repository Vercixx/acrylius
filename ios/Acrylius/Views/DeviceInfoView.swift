#if canImport(SwiftUI)

import SwiftUI

/// What this phone is, and what it can do — the screen equivalent of
/// `acryliusctl status`.
///
/// Capabilities are listed in both directions because they are not the same
/// thing. A phone advertises every capability it knows about, but it can only
/// *serve* the few its hardware supports; the rest it can only ask a computer
/// for. Showing one list would suggest a symmetry that is not there.
struct DeviceInfoView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        List {
            Section("Identity") {
                LabeledContent("Status", value: model.status)
                LabeledContent("Device ID") {
                    Text(model.deviceId).font(.caption.monospaced())
                }
            }

            Section {
                Text(model.fingerprint)
                    .font(.caption.monospaced())
                    .textSelection(.enabled)
            } header: {
                Text("Fingerprint")
            } footer: {
                Text("A computer shows this while pairing. They should match.")
            }

            Section {
                ForEach(capabilities, id: \.name) { cap in
                    LabeledContent(cap.title) {
                        Text(cap.direction).font(.caption).foregroundStyle(.secondary)
                    }
                }
            } header: {
                Text("Capabilities")
            } footer: {
                Text(
                    "\"Send and receive\" is one this phone can carry out itself. "
                        + "\"Send only\" is one it can ask a computer to do but cannot do here — "
                        + "a phone has no desktop session to lock, and runs nothing on request."
                )
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
                Text("Widget")
            } footer: {
                // Nothing in the app reports this failing, because nothing in
                // the app fails: it falls back to its own container and works.
                // Only the widget notices, and a widget cannot tell anyone.
                Text(
                    SharedContainer.isShared
                        ? "The widget can see this app's data. A group name you did not "
                            + "choose is normal: the installer rewrites it to its own team."
                        : "This build has no App Group, so the widget will stay empty."
                )
            }
        }
        .navigationTitle("This device")
        .navigationBarTitleDisplayMode(.inline)
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
