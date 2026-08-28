#if canImport(SwiftUI)

import SwiftUI
#if canImport(UIKit)
import UIKit
#endif

/// One paired computer.
///
/// Every section here is conditional on something the peer announced. A machine
/// with no commands configured sends no catalogue and gets no Commands section,
/// so the screen never offers something that cannot work.
struct DeviceView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    let peer: FfiPeer

    @State private var pasted = ""
    @State private var confirmingForget = false

    private var features: PeerFeatures { model.catalog[peer.deviceId] }

    /// How a link kind reads to a person. `nil` when nothing is carrying the
    /// session, since "Not connected" already says that and a second line
    /// saying it again is noise.
    private static func carrying(_ kind: FfiTransportKind?) -> String? {
        switch kind {
        case .tcpLan: "Wi-Fi"
        case .bleGatt: "Bluetooth"
        case .unixLoopback: "This device"
        case let .custom(name): name
        case nil: nil
        }
    }

    var body: some View {
        List {
            Section {
                LabeledContent("Status", value: peer.reachable ? "Connected" : "Not connected")
                // Which radio is carrying this. A second transport is only
                // useful if it takes over quietly, and something that takes
                // over quietly is indistinguishable from something broken
                // unless it says so somewhere.
                if let over = Self.carrying(peer.transport) {
                    LabeledContent("Over", value: over)
                }
                if !peer.reachable {
                    TaskButton("Connect") { await model.connect(peer); return true }
                }
            }

            if let session = features.session {
                Section {
                    LabeledContent("Screen", value: session.locked ? "Locked" : "Unlocked")
                    TaskButton("Lock") { await model.lock(peer) }
                        .disabled(session.locked)
                    TaskButton("Unlock") { await model.unlock(peer) }
                        .disabled(!session.locked)
                } header: {
                    Text("Session")
                }
            }

            MediaSection(peer: peer)

            // Only while there is a session. An offer travels over it, and a
            // picker that leads to "unreachable" is worse than no picker.
            if peer.reachable {
                SendFileSection(peer: peer)
            }

            // Whenever this phone is not talking to the computer, which is
            // exactly when waking it means something — and it must not depend
            // on the live catalogue, which is empty until a session opens. A
            // machine that is asleep will never fill it in, so the answer comes
            // off disk: the computer handed these over while it was awake.
            if !peer.reachable, WakeTargets.load(for: peer.deviceId) != nil {
                Section {
                    TaskButton("Wake up") {
                        let sent = await model.wake(peer)
                        if !sent {
                            model.lastError = "Could not send a wake packet."
                        }
                        return sent
                    }
                } header: {
                    Text("Power")
                } footer: {
                    Text("This will not work if your network blocks Wake on LAN.")
                }
            }

            if features.canRunCommands {
                Section("Commands") {
                    ForEach(features.commands, id: \.id) { command in
                        TaskButton(command.name) { await model.run(command, on: peer); return true }
                    }
                }
            }

            Section {
                TaskButton("Get remote clipboard") { await model.fetchClipboard(peer); return true }
                if let value = features.clipboard {
                    Text(value)
                        .font(.callout)
                        .textSelection(.enabled)
                }
                // A PasteButton, not a read of UIPasteboard.
                //
                // Since iOS 16, reading the pasteboard programmatically raises a
                // system prompt whenever the content came from another app.
                // Only this control, the paste menu, and the keyboard shortcut
                // are exempt, which is why phone-to-computer sync is a button
                // the user presses and not something that happens quietly.
                PasteButton(payloadType: String.self) { strings in
                    guard let text = strings.first else { return }
                    Task { await model.pushClipboard(text, to: peer) }
                }
                .labelStyle(.titleAndIcon)
            } header: {
                Text("Clipboard")
            } footer: {
                Text("Pushes local clipboard to \(peer.name)")
            }

            if let error = features.lastError ?? model.lastError {
                Section {
                    Text(error).font(.footnote).foregroundStyle(.secondary)
                }
            }

            Section {
                Button("Forget this device", role: .destructive) {
                    confirmingForget = true
                }
            } footer: {
                Text(peer.fingerprint).font(.caption2.monospaced())
            }
        }
        .navigationTitle(peer.name)
        .task {
            guard peer.reachable else { return }
            await model.refreshSession(peer)
            await model.refreshMedia(peer)
        }
        .confirmationDialog(
            "Forget \(peer.name)?",
            isPresented: $confirmingForget,
            titleVisibility: .visible
        ) {
            Button("Forget", role: .destructive) {
                Task {
                    await model.forget(peer)
                    // This screen is about a device that no longer exists, so
                    // going back is part of the action rather than something to
                    // leave the user to work out.
                    dismiss()
                }
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("You'll need to pair \(peer.name) with this \(UIDevice.current.model) again.")
        }
    }
}

#endif
