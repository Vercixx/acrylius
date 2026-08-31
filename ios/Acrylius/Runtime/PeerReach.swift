import Foundation

// `FfiPeer` carries three states rather than `reachable: Bool`, since a peer
// mid-handshake and one that gave up read differently to a person, even
// though most call sites only care about the binary question.
//
// Lives under Runtime, not Views: scripts/swift-test.sh only type-checks Runtime.

extension FfiPeer {
    /// There is a session with this peer right now.
    var reachable: Bool { state == .reachable }

    /// An attempt is in flight. `trouble` is only ever set once every route
    /// has been spent.
    var connecting: Bool { state == .connecting }
}
