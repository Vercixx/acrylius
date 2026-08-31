//
//  This device's own address on the network it is on, needed for receiving a
//  file: the receiver listens and the sender connects, so a socket bound to
//  every interface needs to say which address that is. Nothing else needs this.
//

import Foundation

#if canImport(Darwin)
import Darwin
#endif

enum LocalAddress {
    /// The IPv4 address of the Wi-Fi interface, if this device is on Wi-Fi.
    ///
    /// `en0` and IPv4 only, on purpose: cellular's `pdp_ip0` address is one a
    /// sender would dial and never reach, which looks like a hung transfer.
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
