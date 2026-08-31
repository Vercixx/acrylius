#if canImport(SwiftUI)

import SwiftUI
#if canImport(UIKit)
import UIKit
#endif

/// Files moving in either direction, plus incoming offers waiting on an answer.
struct FilesView: View {
    @Environment(AppModel.self) private var model

    /// Held as a device id, not a peer, so reconnecting doesn't reset the picker.
    @State private var destination: String?

    private var reachable: [FfiPeer] { model.peers.filter(\.reachable) }

    /// Falls back rather than going nil when the chosen peer drops.
    private var chosen: FfiPeer? {
        if let id = destination, let peer = reachable.first(where: { $0.deviceId == id }) {
            return peer
        }
        return reachable.first
    }

    var body: some View {
        NavigationStack {
            List {
                // Listed first: a transfer holds the sending computer open until answered.
                if !model.incoming.isEmpty {
                    Section {
                        ForEach(model.incoming) { offer in
                            IncomingOfferRow(offer: offer)
                        }
                    } header: {
                        Text("Offered to this \(UIDevice.current.model)")
                    } footer: {
                        Text("Incoming files are stored in \"Acrylius\" folder in the Files app.")
                    }
                }

                if let peer = chosen {
                    if reachable.count > 1 {
                        Section {
                            Picker("To", selection: Binding(
                                get: { peer.deviceId },
                                set: { destination = $0 }
                            )) {
                                ForEach(reachable, id: \.deviceId) { candidate in
                                    Text(candidate.name).tag(candidate.deviceId)
                                }
                            }
                        }
                    }
                    SendFileSection(peer: peer)
                } else {
                    Section {
                        // No picker when unreachable: the file is staged before the
                        // offer travels, so the failure would surface well after the tap.
                        ContentUnavailableView(
                            "Nothing is connected",
                            systemImage: "arrow.up.arrow.down",
                            description: Text("Connect a device to begin transfering.")
                        )
                    }
                }
            }
            .navigationTitle("Files")
        }
    }
}

/// One file a computer wants to send, and the two answers to it.
private struct IncomingOfferRow: View {
    @Environment(AppModel.self) private var model
    let offer: AppModel.IncomingOffer

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(offer.name).font(.body)
            Text(ByteCountFormatter.string(fromByteCount: Int64(offer.size), countStyle: .file))
                .font(.caption)
                .foregroundStyle(.secondary)
            // Plain buttons, not `TaskButton`: accepting only submits, the transfer
            // finishes minutes later, so a success tick here would be a false claim.
            if model.accepting.contains(offer.transfer) {
                HStack(spacing: 8) {
                    ProgressView().controlSize(.small)
                    Text("Receiving…").font(.callout).foregroundStyle(.secondary)
                }
            } else {
                HStack {
                    Button("Accept") { Task { await model.accept(offer) } }
                        .buttonStyle(.borderedProminent)
                    Button("Decline") { Task { await model.decline(offer) } }
                        .buttonStyle(.bordered)
                }
            }
        }
        .padding(.vertical, 4)
    }
}

#endif
