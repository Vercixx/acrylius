#if canImport(SwiftUI)

import SwiftUI
#if canImport(UIKit)
import UIKit
#endif

/// The computers this phone is paired with.
///
/// It owns nothing but the list now. Offers moved to Files and this phone's own
/// details moved to Status, which leaves one screen answering one question.
struct DeviceListView: View {
    @Environment(AppModel.self) private var model
    @Binding var path: [String]
    @Binding var showPair: Bool

    var body: some View {
        NavigationStack(path: $path) {
            List {
                if model.peers.isEmpty {
                    Section {
                        ContentUnavailableView(
                            "No devices",
                            systemImage: "desktopcomputer",
                            description: Text("Tap ＋ to get started.")
                        )
                    }
                } else {
                    Section {
                        ForEach(model.peers, id: \.deviceId) { peer in
                            NavigationLink(value: peer.deviceId) {
                                PeerRow(peer: peer)
                            }
                        }
                    }
                }

                // Bluetooth is asked for here rather than on a debug screen.
                //
                // `CBCentralManager` prompts the moment it is built, so the
                // prompt has always been behind a tap. It used to be behind the
                // Bluetooth diagnostics screen — which was fine while that
                // screen was one tap from the root, and is not now that it is
                // three taps into Status › Debug. A phone that is never granted
                // Bluetooth simply stops working when Wi-Fi goes away, with
                // nothing anywhere saying why.
                if model.ble.awaitingPermission {
                    Section {
                        Button("Turn on Bluetooth", systemImage: "dot.radiowaves.left.and.right") {
                            model.startBluetooth()
                        }
                    } footer: {
                        Text("Lets this \(UIDevice.current.model) reach a computer when Wi-Fi is not available.")
                    }
                }
            }
            .navigationTitle("Devices")
            .navigationDestination(for: String.self) { deviceId in
                if let peer = model.peers.first(where: { $0.deviceId == deviceId }) {
                    DeviceView(peer: peer)
                } else {
                    // A widget can outlive the pairing it was made for.
                    ContentUnavailableView(
                        "Not paired",
                        systemImage: "desktopcomputer.trianglebadge.exclamationmark",
                        description: Text("This device is no longer paired with this \(UIDevice.current.model).")
                    )
                }
            }
            .toolbar {
                Button("Pair", systemImage: "plus") { showPair = true }
            }
        }
    }
}

private struct PeerRow: View {
    @Environment(AppModel.self) private var model
    let peer: FfiPeer

    @State private var confirmingForget = false

    var body: some View {
        HStack {
            VStack(alignment: .leading, spacing: 2) {
                Text(peer.name)
                Text(summary).font(.caption).foregroundStyle(.secondary)
            }
            Spacer()
            // Three states, not two. A peer part way through a handshake used
            // to show the same grey dot as one that had given up, which is how
            // a connection that was working perfectly well read as broken.
            switch peer.state {
            case .reachable:
                Circle().fill(.green).frame(width: 8, height: 8)
            case .connecting:
                ProgressView().controlSize(.small)
            case .unreachable:
                Circle().fill(.secondary).frame(width: 8, height: 8)
            }
        }
        .swipeActions {
            Button("Forget", role: .destructive) { confirmingForget = true }
        }
        .confirmationDialog(
            "Forget \(peer.name)?",
            isPresented: $confirmingForget,
            titleVisibility: .visible
        ) {
            Button("Forget", role: .destructive) { Task { await model.forget(peer) } }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("You'll need to pair \(peer.name) with this \(UIDevice.current.model) again.")
        }
    }

    /// What this peer can do, from what it announced.
    private var summary: String {
        let features = model.catalog[peer.deviceId]
        switch peer.state {
        case .connecting:
            return "Connecting…"
        case .unreachable:
            if features.canWake { return "Asleep or away, can be woken" }
            return "Not connected"
        case .reachable:
            var parts: [String] = []
            if let session = features.session { parts.append(session.locked ? "Locked" : "Unlocked") }
            if features.canRunCommands { parts.append("\(features.commands.count) commands") }
            return parts.isEmpty ? peer.platform : parts.joined(separator: " · ")
        }
    }
}

#endif
