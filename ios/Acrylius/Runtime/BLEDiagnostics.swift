//
//  CoreBluetooth state in readable form, for a sideloaded app with no console.
//  Free of CoreBluetooth so it compiles on Linux under scripts/swift-test.sh.
//

import Foundation
// Explicit import: compiled on Linux, where there is no SwiftUI to re-export it.
import Observation

public struct BLESighting: Identifiable, Sendable, Equatable {
    /// CoreBluetooth's per-app UUID, not a MAC: iOS never exposes one and the
    /// desktop's rotates anyway.
    public let id: String
    public let name: String
    public var rssi: Int
    /// A service only in the GATT database, not the advertisement, is invisible
    /// to a filtered scan — the most likely cause of finding nothing.
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

/// A timestamped line; the ring buffer preserves the order things happened in.
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

/// A Sendable value crossing from CoreBluetooth's queue to the main actor.
public enum BLEUpdate: Sendable {
    case state(String, auth: String)
    case scanning(Bool)
    case sighting(BLESighting)
    case link(String)
    case fragment(Int)
    case note(String)
    /// An actionable failure; `nil` clears it.
    case trouble(String?)
}

@Observable @MainActor
public final class BLEDiagnostics {
    /// `CBManagerState`, spelled out.
    public var managerState: String = "not started"
    /// Separate from state: a denied prompt is undone only in Settings.
    public var authorization: String = "unknown"

    /// Compared by the transport and the Devices screen; a literal in both
    /// would drift silently. `nonisolated` because the transport reads it off
    /// the main actor, and only the macOS compiler would flag that.
    public nonisolated static let waitingForPermission = "waiting for permission"

    /// Unusable, but asking would still fix it — unlike a denial.
    public var awaitingPermission: Bool { managerState == Self.waitingForPermission }
    public var scanning: Bool = false
    public var sightings: [BLESighting] = []
    public var link: String = "none"
    /// The negotiated ATT payload, once known.
    public var fragmentBytes: Int?
    /// A failure that won't clear itself plus the step that clears it, kept
    /// apart from the scrolling notes.
    public var trouble: String?
    public var notes: [BLENote] = []

    /// Bounded: an unbounded log on a device with no console is an invisible leak.
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

    /// Everything, as copyable text for a bug report.
    public func transcript() -> String {
        var out = "build: \(BuildInfo.current.summary)\n"
        out += "state: \(managerState)\nauth: \(authorization)\n"
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
