//
//  TCP over Network.framework. iOS and macOS only.
//
//  Network.framework rather than raw sockets, and not by preference: it is what
//  drives the Local Network permission prompt correctly, picks the right
//  interface, and tells us when the path changes. A BSD socket from Rust would
//  work on Linux and behave badly here.
//
//  Framing matches the daemon: u32 big-endian length, then that many bytes,
//  capped at 1 MiB. The cap is enforced before allocating, so a peer cannot name
//  a length and make us reserve it before sending anything.
//

#if canImport(Network)

import Foundation
import Network

public final class NWTransport: Transport, @unchecked Sendable {
    public let transportId: UInt16
    private let serviceType: String
    private let port: UInt16

    private let lock = NSLock()
    private var emit: (@Sendable (FfiEvent) -> Void)?
    private var connections: [UInt64: NWConnection] = [:]
    private var nextLink: UInt64 = 1
    private var browser: NWBrowser?
    private let queue = DispatchQueue(label: "org.acrylius.transport")

    public static let maxFrame: UInt32 = 1 << 20

    public init(transportId: UInt16 = 1, serviceType: String, port: UInt16) {
        self.transportId = transportId
        self.serviceType = serviceType
        self.port = port
    }

    // MARK: - synchronous helpers, so no lock is held across a suspension

    private func setEmit(_ f: @escaping @Sendable (FfiEvent) -> Void) {
        lock.lock(); emit = f; lock.unlock()
    }
    private func fire(_ e: FfiEvent) {
        lock.lock(); let f = emit; lock.unlock()
        f?(e)
    }
    /// The counter is ours; the id is not. `linkId` namespaces it by transport,
    /// because the core keys every link in one table and a second transport
    /// counting from 1 would otherwise mint ids this one already handed out.
    private func claimLink(_ c: NWConnection) -> UInt64 {
        lock.lock(); defer { lock.unlock() }
        let id = linkId(transport: transportId, counter: nextLink)
        nextLink += 1
        connections[id] = c
        return id
    }
    private func connection(_ link: UInt64) -> NWConnection? {
        lock.lock(); defer { lock.unlock() }
        return connections[link]
    }
    private func release(_ link: UInt64) -> NWConnection? {
        lock.lock(); defer { lock.unlock() }
        return connections.removeValue(forKey: link)
    }

    // MARK: - Transport

    public func start(events: @escaping @Sendable (FfiEvent) -> Void) async {
        setEmit(events)
    }

    /// Addresses are opaque to the core and are produced by this transport, so
    /// only these two shapes ever come back:
    ///
    /// - `bonjour:<instance>` for something discovery found. Resolution is left
    ///   to Network.framework, which is the point: `NWBrowser` will not resolve
    ///   SRV while browsing, and synthesising `<name>.<type>.local.` instead
    ///   would depend on unicast DNS resolving a multicast name.
    /// - `host:port` for an address a human supplied.
    public func dial(addr: String, token: UInt64) async {
        let endpoint: NWEndpoint
        if let instance = addr.stripPrefix("bonjour:") {
            endpoint = .service(name: instance, type: serviceType, domain: "local.", interface: nil)
        } else {
            let parts = addr.split(separator: ":")
            guard parts.count >= 2, let last = parts.last,
                  let p = NWEndpoint.Port(String(last)) else {
                fire(.dialFailed(dial: token, reason: "malformed address \(addr)"))
                return
            }
            endpoint = .hostPort(host: .init(parts.dropLast().joined(separator: ":")), port: p)
        }
        attach(NWConnection(to: endpoint, using: .tcp), dial: token)
    }

    public func send(link: UInt64, msg: Data) async {
        guard let c = connection(link) else { return }
        var header = UInt32(msg.count).bigEndian
        var frame = Data(bytes: &header, count: 4)
        frame.append(msg)
        c.send(content: frame, completion: .contentProcessed { _ in })
    }

    public func close(link: UInt64) async {
        release(link)?.cancel()
    }

    public func advertise(enable: Bool, txt: [FfiTxt]) async {
        // Advertising is deliberately unimplemented on iOS.
        //
        // The phone never listens: it always dials, and the PC never dials the
        // phone. That is what lets a session exist at all on a free developer
        // account, where there is no background push and no way to accept an
        // inbound connection while the app is closed. Symmetry lives at the
        // packet layer, where once a session is up either side may send, not at
        // the connection layer.
    }

    public func discover(enable: Bool) async {
        guard enable else {
            browser?.cancel()
            browser = nil
            return
        }
        guard browser == nil else { return }
        let params = NWParameters()
        params.includePeerToPeer = false
        let b = NWBrowser(for: .bonjourWithTXTRecord(type: serviceType, domain: nil), using: params)

        b.stateUpdateHandler = { [weak self] state in
            // `.waiting` on a Bonjour browse almost always means Local Network
            // permission was declined. There is no API to query it, so this is
            // the signal we have. Say so plainly rather than reporting a
            // generic failure the user cannot act on.
            if case let .waiting(error) = state {
                self?.fire(.dialFailed(
                    dial: 0,
                    reason: "local network permission appears to be denied (\(error))"
                ))
            }
        }
        b.browseResultsChangedHandler = { [weak self] results, _ in
            guard let self else { return }
            for r in results {
                guard case let .service(name, _, _, _) = r.endpoint else { continue }
                var txt: NWTXTRecord?
                if case let .bonjour(record) = r.metadata { txt = record }
                self.fire(.discovered(
                    transport: self.transportId,
                    peer: FfiDiscoveredPeer(
                        fingerprint: txt?["fp"],
                        name: txt?["n"] ?? name,
                        // Hand the instance name back, not a resolved address.
                        // See `dial`.
                        addr: "bonjour:\(name)",
                        pairing: txt?["pair"] == "1"
                    )
                ))
            }
        }
        browser = b
        b.start(queue: queue)
    }

    // MARK: - connection plumbing

    private func attach(_ c: NWConnection, dial: UInt64?) {
        let link = claimLink(c)
        c.stateUpdateHandler = { [weak self] state in
            guard let self else { return }
            switch state {
            case .ready:
                self.fire(.linkUp(link: link, attrs: tcpLanAttrs(transport: self.transportId),
                                  dial: dial))
                self.receiveHeader(c, link: link)
            case let .failed(error):
                if let dial {
                    self.fire(.dialFailed(dial: dial, reason: "\(error)"))
                } else {
                    self.fire(.linkDown(link: link, reason: .transport(detail: "\(error)")))
                }
                _ = self.release(link)
            case .cancelled:
                self.fire(.linkDown(link: link, reason: .closed))
                _ = self.release(link)
            default:
                break
            }
        }
        c.start(queue: queue)
    }

    private func receiveHeader(_ c: NWConnection, link: UInt64) {
        c.receive(minimumIncompleteLength: 4, maximumLength: 4) { [weak self] data, _, done, error in
            guard let self else { return }
            if error != nil || done {
                self.fire(.linkDown(link: link, reason: .closed))
                _ = self.release(link)
                return
            }
            guard let data, data.count == 4 else { return }
            let n = data.withUnsafeBytes { $0.load(as: UInt32.self).bigEndian }
            guard n <= Self.maxFrame else {
                // Refuse before allocating. A peer that claims more than the cap
                // is hung up on rather than believed.
                self.fire(.linkDown(link: link,
                                    reason: .transport(detail: "frame of \(n) exceeds the cap")))
                _ = self.release(link)
                c.cancel()
                return
            }
            guard n > 0 else { self.receiveHeader(c, link: link); return }
            self.receiveBody(c, link: link, count: Int(n))
        }
    }

    private func receiveBody(_ c: NWConnection, link: UInt64, count: Int) {
        c.receive(minimumIncompleteLength: count, maximumLength: count) {
            [weak self] data, _, done, error in
            guard let self else { return }
            if error != nil || done {
                self.fire(.linkDown(link: link, reason: .closed))
                _ = self.release(link)
                return
            }
            if let data, data.count == count {
                self.fire(.linkRecv(link: link, msg: data))
            }
            self.receiveHeader(c, link: link)
        }
    }
}

private extension String {
    func stripPrefix(_ p: String) -> String? {
        hasPrefix(p) ? String(dropFirst(p.count)) : nil
    }
}

#endif
