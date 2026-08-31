//
//  TCP over Network.framework (iOS/macOS only) — it drives the Local Network
//  permission prompt correctly and picks the right interface, which a raw BSD
//  socket wouldn't. Framing: u32 BE length + payload, capped at 1 MiB, checked
//  before allocating.
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
    /// Links opened for a dial that has not been answered yet. See `answerDial`.
    private var dialled: [UInt64: UInt64] = [:]
    private var nextLink: UInt64 = 1
    private var browser: NWBrowser?
    /// Watches for this phone being on a local network again. See `watchPath`.
    private var pathWatch: NWPathMonitor?
    /// Whether the last path we were told about was a local network, so that
    /// only changes are acted on rather than every interface update.
    private var onLan = false
    /// Addresses announced to the core, so they can be withdrawn later — only
    /// what we actually announced, or withdrawal would be noise.
    private var announced: Set<String> = []
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
    /// The counter is ours; the id is not. `linkId` namespaces it by transport
    /// so two transports counting from 1 can't collide in the core's link table.
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
        // Both tables, or a link retired before it ever came up leaves its dial
        // token behind for as long as the transport lives.
        dialled.removeValue(forKey: link)
        return connections.removeValue(forKey: link)
    }

    /// Claim the dial this link was opened for, if it is still unanswered.
    ///
    /// A dial is answered once, whichever comes first: `.ready`, or failing
    /// before it ever came up. Removing the token from the map is the claim.
    private func answerDial(_ link: UInt64) -> UInt64? {
        lock.lock(); defer { lock.unlock() }
        return dialled.removeValue(forKey: link)
    }

    private func noteDial(_ link: UInt64, _ dial: UInt64) {
        lock.lock(); defer { lock.unlock() }
        dialled[link] = dial
    }

    /// Tell the core a link died, exactly once.
    ///
    /// Several signals can report the same drop; removing the link from the
    /// table is the claim, so only one caller ever reports it.
    @discardableResult
    private func retire(_ link: UInt64, _ reason: FfiLinkDown) -> NWConnection? {
        guard let dead = release(link) else { return nil }
        fire(.linkDown(link: link, reason: reason))
        return dead
    }

    // MARK: - Transport

    public func start(events: @escaping @Sendable (FfiEvent) -> Void) async {
        setEmit(events)
    }

    /// Addresses are opaque to the core; only these two shapes come back:
    /// - `bonjour:<instance>` — resolved by Network.framework, not synthesized,
    ///   since `NWBrowser` won't resolve SRV while browsing.
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
        attach(NWConnection(to: endpoint, using: Self.tcp), dial: token)
    }

    /// Give up on a dial that is going nowhere, and hang up behind it.
    ///
    /// Network.framework sits in `.waiting` forever with no viable path (Wi-Fi
    /// off, say) and never calls `stateUpdateHandler`. The core has its own,
    /// longer timeout for this, so this one must fire first — only this end
    /// holds the connection and can clean it up.
    private func boundDial(_ link: UInt64, _ c: NWConnection) {
        queue.asyncAfter(deadline: .now() + .milliseconds(Int(dialTimeoutMs()))) {
            [weak self] in
            guard let self, let pending = self.answerDial(link) else { return }
            _ = self.release(link)
            c.cancel()
            self.fire(.dialFailed(dial: pending, reason: "it never answered"))
        }
    }

    /// TCP with the same dead-peer budget the desktop uses, read from the core
    /// so the two numbers can't drift. Plain `.tcp` never questions an idle
    /// connection, so a sleeping peer's socket would sit ESTABLISHED forever.
    private static var tcp: NWParameters {
        // Start from `NWParameters.tcp` rather than building fresh — that
        // carries interface selection and path policy dialling needs.
        let params = NWParameters.tcp
        guard let options = params.defaultProtocolStack.transportProtocol as? NWProtocolTCP.Options
        else {
            return params
        }
        let budget = Int(deadPeerMs() / 1000)
        options.enableKeepalive = true
        options.keepaliveIdle = max(budget / 2, 1)
        options.keepaliveInterval = max(budget / 4, 1)
        options.keepaliveCount = 2
        return params
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

    /// Report every connection that is no longer usable.
    ///
    /// TCP outranks Bluetooth, so a dead Wi-Fi socket doesn't just fail — it
    /// keeps a working Bluetooth link from ever being chosen.
    public func revalidate() async {
        let held: [(UInt64, NWConnection)] = {
            lock.lock(); defer { lock.unlock() }
            return connections.map { ($0.key, $0.value) }
        }()
        for (link, c) in held {
            // Only the states that are over. `.preparing`/`.waiting` are still
            // trying and must not be retired, or a connection about to work dies.
            switch c.state {
            case .failed, .cancelled:
                // `retire` is the claim: only the caller that removes the link
                // reports, so this can't race the state handler.
                retire(link, .transport(detail: "the connection did not survive the background"))?
                    .cancel()
            default:
                break
            }
        }
    }

    public func advertise(enable: Bool, txt: [FfiTxt]) async {
        // Deliberately unimplemented: the phone always dials, never listens —
        // there's no way to accept an inbound connection while the app is closed.
    }

    public func discover(enable: Bool) async {
        guard enable else {
            stopWatchingPath()
            stopBrowse()
            return
        }
        startBrowse()
        watchPath()
    }

    public func rediscover() async {
        stopBrowse()
        startBrowse()
    }

    /// Notice when this phone is back on a network, and act on it.
    ///
    /// Regaining Wi-Fi announces nothing on its own — nothing broke, from the
    /// core's view. So this replaces the browse (one that lived through the
    /// outage may have failed and stays failed) and asks the core to look again.
    private func watchPath() {
        lock.lock()
        guard pathWatch == nil else { lock.unlock(); return }
        let m = NWPathMonitor()
        pathWatch = m
        lock.unlock()

        m.pathUpdateHandler = { [weak self] path in
            guard let self else { return }
            // A local network, not merely a route to the internet. Cellular
            // satisfies a path and reaches nothing on this one.
            let lan = path.status == .satisfied
                && (path.usesInterfaceType(.wifi) || path.usesInterfaceType(.wiredEthernet))
            // Edges only — dialling on every interface change would burn a
            // pocketed phone's radio.
            guard self.noteLan(lan), lan else { return }
            self.stopBrowse()
            self.startBrowse()
            self.fire(.reconsiderRoutes)
        }
        m.start(queue: queue)
    }

    /// Record what is on the network now, and hand back what has gone.
    private func withdraw(keeping present: Set<String>) -> [String] {
        lock.lock(); defer { lock.unlock() }
        let gone = announced.subtracting(present)
        announced = present
        return Array(gone)
    }

    /// Whether this is a change, rather than the same answer again.
    private func noteLan(_ now: Bool) -> Bool {
        lock.lock(); defer { lock.unlock() }
        if onLan == now { return false }
        onLan = now
        return true
    }

    private func stopWatchingPath() {
        lock.lock(); let m = pathWatch; pathWatch = nil; lock.unlock()
        m?.cancel()
    }

    private func stopBrowse() {
        lock.lock(); let b = browser; browser = nil; lock.unlock()
        b?.cancel()
    }

    private func startBrowse() {
        lock.lock()
        guard browser == nil else { lock.unlock(); return }
        let params = NWParameters()
        params.includePeerToPeer = false
        let b = NWBrowser(for: .bonjourWithTXTRecord(type: serviceType, domain: nil), using: params)
        browser = b
        lock.unlock()

        b.stateUpdateHandler = { [weak self] state in
            switch state {
            // `.waiting` on a Bonjour browse almost always means Local
            // Network permission was declined; there's no API to query it directly.
            case let .waiting(error):
                self?.fire(.dialFailed(
                    dial: 0,
                    reason: "local network permission appears to be denied (\(error))"
                ))
            case .failed:
                // Dropped, not restarted here — retrying immediately would
                // spin. The path watch rebuilds it once there's a network.
                self?.stopBrowse()
            default:
                break
            }
        }
        b.browseResultsChangedHandler = { [weak self] results, _ in
            guard let self else { return }
            var present: Set<String> = []
            for r in results {
                guard case let .service(name, _, _, _) = r.endpoint else { continue }
                var txt: NWTXTRecord?
                if case let .bonjour(record) = r.metadata { txt = record }
                // Hand the instance name back, not a resolved address. See
                // `dial`.
                let addr = "bonjour:\(name)"
                present.insert(addr)
                self.fire(.discovered(
                    transport: self.transportId,
                    peer: FfiDiscoveredPeer(
                        fingerprint: txt?["fp"],
                        name: txt?["n"] ?? name,
                        addr: addr,
                        pairing: txt?["pair"] == "1"
                    )
                ))
            }
            // Diffed against the last set, not the `changes` argument: `results`
            // is the full current set, so a rebuilt browse's removals aren't lost.
            for addr in self.withdraw(keeping: present) {
                self.fire(.undiscovered(transport: self.transportId, addr: addr))
            }
        }
        b.start(queue: queue)
    }

    // MARK: - connection plumbing

    private func attach(_ c: NWConnection, dial: UInt64?) {
        let link = claimLink(c)
        if let dial {
            noteDial(link, dial)
            boundDial(link, c)
        }
        c.stateUpdateHandler = { [weak self] state in
            guard let self else { return }
            switch state {
            case .ready:
                // The dial is answered here and nowhere else; after this,
                // anything that happens to the connection is a link going down.
                self.fire(.linkUp(link: link, attrs: tcpLanAttrs(transport: self.transportId),
                                  dial: self.answerDial(link)))
                self.receiveHeader(c, link: link)
            case let .failed(error):
                if let pending = self.answerDial(link) {
                    _ = self.release(link)
                    self.fire(.dialFailed(dial: pending, reason: "\(error)"))
                } else {
                    self.retire(link, .transport(detail: "\(error)"))
                }
            case .cancelled:
                // A dial cancelled before it ever came up still has to be
                // answered, or the core waits on it for as long as it lives.
                if let pending = self.answerDial(link) {
                    _ = self.release(link)
                    self.fire(.dialFailed(dial: pending, reason: "cancelled"))
                } else {
                    self.retire(link, .closed)
                }
            default:
                break
            }
        }
        // Turning Wi-Fi off doesn't fail the connection — it sits `.ready` and
        // silent while the kernel retransmits. Viability going false is the
        // signal; no waiting to see if it recovers, since redialing is cheap.
        c.viabilityUpdateHandler = { [weak self] viable in
            guard let self, !viable else { return }
            self.retire(link, .transport(detail: "the network went away"))?.cancel()
        }
        c.start(queue: queue)
    }

    private func receiveHeader(_ c: NWConnection, link: UInt64) {
        c.receive(minimumIncompleteLength: 4, maximumLength: 4) { [weak self] data, _, done, error in
            guard let self else { return }
            if error != nil || done {
                self.retire(link, .closed)
                return
            }
            guard let data, data.count == 4 else { return }
            let n = data.withUnsafeBytes { $0.load(as: UInt32.self).bigEndian }
            guard n <= Self.maxFrame else {
                // Refuse before allocating. A peer that claims more than the cap
                // is hung up on rather than believed.
                self.retire(link, .transport(detail: "frame of \(n) exceeds the cap"))
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
                self.retire(link, .closed)
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
