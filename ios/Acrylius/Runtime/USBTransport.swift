//
//  USB: the desktop dials in through an `iproxy` tunnel, so unlike TCP this
//  end listens on loopback and never dials. Framing matches NWTransport.
//

#if canImport(Network)

import Foundation
import Network

public final class USBTransport: Transport, @unchecked Sendable {
    public let transportId: UInt16
    private let port: NWEndpoint.Port

    private let lock = NSLock()
    private var emit: (@Sendable (FfiEvent) -> Void)?
    private var listener: NWListener?
    private var link: UInt64?
    private var connection: NWConnection?
    private var nextLink: UInt64 = 1

    private let queue = DispatchQueue(label: "org.acrylius.usb")

    public init(transportId: UInt16 = 0, port: UInt16 = 1972) {
        self.transportId = transportId
        self.port = NWEndpoint.Port(rawValue: port) ?? 1972
    }

    private func setEmit(_ f: @escaping @Sendable (FfiEvent) -> Void) {
        lock.lock(); emit = f; lock.unlock()
    }
    private func fire(_ e: FfiEvent) {
        lock.lock(); let f = emit; lock.unlock()
        f?(e)
    }
    private func claimLink() -> UInt64 {
        lock.lock(); defer { lock.unlock() }
        let id = linkId(transport: transportId, counter: nextLink)
        nextLink += 1
        return id
    }
    private func held(_ link: UInt64) -> NWConnection? {
        lock.lock(); defer { lock.unlock() }
        return self.link == link ? connection : nil
    }
    /// The claim: whoever clears the link is the one who reports it dead.
    private func release(_ link: UInt64) -> NWConnection? {
        lock.lock()
        guard self.link == link else { lock.unlock(); return nil }
        let c = connection
        self.link = nil
        connection = nil
        lock.unlock()
        return c
    }

    public func start(events: @escaping @Sendable (FfiEvent) -> Void) async {
        setEmit(events)
    }

    /// A phone never accepts an unsolicited link, over USB or otherwise.
    public func advertise(enable: Bool, txt: [FfiTxt]) async {}

    /// USB has no discovery of its own — `iproxy` on the desktop is what
    /// finds this phone. Listening starts and stops with this call instead.
    public func discover(enable: Bool) async {
        NSLog("acrylius usb: discover(\(enable)) on port \(port)")
        guard enable else {
            stopListening()
            return
        }
        startListening()
    }

    /// The desktop dials over USB; this end only ever answers.
    public func dial(addr: String, token: UInt64) async {
        fire(.dialFailed(dial: token, reason: "USB is only ever dialled, never dials out"))
    }

    public func send(link: UInt64, msg: Data) async {
        guard let c = held(link) else { return }
        var header = UInt32(msg.count).bigEndian
        var frame = Data(bytes: &header, count: 4)
        frame.append(msg)
        c.send(content: frame, completion: .contentProcessed { _ in })
    }

    public func close(link: UInt64) async {
        release(link)?.cancel()
    }

    private func startListening() {
        lock.lock()
        guard listener == nil else { lock.unlock(); return }
        lock.unlock()
        // No `requiredInterfaceType` here — it can fail the bind silently.
        // Loopback-only is enforced in `accept` instead, per connection.
        let l: NWListener
        do {
            l = try NWListener(using: .tcp, on: port)
        } catch {
            NSLog("acrylius usb: could not create listener on port \(port): \(error)")
            return
        }
        lock.lock(); listener = l; lock.unlock()
        l.stateUpdateHandler = { state in
            NSLog("acrylius usb: listener state \(state)")
        }
        l.newConnectionHandler = { [weak self] conn in self?.accept(conn) }
        l.start(queue: queue)
    }

    private func stopListening() {
        NSLog("acrylius usb: stopping the listener")
        lock.lock()
        let l = listener
        listener = nil
        lock.unlock()
        l?.cancel()
        if let dead = release(link ?? 0) {
            dead.cancel()
        }
    }

    /// True only for a connection that terminates at this device's own
    /// loopback interface — the shape `iproxy`'s tunnel always arrives as.
    private func isLoopback(_ conn: NWConnection) -> Bool {
        guard case let .hostPort(host, _) = conn.endpoint else { return false }
        switch host {
        case .ipv4(let a): return a == IPv4Address("127.0.0.1")!
        case .ipv6(let a): return a == IPv6Address("::1")!
        default: return false
        }
    }

    /// A second tunnel connecting replaces whatever link is already held.
    private func accept(_ conn: NWConnection) {
        NSLog("acrylius usb: inbound connection from \(conn.endpoint)")
        if let old = release(link ?? 0) {
            old.cancel()
            fire(.linkDown(link: link ?? 0, reason: .closed))
        }
        let newLink = claimLink()
        lock.lock(); link = newLink; connection = conn; lock.unlock()
        conn.stateUpdateHandler = { [weak self] state in
            guard let self else { return }
            NSLog("acrylius usb: connection state \(state)")
            switch state {
            case .ready:
                guard self.isLoopback(conn) else {
                    NSLog("acrylius usb: refusing a non-loopback peer")
                    self.retire(newLink, .closed)
                    conn.cancel()
                    return
                }
                self.fire(.linkUp(link: newLink, attrs: usbAttrs(transport: self.transportId), dial: nil))
                self.receiveHeader(conn, link: newLink)
            case let .failed(error):
                self.retire(newLink, .transport(detail: "\(error)"))
            case .cancelled:
                self.retire(newLink, .closed)
            default:
                break
            }
        }
        conn.start(queue: queue)
    }

    @discardableResult
    private func retire(_ link: UInt64, _ reason: FfiLinkDown) -> NWConnection? {
        guard let dead = release(link) else { return nil }
        fire(.linkDown(link: link, reason: reason))
        return dead
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
            guard n <= NWTransport.maxFrame else {
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

#endif
