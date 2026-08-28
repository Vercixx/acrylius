#if canImport(AppIntents)

import AppIntents
import Foundation

/// A paired computer, as a shortcut can refer to it.
///
/// Backed by a query so a shortcut reads "Lock Desktop" rather than asking the
/// user to paste a device id, and so it defaults sensibly when only one computer
/// is paired.
struct PCEntity: AppEntity {
    let id: String
    let name: String

    static var typeDisplayRepresentation: TypeDisplayRepresentation { "PC" }

    var displayRepresentation: DisplayRepresentation {
        DisplayRepresentation(title: "\(name)")
    }

    static var defaultQuery: PCQuery { PCQuery() }
}

struct PCQuery: EntityQuery {
    func entities(for identifiers: [String]) async throws -> [PCEntity] {
        try await suggestedEntities().filter { identifiers.contains($0.id) }
    }

    /// Read the peer list without standing up a session.
    ///
    /// An intent runs in a short-lived process, and listing what is paired needs
    /// no network at all: the records are already on disk.
    ///
    /// The snapshot is tried first, and not only because it is cheaper. A widget
    /// runs in a process with no Keychain access and no identity, so building a
    /// core there is not merely wasteful — it returns nothing at all.
    func suggestedEntities() async throws -> [PCEntity] {
        if let snapshot = SnapshotStore.load(), !snapshot.peers.isEmpty {
            return snapshot.peers.map { PCEntity(id: $0.deviceId, name: $0.name) }
        }
        let store = try KeychainStore()
        // A locked phone throws here rather than answering "no identity", and
        // suggesting no computers is the right answer to that — inventing one
        // is not. See `KeychainStore.identityKey`.
        guard let key = try store.identityKey() else { return [] }
        let core = try AcryliusCore(
            config: defaultConfig(name: "Acrylius", platform: "ios"),
            identityKey: key,
            peers: store.loadPeers(),
            effects: []
        )
        return core.peers().map { PCEntity(id: $0.deviceId, name: $0.name) }
    }

    func defaultResult() async -> PCEntity? {
        try? await suggestedEntities().first
    }
}

#endif
