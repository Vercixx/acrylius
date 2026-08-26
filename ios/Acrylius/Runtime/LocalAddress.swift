//
//  This device's own address on the network it is on.
//
//  Needed for exactly one thing: receiving a file. The side channel has the
//  receiver listen and the sender connect, so a phone accepting a file has to
//  name somewhere the computer can reach — and a socket bound to every
//  interface cannot say which of its addresses that is. The daemon has the same
//  problem and answers it with `advertise_host` in its config; a phone has
//  nowhere to put a config and has to look.
//
//  Nothing else needs this. The session transport dials out and is never
//  dialled, which is why this did not exist until now.
//

import Foundation

#if canImport(Darwin)
import Darwin
#endif

enum LocalAddress {
    /// The IPv4 address of the Wi-Fi interface, if this device is on Wi-Fi.
    ///
    /// `en0` only, and IPv4 only, and both on purpose. Cellular cannot carry a
    /// transfer from a computer on your desk, and an address from `pdp_ip0`
    /// would be one the sender dials and never reaches — a failure that looks
    /// like a hung transfer rather than a phone that is not on the network.
    /// Returning nothing is the honest answer there, and the caller says so.
    static func wifiIPv4() -> String? {
        #if canImport(Darwin)
        var head: UnsafeMutablePointer<ifaddrs>?
        guard getifaddrs(&head) == 0, let first = head else { return nil }
        defer { freeifaddrs(head) }

        var found: String?
        for ptr in sequence(first: first, next: { $0.pointee.ifa_next }) {
            let flags = Int32(ptr.pointee.ifa_flags)
            guard flags & IFF_UP != 0, flags & IFF_LOOPBACK == 0 else { continue }
            guard let addr = ptr.pointee.ifa_addr,
                  addr.pointee.sa_family == UInt8(AF_INET)
            else { continue }
            guard String(cString: ptr.pointee.ifa_name) == "en0" else { continue }

            var host = [CChar](repeating: 0, count: Int(NI_MAXHOST))
            let ok = getnameinfo(
                addr, socklen_t(addr.pointee.sa_len),
                &host, socklen_t(host.count),
                nil, 0, NI_NUMERICHOST)
            if ok == 0 {
                found = String(cString: host)
                break
            }
        }
        return found
        #else
        return nil
        #endif
    }
}
