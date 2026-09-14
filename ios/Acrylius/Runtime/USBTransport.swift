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
        // Loopback only: reachable exclusively through the desktop's own
        // `iproxy` tunnel, never over Wi-Fi.
        let params = NWParameters.tcp
        params.requiredInterfaceType = .loopback
        guard let l = try? NWListener(using: params, on: port) else { return }
        lock.lock(); listener = l; lock.unlock()
        l.newConnectionHandler = { [weak self] conn in self?.accept(conn) }
        l.start(queue: queue)
    }

    private func stopListening() {
        lock.lock()
        let l = listener
        listener = nil
        lock.unlock()
        l?.cancel()
        if let dead = release(link ?? 0) {
            dead.cancel()
        }
    }

    /// A second tunnel connecting replaces whatever link is already held.
    private func accept(_ conn: NWConnection) {
        if let old = release(link ?? 0) {
            old.cancel()
            fire(.linkDown(link: link ?? 0, reason: .closed))
        }
        let newLink = claimLink()
        lock.lock(); link = newLink; connection = conn; lock.unlock()
        conn.stateUpdateHandler = { [weak self] state in
            guard let self else { return }
            switch state {
            case .ready:
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
