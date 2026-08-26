//
//  What each peer has told us it can do.
//
//  A device does not learn a peer's abilities from the handshake. The handshake
//  says which capabilities may be exchanged; what a peer actually has is what it
//  announces on connect: a session state, a list of commands, wake targets. A
//  computer with no commands configured sends no list, and the phone shows no
//  button. That is the whole discovery mechanism, and it means a screen never
//  offers something that cannot work.
//
//  Platform-free on purpose, so it can be tested on Linux.
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
    /// What is playing there. Present once the peer has described its players,
    /// which it does on connect and after every command.
    public var media: FfiMediaState?
    /// The most recent refusal, for showing why a button did nothing.
    public var lastError: String?

    public init() {}

    public var canLock: Bool { session != nil }
    public var canWake: Bool { wake?.macs.isEmpty == false }
    public var canRunCommands: Bool { !commands.isEmpty }

    /// Something worth showing a transport control for.
    ///
    /// Not merely "the peer has media": a machine with no players open sends an
    /// empty list, and a remote with nothing to control is a remote that looks
    /// broken.
    public var canControlMedia: Bool { activePlayer != nil }

    /// The player a command with no name goes to, as the peer named it.
    public var activePlayer: FfiMediaPlayer? {
        guard let media else { return nil }
        return media.players.first { $0.id == media.active }
            ?? media.players.first
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

    /// Absorb one event. Returns true when something a view shows changed.
    @discardableResult
    public mutating func ingest(_ event: FfiUiEvent) -> Bool {
        guard case let .plugin(peer, cap, ty, body) = event else {
            if case let .peerUnreachable(peer) = event {
                // Keep what the peer told us. A wake target is only useful once
                // the machine is gone, and a command list does not change while
                // nobody is looking.
                byPeer[peer]?.session = nil
                return true
            }
            return false
        }

        var features = byPeer[peer] ?? PeerFeatures()
        var changed = false

        if ty == "err" {
            features.lastError = (try? decodeError(body: body)) ?? "refused"
            changed = true
        } else if cap == capSession() {
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
                changed = true
            }
        } else if cap == capMedia() {
            if ty == "state", let state = try? decodeMediaState(body: body) {
                features.media = state
                changed = true
            }
        }

        if changed { byPeer[peer] = features }
        return changed
    }
}
