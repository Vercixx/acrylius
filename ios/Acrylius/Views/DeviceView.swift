#if canImport(SwiftUI)

import SwiftUI
#if canImport(UIKit)
import UIKit
#endif

/// How a link kind reads to a person; `nil` when nothing carries the session.
func carrying(_ kind: FfiTransportKind?) -> String? {
    switch kind {
    case .tcpLan: "Wi-Fi"
    case .bleGatt: "Bluetooth"
    case .unixLoopback: "This device"
    case let .custom(name): name
    case nil: nil
    }
}

/// One paired computer. Every section is conditional on something the peer
/// announced, so the screen never offers what cannot work.
struct DeviceView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    let peer: FfiPeer

    @State private var pasted = ""
    @State private var confirmingForget = false

    private var features: PeerFeatures { model.catalog[peer.deviceId] }

    var body: some View {
        List {
            // No Connect button: the core already dials on discovery.
            Section {
                switch peer.state {
                case .reachable:
                    LabeledContent("Status", value: "Connected")
                case .connecting:
                    LabeledContent("Status") {
                        HStack(spacing: 6) {
                            ProgressView().controlSize(.small)
                            Text("Connecting…").foregroundStyle(.secondary)
                        }
                    }
                case .unreachable:
                    LabeledContent("Status", value: "Not connected")
                    TaskButton("Try again") { await model.retry(peer) }
                }
                if let over = carrying(peer.transport) {
                    LabeledContent("Transport", value: over)
                }
            } footer: {
                if peer.state == .unreachable {
                    VStack(alignment: .leading, spacing: 4) {
                        if let trouble = peer.trouble {
                            Text(trouble)
                        }
                        Text("Still trying every few seconds.")
                    }
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

            if features.canTouchpad, peer.reachable {
                Section {
                    NavigationLink("Use as touchpad") {
                        TouchpadView(peer: peer)
                    }
                }
            }

            // Read off disk, not the live catalogue: a sleeping machine never
            // fills the catalogue in.
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
                TaskButton("Get remote clipboard") { await model.fetchClipboard(peer) }
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
        // An alert, for the reason spelled out in `DeviceListView`: a
        // confirmation dialog wants something to anchor to and this one had
        // nothing to point at.
        .alert("Forget \(peer.name)?", isPresented: $confirmingForget) {
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
