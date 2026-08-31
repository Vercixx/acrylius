#if canImport(AppIntents)

import AppIntents
import Foundation

/// A paired computer, as a shortcut refers to it.
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

    /// List paired peers from disk. Snapshot first: the widget process has no
    /// Keychain access, so building a core there returns nothing.
    func suggestedEntities() async throws -> [PCEntity] {
        if let snapshot = SnapshotStore.load(), !snapshot.peers.isEmpty {
            return snapshot.peers.map { PCEntity(id: $0.deviceId, name: $0.name) }
        }
        let store = try KeychainStore()
        // A locked phone throws here; suggesting no computers is the right
        // answer to that. See `KeychainStore.identityKey`.
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
