#if canImport(SwiftUI)

import SwiftUI

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

    var body: some View {
        List {
            Section {
                LabeledContent("Status", value: peer.reachable ? "Connected" : "Not connected")
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
                } footer: {
                    Text("Unlocking asks for Face ID. Locking does not.")
                }
            }

            if features.canWake, !peer.reachable {
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
                    Text("Aims at the last known address first. Your router needs a "
                         + "reservation and a static ARP entry for a sleeping machine.")
                }
            }

            if features.canRunCommands {
                Section("Commands") {
                    ForEach(features.commands, id: \.id) { command in
                        TaskButton(command.name) { await model.run(command, on: peer); return true }
                    }
                }
            }

            Section("Clipboard") {
                TaskButton("Get from \(peer.name)") { await model.fetchClipboard(peer); return true }
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
        .task { if peer.reachable { await model.refreshSession(peer) } }
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
            Text("Pairing has to be done again from both ends to undo this.")
        }
    }
}

#endif
