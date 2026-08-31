//
//  What each peer has told us it can do. The handshake only says which
//  capabilities may be exchanged; what a peer actually has is announced on
//  connect (session state, commands, wake targets). Platform-free, so it can
//  be tested on Linux.
//

import Foundation

public struct PeerFeatures: Equatable, Sendable {
    /// Present once the peer has described a desktop session.
    public var session: FfiSessionState?
    /// Empty when the peer offers none, which is also when it sends no list.
    public var commands: [FfiCommand] = []
    /// Present when the peer can be woken. Kept because by the time it is
    /// needed the peer is asleep and cannot be asked.
    public var wake: FfiWolConfig?
    /// The last clipboard value this peer handed over.
    public var clipboard: String?
    /// When that value arrived, by this device's clock. Needed because asking
    /// twice can return an identical value, which alone can't signal a reply landed.
    public var clipboardAt: Date?
    /// What is playing there. Present once the peer has described its players,
    /// which it does on connect and after every command.
    public var media: FfiMediaState?
    /// When that reading was taken (not when it arrived) by this device's
    /// clock. Stamping arrival instead skews the position by a round trip.
    public var mediaAt: Date?
    /// When the outstanding media query was sent, if one is. Half the round
    /// trip estimates how long ago the far end looked, with no clock agreement needed.
    public var mediaQuerySentAt: Date?
    /// The most recent refusal, for showing why a button did nothing.
    public var lastError: String?

    public init() {}

    public var canLock: Bool { session != nil }
    public var canWake: Bool { wake?.macs.isEmpty == false }
    public var canRunCommands: Bool { !commands.isEmpty }

    /// Something worth showing a transport control for — not merely "the peer
    /// has media", since a remote with nothing to control looks broken.
    public var canControlMedia: Bool { activePlayer != nil }

    /// The player a command with no name goes to, as the peer named it.
    public var activePlayer: FfiMediaPlayer? {
        guard let media else { return nil }
        return media.players.first { $0.id == media.active }
            ?? media.players.first
    }

    /// How old a reading may be before its position stops being counted
    /// forward. Generous enough to ride out a slow Bluetooth notification.
    static let staleReading: TimeInterval = 10

    /// Where the active track has got to, as of now: the reported position
    /// plus elapsed time, only while playing. Never sent or stored — an
    /// estimate corrected by the next reading.
    public func positionMs(at now: Date = Date()) -> UInt64 {
        guard let player = activePlayer else { return 0 }
        guard player.status == "playing", let mediaAt else { return player.positionMs }
        let elapsed = max(0, now.timeIntervalSince(mediaAt))
        // Only while readings are still arriving — one this stale means nobody
        // is refreshing any more, so freeze rather than invent a position.
        guard elapsed <= Self.staleReading else { return player.positionMs }
        let estimate = player.positionMs + UInt64(elapsed * 1000)
        // Never past the end. A track that finished while nobody was asking
        // would otherwise show a time longer than itself.
        return player.lengthMs > 0 ? min(estimate, player.lengthMs) : estimate
    }
}

/// Folds the core's UI events into a per-peer view.
public struct PeerCatalog: Equatable, Sendable {
    private var byPeer: [String: PeerFeatures] = [:]

    public init() {}

    public subscript(peer: String) -> PeerFeatures {
        byPeer[peer] ?? PeerFeatures()
    }

    public var peers: [String] { Array(byPeer.keys).sorted() }

    /// Record a failure the core reported against a particular peer. Clears
    /// the same way a wire refusal does — on the next thing that peer says.
    public mutating func note(error: String, for peer: String) {
        var features = byPeer[peer] ?? PeerFeatures()
        features.lastError = error
        byPeer[peer] = features
    }

    /// Note that a media reading has just been asked for, paired with the
    /// arrival in `ingest` to place the reading halfway between the two.
    public mutating func noteMediaQuery(for peer: String, at sent: Date = Date()) {
        var features = byPeer[peer] ?? PeerFeatures()
        features.mediaQuerySentAt = sent
        byPeer[peer] = features
    }

    /// When a reading that arrived `now` was most likely taken: the midpoint
    /// of the round trip, assuming both legs are roughly equal.
    static func measuredAt(sent: Date?, arrived: Date) -> Date {
        guard let sent, sent <= arrived else { return arrived }
        let round = arrived.timeIntervalSince(sent)
        // A reply this late isn't a round trip — likely queued behind a
        // reconnect. Halving it would run the clock fast.
        guard round <= PeerFeatures.staleReading else { return arrived }
        return sent.addingTimeInterval(round / 2)
    }

    /// Absorb one event. Returns true when something a view shows changed.
    @discardableResult
    public mutating func ingest(_ event: FfiUiEvent, at now: Date = Date()) -> Bool {
        guard case let .plugin(peer, cap, ty, body) = event else {
            if case let .revoked(peer) = event {
                // Forgetting the device rather than keeping stale commands,
                // wake targets etc. that would resurface if paired again.
                byPeer.removeValue(forKey: peer)
                return true
            }
            if case let .peerUnreachable(peer) = event {
                // Keep what the peer told us — a wake target only matters once
                // the machine is gone.
                byPeer[peer]?.session = nil
                // `positionMs` treats a nil timestamp as "unknown", so this
                // freezes the timeline instead of counting it forward forever.
                byPeer[peer]?.mediaAt = nil
                return true
            }
            return false
        }

        var features = byPeer[peer] ?? PeerFeatures()
        var changed = false

        if ty == "err" {
            features.lastError = (try? decodeError(body: body)) ?? "refused"
            changed = true
        } else if features.lastError != nil {
            // Anything else from this peer means it's answering again, so
            // whatever went wrong before is over.
            features.lastError = nil
            changed = true
        }
        if cap == capSession() {
            switch ty {
            case "state":
                features.session = try? decodeSessionState(body: body)
                changed = true
            case "result":
                if let outcome = try? decodeSessionOutcome(body: body) {
                    // A result carries the state read back afterwards, so it is
                    // as current as a `state` and there is no need to ask again.
                    features.session = FfiSessionState(
                        locked: outcome.locked,
                        sessionId: outcome.sessionId,
                        kind: features.session?.kind ?? "",
                        active: features.session?.active ?? true
                    )
                    changed = true
                }
            default: break
            }
        } else if cap == capCommand() {
            if ty == "list", let commands = try? decodeCommandList(body: body) {
                features.commands = commands
                changed = true
            }
        } else if cap == capWol() {
            if ty == "config", let config = try? decodeWolConfig(body: body) {
                features.wake = config
                // Write it down now. The next time anyone wants it, the machine
                // that sent it will be asleep.
                WakeTargets.save(config, for: peer)
                changed = true
            }
        } else if cap == capClipboard() {
            if ty == "set", let value = try? decodeClipboard(body: body) {
                features.clipboard = value.text
                features.clipboardAt = Date()
                changed = true
            }
        } else if cap == capMedia() {
            if ty == "state", let state = try? decodeMediaState(body: body) {
                features.media = state
                features.mediaAt = Self.measuredAt(sent: features.mediaQuerySentAt, arrived: now)
                // Answered, so the next reading gets its own round trip rather
                // than being measured against this one.
                features.mediaQuerySentAt = nil
                changed = true
            }
        }

        if changed { byPeer[peer] = features }
        return changed
    }
}
