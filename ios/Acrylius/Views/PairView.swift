#if canImport(SwiftUI)

import SwiftUI

/// Enter the code the PC printed, and where to reach it.
struct PairView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @State private var code = ""
    @State private var addr = ""

    var body: some View {
        NavigationStack {
            Form {
                Section("Code from your PC") {
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
                    Text("Where")
                } footer: {
                    Text("Run `acryliusctl pair` on the PC first. It prints both.")
                }
            }
            .navigationTitle("Pair a PC")
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
        }
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

            Text("Confirm this matches what your PC shows.")
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
