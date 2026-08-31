//
//  Sending a magic packet from the phone; built in Rust, this only puts the
//  bytes on the wire. Unicast is primary, not a fallback — a NIC matches a
//  magic packet by payload regardless of destination, and iOS gates broadcast
//  behind an entitlement a free account can't get.
//

#if canImport(Network)

import Foundation
import Network

/// Lets exactly one caller through, whichever arrives first.
private final class Once: @unchecked Sendable {
    private let lock = NSLock()
    private var taken = false

    func claim() -> Bool {
        lock.lock()
        defer { lock.unlock() }
        if taken { return false }
        taken = true
        return true
    }
}

public enum MagicPacketSender {
    /// How long one destination may take before it is given up on. At most
    /// two destinations are tried, so the whole call is bounded by twice this.
    static let sendTimeout: TimeInterval = 2

    /// Send to every destination in order. Returns true if any send succeeded.
    /// A send succeeding only means the datagram left, not that the machine woke.
    public static func send(_ packet: Data, to destinations: [String], port: UInt16) async -> Bool {
        var any = false
        for destination in destinations {
            if await sendOne(packet, to: destination, port: port) { any = true }
        }
        return any
    }

    private static func sendOne(_ packet: Data, to host: String, port: UInt16) async -> Bool {
        guard let p = NWEndpoint.Port(rawValue: port) else { return false }
        let connection = NWConnection(host: .init(host), port: p, using: .udp)
        return await withCheckedContinuation { continuation in
            // Not a captured `var`: two Network.framework callbacks can arrive
            // concurrently, and resuming a continuation twice crashes.
            let once = Once()
            let finish: @Sendable (Bool) -> Void = { ok in
                guard once.claim() else { return }
                connection.cancel()
                continuation.resume(returning: ok)
            }
            // Bounded because `stateUpdateHandler` never fires without Local
            // Network permission — the connection just sits in `.waiting` forever.
            let timeout = DispatchWorkItem { finish(false) }
            DispatchQueue.global().asyncAfter(deadline: .now() + Self.sendTimeout, execute: timeout)

            connection.stateUpdateHandler = { state in
                switch state {
                case .ready:
                    connection.send(content: packet, completion: .contentProcessed { error in
                        // EACCES here is iOS refusing broadcast without the
                        // multicast entitlement; the unicast attempt is what matters.
                        finish(error == nil)
                    })
                case .failed, .cancelled:
                    finish(false)
                default:
                    break
                }
            }
            connection.start(queue: .global())
        }
    }
}

#endif
