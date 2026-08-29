#if canImport(SwiftUI)

import SwiftUI
#if canImport(UIKit)
import UIKit
#endif

/// Pick a computer. That is the whole thing.
///
/// There is nothing to type and nothing to scan. Tapping a machine runs the
/// handshake, six digits appear here and on that machine's screen, and a person
/// at each end says whether they match. This screen used to open on an empty
/// text field and an IP address to type on a phone keyboard; then on a field for
/// an eight-character code read off the other screen. Both were asking somebody
/// to be at the computer already, which is the one thing pairing a phone to a
/// computer across the room cannot assume.
struct PairView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @State private var addr = ""
    @State private var manual = false

    /// Machines seen recently enough to still be worth offering.
    ///
    /// mDNS resolves a service once and then says nothing until something
    /// changes, so an entry is never withdrawn — a computer switched off an
    /// hour ago would otherwise sit here looking available for the life of the
    /// app.
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
                                // The whole gesture. Everything a pairing needs
                                // comes from the handshake this starts.
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
                            // A machine already showing somebody else six
                            // digits will refuse this one, so offering the tap
                            // would only produce a failure to explain.
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

                Section(isExpanded: $manual) {
                    TextField("192.168.1.10:1971", text: $addr)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .keyboardType(.URL)
                    Button("Pair with this address") {
                        Task { await model.pair(at: addr) }
                    }
                    .disabled(addr.isEmpty)
                } header: {
                    Text("Enter an address")
                } footer: {
                    // An address is a route, not a secret. Typing one still
                    // ends at the same six digits, so this is a convenience for
                    // a network mDNS cannot cross rather than a way around the
                    // comparison.
                    Text("Only needed if this \(deviceKind()) cannot find the computer by itself.")
                }
            }
            .formStyle(.grouped)
            .navigationTitle("Pair another device")
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
            }
            // The sheet closes itself when the digits arrive: `RootView` puts
            // `ConfirmPairingView` up on `pairingSas`, and two sheets at once
            // would leave the confirmation behind this one.
            .onChange(of: model.pairingSas) {
                if model.pairingSas != nil { dismiss() }
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

/// Both ends show the same six digits. The user compares them.
///
/// **This is the security boundary.** Pairing runs plain `XX` with no shared
/// secret, so anybody who can reach a device can complete a handshake with it —
/// and an attacker who can relay traffic completes two, one with each side.
/// What gives that away is that the two handshake hashes differ, so the digits
/// differ, and a person notices. Nothing else is checking.
///
/// So: never auto-confirm, never make "they match" the easier press by accident,
/// and never let this be dismissed by a swipe. A person who did not look has not
/// authenticated anything.
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
