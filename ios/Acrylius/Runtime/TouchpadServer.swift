//
//  Touchpad over USB: the desktop connects through an `iproxy` tunnel, says
//  hello with its device id, then reads touches. Wire in PROTOCOL.md.
//

#if canImport(Network)

import Foundation
import Network

public final class TouchpadServer: @unchecked Sendable {
    private let port: NWEndpoint.Port
    private let onHost: @Sendable (String?) -> Void

    private let lock = NSLock()
    private var listener: NWListener?
    /// Only a connection that has said hello; anything earlier cannot be sent to.
    private var connection: NWConnection?

    private let queue = DispatchQueue(label: "org.acrylius.touchpad")

    public init(port: UInt16 = 1972, onHost: @escaping @Sendable (String?) -> Void) {
        self.port = NWEndpoint.Port(rawValue: port) ?? 1972
        self.onHost = onHost
    }

    /// Safe to call repeatedly; a listener iOS failed while suspended is replaced.
    public func start() {
        lock.lock()
        guard listener == nil else { lock.unlock(); return }
        // No `requiredInterfaceType`: it can fail the bind silently. Loopback
        // is checked per connection instead.
        let l: NWListener
        do {
            l = try NWListener(using: .tcp, on: port)
        } catch {
            lock.unlock()
            NSLog("acrylius touchpad: could not listen on \(port): \(error)")
            return
        }
        listener = l
        lock.unlock()
        l.stateUpdateHandler = { [weak self] state in
            NSLog("acrylius touchpad: listener \(state)")
            guard case .failed = state, let self else { return }
            self.lock.lock()
            if self.listener === l { self.listener = nil }
            self.lock.unlock()
            l.cancel()
            self.queue.asyncAfter(deadline: .now() + 1) { [weak self] in self?.start() }
        }
        l.newConnectionHandler = { [weak self] conn in self?.accept(conn) }
        l.start(queue: queue)
    }

    /// `false` when no desktop is connected, so the caller can go another way.
    public func send(_ kind: UInt8, _ body: Data) async -> Bool {
        guard let c = lock.withLock({ connection }) else { return false }
        var header = UInt32(body.count + 1).bigEndian
        var frame = Data(bytes: &header, count: 4)
        frame.append(kind)
        frame.append(body)
        return await withCheckedContinuation { done in
            c.send(content: frame, completion: .contentProcessed { error in
                done.resume(returning: error == nil)
            })
        }
    }

    private static func isLoopback(_ conn: NWConnection) -> Bool {
        guard case let .hostPort(host, _) = conn.endpoint else { return false }
        switch host {
        case .ipv4(let a): return a == IPv4Address("127.0.0.1")!
        case .ipv6(let a): return a == IPv6Address("::1")!
        default: return false
        }
    }

    private func accept(_ conn: NWConnection) {
        guard Self.isLoopback(conn) else {
            NSLog("acrylius touchpad: refusing \(conn.endpoint)")
            conn.cancel()
            return
        }
        conn.stateUpdateHandler = { [weak self] state in
            switch state {
            case .ready: self?.readHello(conn)
            case .failed, .cancelled: self?.drop(conn)
            default: break
            }
        }
        conn.start(queue: queue)
    }

    private func readHello(_ conn: NWConnection) {
        conn.receive(minimumIncompleteLength: 4, maximumLength: 4) { [weak self] data, _, _, _ in
            guard let data, data.count == 4 else { conn.cancel(); return }
            let n = Int(data.reduce(UInt32(0)) { $0 << 8 | UInt32($1) })
            guard (1...256).contains(n) else { conn.cancel(); return }
            self?.readHost(conn, count: n)
        }
    }

    private func readHost(_ conn: NWConnection, count n: Int) {
        conn.receive(minimumIncompleteLength: n, maximumLength: n) { [weak self] data, _, _, _ in
            guard let data, data.count == n, let host = String(data: data, encoding: .utf8) else {
                conn.cancel()
                return
            }
            self?.serve(conn, host: host)
        }
    }

    private func serve(_ conn: NWConnection, host: String) {
        lock.lock()
        let old = connection
        connection = conn
        lock.unlock()
        old?.cancel()
        NSLog("acrylius touchpad: serving \(host)")
        onHost(host)
        watchForClose(conn)
    }

    /// The desktop sends nothing after hello; reading only notices it leave.
    private func watchForClose(_ conn: NWConnection) {
        conn.receive(minimumIncompleteLength: 1, maximumLength: 64) { [weak self] _, _, done, error in
            if done || error != nil {
                conn.cancel()
                return
            }
            self?.watchForClose(conn)
        }
    }

    private func drop(_ conn: NWConnection) {
        lock.lock()
        let held = connection === conn
        if held { connection = nil }
        lock.unlock()
        if held { onHost(nil) }
    }
}

#endif
