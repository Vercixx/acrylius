//
//  What CoreBluetooth is doing, in a form a person can read.
//
//  This app reaches a device through a CI build and a sideload. There is no
//  Xcode console, no breakpoint and no `print` anyone will ever see, so an app
//  that cannot say what it is doing costs a full build cycle per question. That
//  is the whole reason this file exists, and it is why it is written before the
//  transport rather than after it.
//
//  Deliberately free of CoreBluetooth. It is a plain state holder, so it
//  compiles on Linux and `scripts/swift-test.sh` keeps covering it — the probe
//  that fills it in is the part that cannot.
//

import Foundation
// Explicit rather than leaning on SwiftUI to re-export it: this file is compiled
// on Linux by `scripts/swift-test.sh`, where there is no SwiftUI.
import Observation

/// One peripheral the scan has seen.
public struct BLESighting: Identifiable, Sendable, Equatable {
    /// CoreBluetooth's per-app, per-device UUID for the peer. Not a MAC: iOS
    /// never exposes one, and the desktop's address rotates anyway.
    public let id: String
    public let name: String
    public var rssi: Int
    /// Whether the advertisement carried our service UUID.
    ///
    /// Worth showing on its own, because a service that is in the peripheral's
    /// GATT database but absent from its advertisement is invisible to a
    /// filtered scan — the single most likely cause of finding nothing.
    public var advertisedOurService: Bool
    public var lastSeen: Date

    public init(
        id: String, name: String, rssi: Int,
        advertisedOurService: Bool, lastSeen: Date
    ) {
        self.id = id
        self.name = name
        self.rssi = rssi
        self.advertisedOurService = advertisedOurService
        self.lastSeen = lastSeen
    }
}

/// A timestamped line. The ring buffer is the useful part: what matters when
/// something goes wrong is the *order* things happened in, and a single "last
/// error" field throws that away.
public struct BLENote: Identifiable, Sendable, Equatable {
    public let id: UUID
    public let at: Date
    public let text: String

    public init(text: String, at: Date = Date()) {
        self.id = UUID()
        self.at = at
        self.text = text
    }
}

/// One thing the radio did.
///
/// A plain `Sendable` value rather than a closure the transport hands over. The
/// transport runs on CoreBluetooth's queue and the diagnostics live on the main
/// actor, so whatever crosses between them has to be sendable — and a value the
/// compiler can check is worth more than a closure whose annotations have to be
/// right, on a file no compiler here can see inside.
public enum BLEUpdate: Sendable {
    case state(String, auth: String)
    case scanning(Bool)
    case sighting(BLESighting)
    case link(String)
    case fragment(Int)
    case note(String)
    /// Something is wrong *and* there is something a person can do about it.
    /// `nil` clears it, which is what connecting successfully does.
    case trouble(String?)
}

@Observable @MainActor
public final class BLEDiagnostics {
    /// `CBManagerState`, spelled out. `.unsupported` on a simulator,
    /// `.unauthorized` when the permission was refused, `.poweredOff` when
    /// Bluetooth is off in Control Centre — three very different problems that
    /// look identical from the outside.
    public var managerState: String = "not started"
    /// `CBManagerAuthorization`. Separate from the state because a user who
    /// denied the prompt can only fix it in Settings, and nothing else will.
    public var authorization: String = "unknown"
    public var scanning: Bool = false
    public var sightings: [BLESighting] = []
    /// Whether a link is up, and to whom.
    public var link: String = "none"
    /// The negotiated ATT payload, once known. Asked for, never assumed.
    public var fragmentBytes: Int?
    /// The one line worth putting in front of someone.
    ///
    /// Separate from the notes, because a transcript is where a thing goes to
    /// be scrolled past. This is for a failure that will not clear itself and
    /// names the step that clears it.
    public var trouble: String?
    public var notes: [BLENote] = []

    /// Kept short on purpose: this is read on a phone screen, and an unbounded
    /// log on a device with no console is a memory leak nobody can see.
    static let maxNotes = 60

    public init() {}

    public func note(_ text: String) {
        notes.append(BLENote(text: text))
        if notes.count > Self.maxNotes {
            notes.removeFirst(notes.count - Self.maxNotes)
        }
    }

    public func apply(_ u: BLEUpdate) {
        switch u {
        case let .state(s, auth):
            managerState = s
            authorization = auth
            note("state: \(s)")
        case let .scanning(on):
            scanning = on
        case let .sighting(s):
            saw(s)
        case let .link(l):
            link = l
        case let .fragment(n):
            fragmentBytes = n
        case let .note(t):
            note(t)
        case let .trouble(t):
            trouble = t
            if let t { note("problem: \(t)") }
        }
    }

    public func saw(_ s: BLESighting) {
        if let i = sightings.firstIndex(where: { $0.id == s.id }) {
            sightings[i] = s
        } else {
            sightings.append(s)
            note("saw \(s.name) (\(s.advertisedOurService ? "ours" : "not ours"))")
        }
        sightings.sort { $0.rssi > $1.rssi }
    }

    /// Everything, as text to copy out of the app. A screenshot of a scrolling
    /// list is a poor bug report; this is the thing worth pasting.
    public func transcript() -> String {
        var out = "state: \(managerState)\nauth: \(authorization)\n"
        out += "scanning: \(scanning)\nlink: \(link)\n"
        if let f = fragmentBytes { out += "fragment: \(f) bytes\n" }
        if let t = trouble { out += "trouble: \(t)\n" }
        out += "\nsightings:\n"
        for s in sightings {
            out += "  \(s.name)  \(s.rssi) dBm  \(s.advertisedOurService ? "ours" : "-")\n"
        }
        out += "\nnotes:\n"
        let f = DateFormatter()
        f.dateFormat = "HH:mm:ss"
        for n in notes {
            out += "  \(f.string(from: n.at))  \(n.text)\n"
        }
        return out
    }
}
