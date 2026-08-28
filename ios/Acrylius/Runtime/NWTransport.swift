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
    /// Links opened for a dial that has not been answered yet. See `answerDial`.
    private var dialled: [UInt64: UInt64] = [:]
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
        // Both tables, or a link retired before it ever came up leaves its dial
        // token behind for as long as the transport lives.
        dialled.removeValue(forKey: link)
        return connections.removeValue(forKey: link)
    }

    /// Claim the dial this link was opened for, if it is still unanswered.
    ///
    /// A dial is answered once, by whichever comes first: the connection going
    /// `.ready`, or it failing before it ever did. Removing the token is the
    /// claim, the same trick `retire` uses, so the two cannot both report.
    ///
    /// This exists because the token used to be captured for the connection's
    /// whole life. A link that came up and *then* died still had one, so it
    /// reported `dialFailed` for a dial that had already succeeded — and the
    /// core, which had recorded `LinkUp` and was routing over it, never heard
    /// `LinkDown`. The link stayed up forever, the peer stayed reachable over a
    /// route that carried nothing, and no plugin was ever told the peer had
    /// gone. Wi-Fi outranks Bluetooth, so everything went into the dead one.
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
    /// One dropped connection is noticed by several things at once — a read
    /// that errors, a state change to `.failed`, then `.cancelled` behind it,
    /// and the viability handler — and the core must hear about it once.
    /// Removing it from the table is the claim, and only the caller who
    /// succeeds in removing it gets to report.
    /// Returns the connection it removed, so a caller that also wants to hang
    /// up can do so without capturing it — a handler stored *on* a connection
    /// that captures that connection never lets it go.
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
        attach(NWConnection(to: endpoint, using: Self.tcp), dial: token)
    }

    /// Give up on a dial that is going nowhere, and hang up behind it.
    ///
    /// Network.framework waits for connectivity rather than failing: a
    /// connection with no viable path sits in `.waiting` for as long as it
    /// takes, which with Wi-Fi switched off is forever. `stateUpdateHandler`
    /// answers a dial on `.ready`, `.failed` and `.cancelled`, and none of
    /// those arrive — so the core was left holding a route walk that could not
    /// continue, and never tried the Bluetooth route behind it.
    ///
    /// The core bounds this too, but later and on purpose. Only this end holds
    /// the connection, so only this end can stop it, and a backstop that fired
    /// first would take the answer away from the half that can clean up.
    private func boundDial(_ link: UInt64, _ c: NWConnection) {
        queue.asyncAfter(deadline: .now() + .milliseconds(Int(dialTimeoutMs()))) {
            [weak self] in
            guard let self, let pending = self.answerDial(link) else { return }
            _ = self.release(link)
            c.cancel()
            self.fire(.dialFailed(dial: pending, reason: "it never answered"))
        }
    }

    /// TCP with the same dead-peer budget the desktop uses.
    ///
    /// `.tcp` on its own is the default, and the default never questions an
    /// idle connection at all: a computer that goes to sleep closes nothing, so
    /// the phone went on holding an ESTABLISHED socket and reporting the peer
    /// as connected indefinitely. The Linux runtime has bounded this since M2;
    /// this is the same number, read from the core so the two cannot drift.
    ///
    /// `connectionDropTime` is the half `TCP_USER_TIMEOUT` covers on Linux —
    /// bytes already in the send queue to a peer that has stopped answering,
    /// which keepalive alone does not notice because the connection is not
    /// idle.
    private static var tcp: NWParameters {
        // `NWParameters.tcp` and then reach into its stack, rather than
        // building parameters from scratch. Constructing them fresh means
        // opting out of every default the convenience carries — interface
        // selection, path policy, how a Bonjour endpoint is resolved — and
        // those defaults are why dialling worked. Losing them stopped the app
        // connecting over Wi-Fi at all.
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
    /// `.ready` is the only state that can carry a frame. Anything else here is
    /// a link the core still believes in and would go on choosing — and TCP
    /// outranks Bluetooth, so a dead Wi-Fi socket does not merely fail, it
    /// keeps a working Bluetooth link from ever being picked.
    public func revalidate() async {
        let held: [(UInt64, NWConnection)] = {
            lock.lock(); defer { lock.unlock() }
            return connections.map { ($0.key, $0.value) }
        }()
        for (link, c) in held {
            // Only the states that are over.
            //
            // `!= .ready` was too much: `.preparing` is a dial in flight and
            // `.waiting` is one the system intends to retry, and retiring
            // those killed connections that were about to work — including,
            // at launch, the very first one. A connection that is still trying
            // is not a link that died while the app was away.
            switch c.state {
            case .failed, .cancelled:
                // `retire` is the claim: only the caller who removes it
                // reports, so this cannot race the state handler into
                // reporting the same link twice.
                retire(link, .transport(detail: "the connection did not survive the background"))?
                    .cancel()
            default:
                break
            }
        }
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
        if let dial {
            noteDial(link, dial)
            boundDial(link, c)
        }
        c.stateUpdateHandler = { [weak self] state in
            guard let self else { return }
            switch state {
            case .ready:
                // The dial is answered here and nowhere else. After this the
                // connection is a link, and anything that happens to it is a
                // link going down — never a dial that failed.
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
        // The direct answer to "Wi-Fi was switched off".
        //
        // Turning Wi-Fi off does not fail an established connection — nothing
        // is closed, the peer simply stops answering, and the socket sits
        // `.ready` and silent while the kernel retransmits. The core ranks
        // transports by id and Wi-Fi outranks Bluetooth, so until this link is
        // retired every message is routed into a connection that cannot carry
        // it, past a Bluetooth link that is up and working. The app says
        // "connected", and nothing happens.
        //
        // iOS knows the moment it happens and will say so, which is far better
        // than any timeout: viability going false means the path this
        // connection runs over can no longer carry traffic. There is no waiting
        // to see whether it recovers — the core re-dials when discovery finds
        // the desktop again, and being wrong for a second costs a redial, while
        // being right and slow costs every message in between.
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
