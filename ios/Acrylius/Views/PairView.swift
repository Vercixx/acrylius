#if canImport(SwiftUI)

import SwiftUI
#if canImport(UIKit)
import UIKit
#endif

/// Pick a computer, scan its code, or type both.
///
/// Three ways in, in the order they cost the person something. Discovery has
/// always known which machines are on the network; until M3 nothing could ask
/// it, so this screen opened on an empty text field and an IP address to type
/// on a phone keyboard.
struct PairView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @State private var code = ""
    @State private var addr = ""
    @State private var scanning = false
    @State private var scanTrouble: String?

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
                Section {
                    Button {
                        scanning = true
                    } label: {
                        Label("Scan the code on your computer", systemImage: "qrcode.viewfinder")
                    }
                } footer: {
                    if let scanTrouble {
                        Text(scanTrouble).foregroundStyle(.orange)
                    } else {
                        Text("Run `acryliusctl pair` on the computer to show one.")
                    }
                }

                if !fresh.isEmpty {
                    Section {
                        ForEach(fresh) { pc in
                            Button {
                                // Fills the form rather than pairing outright:
                                // the code is the pre-shared key and discovery
                                // does not carry it, so there is still one
                                // thing only the computer's screen can supply.
                                addr = pc.addr
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
                                        Text("waiting")
                                            .font(.caption)
                                            .foregroundStyle(.green)
                                    }
                                }
                            }
                        }
                    } header: {
                        Text("On this network")
                    } footer: {
                        Text("“Waiting” means that computer has a pairing window open.")
                    }
                }

                Section("Pairing code") {
                    TextField("ABCD1234", text: $code)
                        .textInputAutocapitalization(.characters)
                        .autocorrectionDisabled()
                        .font(.body.monospaced())
                }
                Section {
                    TextField("192.168.1.10:1971", text: $addr)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .keyboardType(.URL)
                } header: {
                    Text("IP address")
                } footer: {
                    Text("Only needed if this \(deviceKind()) cannot find the computer by itself.")
                }
            }
            .navigationTitle("Pair another device")
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Pair") {
                        Task { await model.pair(withCode: code, at: addr); dismiss() }
                    }
                    .disabled(code.isEmpty || addr.isEmpty)
                }
            }
            .sheet(isPresented: $scanning) {
                QrScannerView { text in
                    scanning = false
                    apply(scanned: text)
                }
            }
        }
    }

    /// Take what the camera read, if it is one of ours.
    ///
    /// Decoded by the same Rust that built it, so a payload this cannot read is
    /// genuinely not an acrylius code rather than a disagreement between two
    /// implementations of the format.
    private func apply(scanned text: String) {
        do {
            let q = try decodePairingQr(text: text)
            code = q.code
            addr = q.addr
            scanTrouble = nil
            // Straight through. Everything a pairing needs was in the picture,
            // and the SAS on both screens is still the thing a person checks.
            Task { await model.pair(withCode: q.code, at: q.addr); dismiss() }
        } catch {
            scanTrouble = "That is not an Acrylius pairing code."
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
/// The code is a cross-check, not the security mechanism: `XXpsk0` already
/// makes a wrong pairing code fail to decrypt. Showing it costs nothing and
/// catches the class of bug a PSK check would mask.
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
