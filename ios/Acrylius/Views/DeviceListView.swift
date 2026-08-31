#if canImport(SwiftUI)

import SwiftUI
#if canImport(UIKit)
import UIKit
#endif

/// The computers this phone is paired with.
struct DeviceListView: View {
    @Environment(AppModel.self) private var model
    @Binding var path: [String]
    @Binding var showPair: Bool

    /// Held here, not in the row: a dialog attached to a swiped row is torn
    /// down with it before anyone can answer.
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
                                Button("Forget", role: .destructive) {
                                    // Next runloop: presenting in the same
                                    // frame cuts off the swipe's close animation.
                                    Task { @MainActor in forgetting = peer }
                                }
                            }
                        }
                    }
                }

                // The Bluetooth permission prompt lives here, not three taps
                // deep in Status › Debug where nobody would ever grant it.
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
            // An alert on the List, not a `confirmationDialog` on the row: the
            // dialog wants an anchor and the swiped row is sliding away.
            .alert(
                forgetting.map { "Forget \($0.name)?" } ?? "",
                isPresented: Binding(
                    get: { forgetting != nil },
                    set: { if !$0 { forgetting = nil } }
                ),
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
