import Foundation

// `FfiPeer` carries three states rather than `reachable: Bool`, because with
// dialling entirely automatic a peer mid-handshake and a peer that has given up
// are different things to say to a person.
//
// Most of the app does not care about that difference: "may I send a file",
// "may I run a command" and "is there a session at all" are all one question,
// and asking them through `state == .reachable` at every call site would read
// as though the answer were subtle when it is not. So the binary question keeps
// a name, and the screens that report status use `state` directly.
//
// This lives under Runtime rather than Views on purpose: scripts/swift-test.sh
// compiles Runtime and nothing else, so the one piece of this that a Linux box
// can type-check is the piece every view depends on.

extension FfiPeer {
    /// There is a session with this peer right now.
    var reachable: Bool { state == .reachable }

    /// An attempt is in flight. Not yet a failure, and not worth explaining —
    /// `trouble` is only ever set once every route has been spent.
    var connecting: Bool { state == .connecting }
}
