#if canImport(SwiftUI)

import SwiftUI

/// One paired computer.
///
/// Every section here is conditional on something the peer announced. A machine
/// with no commands configured sends no catalogue and gets no Commands section,
/// so the screen never offers something that cannot work.
struct DeviceView: View {
    @Environment(AppModel.self) private var model
    let peer: FfiPeer

    @State private var busy = false
    @State private var pasted = ""

    private var features: PeerFeatures { model.catalog[peer.deviceId] }

    var body: some View {
        List {
            Section {
                LabeledContent("Status", value: peer.reachable ? "Connected" : "Not connected")
                if !peer.reachable {
                    Button("Connect") { Task { await model.connect(peer) } }
                }
            }

            if let session = features.session {
                Section {
                    LabeledContent("Screen", value: session.locked ? "Locked" : "Unlocked")
                    Button("Lock") { Task { await model.lock(peer) } }
                        .disabled(session.locked)
                    Button("Unlock") { Task { await model.unlock(peer) } }
                        .disabled(!session.locked)
                } header: {
                    Text("Session")
                } footer: {
                    Text("Unlocking asks for Face ID. Locking does not.")
                }
            }

            if features.canWake, !peer.reachable {
                Section("Power") {
                    Button("Wake up") {
                        Task {
                            busy = true
                            let sent = await model.wake(peer)
                            busy = false
                            if !sent { model.lastError = "Could not send a wake packet." }
                        }
                    }
                    .disabled(busy)
                } footer: {
                    Text("Aims at the last known address first. Your router needs a "
                         + "reservation and a static ARP entry for a sleeping machine.")
                }
            }

            if features.canRunCommands {
                Section("Commands") {
                    ForEach(features.commands, id: \.id) { command in
                        Button(command.name) { Task { await model.run(command, on: peer) } }
                    }
                }
            }

            Section("Clipboard") {
                Button("Get from \(peer.name)") { Task { await model.fetchClipboard(peer) } }
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
                    Task { await model.forget(peer) }
                }
            } footer: {
                Text(peer.fingerprint).font(.caption2.monospaced())
            }
        }
        .navigationTitle(peer.name)
        .task { if peer.reachable { await model.refreshSession(peer) } }
    }
}

#endif
