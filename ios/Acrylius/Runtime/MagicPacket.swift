//
//  Sending a magic packet from the phone.
//
//  The packet itself is built in Rust, so there is one definition of it. This
//  only puts the bytes on the wire.
//
//  Unicast is the primary path and not a fallback. A network interface matches a
//  magic packet by its payload and pays no attention to the destination address,
//  so a datagram aimed at the machine's last known address wakes it exactly as
//  well as a broadcast, and iOS gates broadcast behind an entitlement a free
//  developer account cannot get. The requirement is that the router still holds
//  an ARP entry for the sleeping machine, which in practice means a DHCP
//  reservation and a static ARP entry.
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
    /// Send to every destination in order. Returns true if any send succeeded.
    ///
    /// Every destination is tried even after one succeeds, because a send
    /// completing only means the datagram left. Whether the machine woke is not
    /// knowable from here, which is why the caller confirms by looking for the
    /// machine to come back.
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
            // Not a captured `var`. Two Network.framework callbacks can arrive
            // concurrently — a send completing while the state handler reports
            // `.cancelled`, say — and resuming a continuation twice is a crash,
            // not a warning. `Once` makes the winner unambiguous.
            let once = Once()
            let finish: @Sendable (Bool) -> Void = { ok in
                guard once.claim() else { return }
                connection.cancel()
                continuation.resume(returning: ok)
            }
            connection.stateUpdateHandler = { state in
                switch state {
                case .ready:
                    connection.send(content: packet, completion: .contentProcessed { error in
                        // EACCES here is iOS refusing a broadcast, which is
                        // expected without the multicast entitlement. The
                        // unicast attempt is the one that matters.
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
