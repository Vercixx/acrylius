#if canImport(SwiftUI)

import SwiftUI
#if canImport(UIKit)
import UIKit
#endif

/// Files moving in either direction.
///
/// Sending used to live inside one computer's screen, which made the
/// destination implicit and free. It is a picker here instead — the cost of
/// having somewhere for an *incoming* offer to live that a person can reach
/// from anywhere, rather than only from the root list they had navigated away
/// from.
struct FilesView: View {
    @Environment(AppModel.self) private var model

    /// The chosen destination, by device id. Held as an id rather than a peer
    /// so a reconnection — which replaces every `FfiPeer` — does not silently
    /// reset the picker.
    @State private var destination: String?

    private var reachable: [FfiPeer] { model.peers.filter(\.reachable) }

    /// Where a file would go. Falls back rather than going nil when the chosen
    /// peer drops, so the picker cannot get stuck pointing at nothing.
    private var chosen: FfiPeer? {
        if let id = destination, let peer = reachable.first(where: { $0.deviceId == id }) {
            return peer
        }
        return reachable.first
    }

    var body: some View {
        NavigationStack {
            List {
                // First, because it is the only thing here waiting on you. A
                // transfer holds the sending computer open until it is answered.
                if !model.incoming.isEmpty {
                    Section {
                        ForEach(model.incoming) { offer in
                            IncomingOfferRow(offer: offer)
                        }
                    } header: {
                        Text("Offered to this \(UIDevice.current.model)")
                    } footer: {
                        Text("Accepted files go to Acrylius in the Files app.")
                    }
                }

                if let peer = chosen {
                    // Only worth asking when there is a choice to make.
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
                        // A picker that leads to "unreachable" is worse than no
                        // picker: the file is copied and staged before the
                        // offer travels, so the failure would arrive well after
                        // the part that felt like the decision.
                        ContentUnavailableView(
                            "Nothing connected",
                            systemImage: "arrow.up.arrow.down",
                            description: Text("Connect a computer to send it a file.")
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
            HStack {
                TaskButton("Accept") { await model.accept(offer); return true }
                    .buttonStyle(.borderedProminent)
                TaskButton("Decline") { await model.decline(offer); return true }
                    .buttonStyle(.bordered)
            }
        }
        .padding(.vertical, 4)
    }
}

#endif
