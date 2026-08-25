#if canImport(SwiftUI)

import SwiftUI

struct DeviceListView: View {
    @Environment(AppModel.self) private var model
    @State private var showPair = false

    var body: some View {
        NavigationStack {
            List {
                Section {
                    if model.peers.isEmpty {
                        ContentUnavailableView(
                            "No devices",
                            systemImage: "desktopcomputer",
                            description: Text("Run `acryliusctl pair` on your PC, then tap ＋.")
                        )
                    } else {
                        ForEach(model.peers, id: \.deviceId) { peer in
                            NavigationLink {
                                DeviceView(peer: peer)
                            } label: {
                                PeerRow(peer: peer)
                            }
                        }
                    }
                }

                Section("This device") {
                    LabeledContent("Status", value: model.status)
                    LabeledContent("Fingerprint") {
                        Text(model.fingerprint)
                            .font(.caption.monospaced())
                            .lineLimit(2)
                            .truncationMode(.middle)
                    }
                }

                if let error = model.lastError {
                    Section {
                        Text(error).foregroundStyle(.secondary).font(.footnote)
                    }
                }
            }
            .navigationTitle("Acrylius")
            .toolbar {
                Button("Pair", systemImage: "plus") { showPair = true }
            }
            .sheet(isPresented: $showPair) { PairView() }
            .sheet(isPresented: .constant(model.pairingSas != nil)) { ConfirmPairingView() }
        }
    }
}

private struct PeerRow: View {
    @Environment(AppModel.self) private var model
    let peer: FfiPeer

    var body: some View {
        HStack {
            VStack(alignment: .leading) {
                Text(peer.name)
                Text(summary).font(.caption).foregroundStyle(.secondary)
            }
            Spacer()
            Circle()
                .fill(peer.reachable ? .green : .secondary)
                .frame(width: 8, height: 8)
        }
        .swipeActions {
            Button("Forget", role: .destructive) { Task { await model.forget(peer) } }
        }
    }

    /// What this peer can do, from what it announced.
    private var summary: String {
        let features = model.catalog[peer.deviceId]
        if !peer.reachable {
            return features.canWake ? "Asleep or away, can be woken" : "Not connected"
        }
        var parts: [String] = []
        if let session = features.session { parts.append(session.locked ? "Locked" : "Unlocked") }
        if features.canRunCommands { parts.append("\(features.commands.count) commands") }
        return parts.isEmpty ? peer.platform : parts.joined(separator: " · ")
    }
}

#endif
