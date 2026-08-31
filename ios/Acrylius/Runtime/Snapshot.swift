//
//  What the app leaves behind for the widget to draw. A widget can't open a
//  session (no Local Network permission, a fraction of a second of runtime),
//  so it renders what the app saw last. Plain `Codable`, not FFI protocol —
//  a decode failure is just a stale render.
//

import Foundation

public struct PeerSnapshot: Codable, Equatable, Sendable {
    public var deviceId: String
    public var name: String
    public var platform: String
    /// When this peer was last actually reachable, or nil since the app
    /// started. Shown instead of a live dot, since "connected" would be a lie.
    public var lastSeen: Date?
    /// Nil when the peer has never described a desktop session.
    public var locked: Bool?
    /// Whether a wake target is on file. A widget needs to know before it can
    /// offer the button, and the answer lives in a different file.
    public var canWake: Bool
    /// "Artist — Title", already formatted, or nil.
    public var nowPlaying: String?

    public init(
        deviceId: String, name: String, platform: String, lastSeen: Date? = nil,
        locked: Bool? = nil, canWake: Bool = false, nowPlaying: String? = nil
    ) {
        self.deviceId = deviceId
        self.name = name
        self.platform = platform
        self.lastSeen = lastSeen
        self.locked = locked
        self.canWake = canWake
        self.nowPlaying = nowPlaying
    }
}

public struct Snapshot: Codable, Equatable, Sendable {
    public var peers: [PeerSnapshot]
    public var written: Date
    /// False when the App Group did not resolve. The app shows this; the widget
    /// never sees a snapshot at all in that case, which is the whole problem.
    public var shared: Bool

    public init(peers: [PeerSnapshot], written: Date, shared: Bool) {
        self.peers = peers
        self.written = written
        self.shared = shared
    }
}

public enum SnapshotStore {
    private static var url: URL? {
        SharedContainer.base?.appendingPathComponent("snapshot.json")
    }

    /// Keeps `lastSeen` from the snapshot already on disk — once a peer is
    /// gone, the running app is the only thing that ever knew when.
    public static func save(peers: [PeerSnapshot]) {
        guard let url else { return }
        let previous = load()?.peers.reduce(into: [String: Date]()) { seen, p in
            if let last = p.lastSeen { seen[p.deviceId] = last }
        } ?? [:]
        let merged = peers.map { peer -> PeerSnapshot in
            var peer = peer
            if peer.lastSeen == nil { peer.lastSeen = previous[peer.deviceId] }
            return peer
        }
        let snapshot = Snapshot(
            peers: merged, written: Date(), shared: SharedContainer.isShared)
        guard let body = try? JSONEncoder().encode(snapshot) else { return }
        try? body.write(to: url, options: .atomic)
    }

    public static func load() -> Snapshot? {
        guard let url, let body = try? Data(contentsOf: url) else { return nil }
        return try? JSONDecoder().decode(Snapshot.self, from: body)
    }
}
