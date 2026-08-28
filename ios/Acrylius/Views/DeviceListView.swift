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

    /// The device a confirmation is currently about.
    ///
    /// Held here rather than in the row, which is the whole of a bug that has
    /// been in the app since M1: swiping a row and tapping Forget opened a
    /// `confirmationDialog` *attached to that row*, and the swipe had already
    /// begun removing the row. The dialog went with it about half a second
    /// later, before anyone could answer, and the device stayed paired while
    /// the list no longer listed it — which is why it came back on relaunch.
    /// A dialog has to outlive the thing it is asking about.
    @State private var forgetting: FfiPeer?

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
                            .swipeActions {
                                Button("Forget", role: .destructive) { forgetting = peer }
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
            // On the List, which survives a row going away.
            .confirmationDialog(
                forgetting.map { "Forget \($0.name)?" } ?? "",
                isPresented: Binding(
                    get: { forgetting != nil },
                    set: { if !$0 { forgetting = nil } }
                ),
                titleVisibility: .visible,
                presenting: forgetting
            ) { peer in
                Button("Forget", role: .destructive) {
                    Task { await model.forget(peer) }
                    forgetting = nil
                }
                Button("Cancel", role: .cancel) { forgetting = nil }
            } message: { peer in
                Text("You'll need to pair \(peer.name) with this \(UIDevice.current.model) again.")
            }
        }
    }
}

private struct PeerRow: View {
    @Environment(AppModel.self) private var model
    let peer: FfiPeer

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
