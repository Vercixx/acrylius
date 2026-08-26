//
//  What the app leaves behind for the widget to draw.
//
//  A widget cannot open a session. It has no Local Network permission of its
//  own, it gets a fraction of a second of runtime, and the daemon cannot reach
//  a phone that is not running the app anyway. So it does not ask anything: it
//  renders what the app saw last, and says when that was.
//
//  This is not protocol and deliberately does not go through the FFI. Nothing
//  here crosses a network or is read by anything but this app's own processes,
//  so it is plain `Codable` and may change shape whenever it likes — a decode
//  that fails is one stale render, not a compatibility break.
//

import Foundation

public struct PeerSnapshot: Codable, Equatable, Sendable {
    public var deviceId: String
    public var name: String
    public var platform: String
    /// When this peer was last actually reachable. Nil means not since the app
    /// started. A widget shows this rather than a live dot, because "connected"
    /// on a screen the app is not behind is always a lie.
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

    /// Keeps `lastSeen` from the snapshot already on disk.
    ///
    /// The app only knows a peer is reachable while it is; the moment it is not,
    /// the interesting fact is when it stopped, and the running app is the only
    /// thing that ever knew.
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
