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
    func suggestedEntities() async throws -> [PCEntity] {
        let store = try KeychainStore()
        guard let key = store.identityKey() else { return [] }
        let core = try AcryliusCore(
            config: defaultConfig(name: "Acrylius", platform: "ios"),
            identityKey: key,
            peers: store.loadPeers()
        )
        return core.peers().map { PCEntity(id: $0.deviceId, name: $0.name) }
    }

    func defaultResult() async -> PCEntity? {
        try? await suggestedEntities().first
    }
}

#endif
