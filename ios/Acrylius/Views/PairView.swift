#if canImport(SwiftUI)

import SwiftUI
#if canImport(UIKit)
import UIKit
#endif

/// Pick a computer. Tapping one runs the handshake; six digits appear here
/// and on that machine's screen, and a person at each end says whether they match.
struct PairView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @State private var addr = ""
    /// Set once a pairing is requested, so an unrelated error elsewhere
    /// doesn't close this screen while it's still being read.
    @State private var asked = false

    /// mDNS never withdraws an entry once resolved, so this filters out
    /// machines not seen recently.
    private var fresh: [AppModel.Nearby] {
        model.nearby.filter { Date().timeIntervalSince($0.seen) < 300 }
    }

    var body: some View {
        NavigationStack {
            Form {
                if fresh.isEmpty {
                    Section {
                        ContentUnavailableView(
                            "No computers found",
                            systemImage: "desktopcomputer.trianglebadge.exclamationmark",
                            description: Text(
                                "Make sure acryliusd is running and that this \(deviceKind()) is on the same Wi-Fi network."
                            )
                        )
                    }
                } else {
                    Section {
                        ForEach(fresh) { pc in
                            Button {
                                asked = true
                                Task { await model.pair(at: pc.addr, transport: pc.transport) }
                            } label: {
                                HStack {
                                    VStack(alignment: .leading) {
                                        Text(pc.name).foregroundStyle(.primary)
                                        Text(pc.addr)
                                            .font(.caption.monospaced())
                                            .foregroundStyle(.secondary)
                                    }
                                    Spacer()
                                    if pc.pairing {
                                        Text("busy")
                                            .font(.caption)
                                            .foregroundStyle(.secondary)
                                    }
                                }
                            }
                            // A machine mid-handshake with someone else will refuse this one.
                            .disabled(pc.pairing)
                        }
                    } header: {
                        Text("On this network")
                    } footer: {
                        Text(
                            "Tap a computer to pair with it. Both screens will show the same six digits."
                        )
                    }
                }

                // Not `Section(isExpanded:)`: it has no initializer taking a footer.
                Section {
                    TextField("192.168.1.10:1971", text: $addr)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .keyboardType(.URL)
                    Button("Pair with this address") {
                        asked = true
                        Task { await model.pair(at: addr) }
                    }
                    .disabled(addr.isEmpty)
                } header: {
                    Text("Enter an address")
                } footer: {
                    Text("Only needed if this \(deviceKind()) cannot find the computer by itself.")
                }
            }
            .navigationTitle("Pair another device")
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
            }
            // Closes itself when digits arrive: `RootView` presents `ConfirmPairingView`
            // on `pairingSas`, and two sheets at once would hide the confirmation.
            .onChange(of: model.pairingSas) {
                if model.pairingSas != nil { dismiss() }
            }
            // And on refusal: `RootView`'s error alert is behind this sheet
            // and invisible until it closes.
            .onChange(of: model.lastErrorAt) {
                if asked { dismiss() }
            }
        }
    }

    private func deviceKind() -> String {
        #if canImport(UIKit)
        return UIDevice.current.model
        #else
        return "device"
        #endif
    }
}

/// Both ends show the same six digits and the user compares them.
///
/// This is the security boundary: pairing has no shared secret, so a relaying
/// attacker can complete a handshake with each side, but the two SAS codes
/// would differ. Never auto-confirm and never allow swipe-to-dismiss — an
/// unread screen authenticates nothing.
struct ConfirmPairingView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        VStack(spacing: 24) {
            Text(model.pairingPeerName ?? "A device")
                .font(.title2)
            Text("wants to pair")
                .foregroundStyle(.secondary)

            Text(model.pairingSas ?? "")
                .font(.system(.largeTitle, design: .monospaced))
                .fontWeight(.semibold)

            Text("Confirm this matches what the other device shows.")
                .font(.footnote)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)

            if let fp = model.pairingPeerFingerprint {
                Text(fp)
                    .font(.caption2.monospaced())
                    .foregroundStyle(.tertiary)
                    .lineLimit(2)
                    .truncationMode(.middle)
            }

            HStack {
                Button("They differ", role: .destructive) {
                    Task { await model.confirmPairing(false) }
                }
                .buttonStyle(.bordered)
                Button("They match") {
                    Task { await model.confirmPairing(true) }
                }
                .buttonStyle(.borderedProminent)
            }
        }
        .padding()
        .presentationDetents([.medium])
        .interactiveDismissDisabled()
    }
}

#endif
