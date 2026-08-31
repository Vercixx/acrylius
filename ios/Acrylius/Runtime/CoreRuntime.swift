//
//  Single serial executor: one task owns the core, draining events one at a
//  time so `handle()` is never reentered. Not a plain actor method — actors
//  are reentrant across suspension, a single-consumer stream is not.
//

import Foundation

public actor CoreRuntime {
    private let core: AcryliusCore
    private let store: Store
    private let effector: Effector
    /// Holds offered files' paths so the core, plugins and peers never do.
    public let outbox = FileOutbox()
    public let inbox = FileInbox()
    private weak var ui: UiSink?

    private var transports: [UInt16: any Transport] = [:]
    private var events: AsyncStream<FfiEvent>.Continuation?
    private var pump: Task<Void, Never>?
    private var timer: Task<Void, Never>?

    /// Monotonic. Deadlines must not move when the wall clock does, or a pairing
    /// window could be extended by changing the time.
    private let origin = ContinuousClock.now

    public init(core: AcryliusCore, store: Store, effector: Effector = NullEffector()) {
        self.core = core
        self.store = store
        self.effector = effector
    }

    /// Build a core from stored state, generating an identity on first run.
    public static func bootstrap(
        name: String,
        platform: String = "ios",
        store: Store,
        effector: Effector = NullEffector(),
        effects: [FfiEffectKind] = []
    ) throws -> CoreRuntime {
        // A failed read must not fall through to generate-and-overwrite: on a
        // locked phone the read can fail even though an identity exists.
        let key: Data
        if let existing = try store.identityKey() {
            key = existing
        } else {
            key = try generateIdentity()
            try store.setIdentityKey(key)
        }
        let core = try AcryliusCore(
            config: defaultConfig(name: name, platform: platform),
            identityKey: key,
            peers: store.loadPeers(),
            effects: effects
        )
        return CoreRuntime(core: core, store: store, effector: effector)
    }

    public func setUi(_ sink: UiSink) { ui = sink }

    public func add(transport: any Transport) async {
        transports[transport.transportId] = transport
        await transport.start { [weak self] event in
            // Transports only ever yield here: the one-way door against reentrancy.
            Task { await self?.submit(event) }
        }
    }

    /// Hand the core an event. Returns immediately; the pump does the work.
    public func submit(_ event: FfiEvent) {
        events?.yield(event)
    }

    /// On return to foreground: a suspended process notices nothing, so held
    /// links are questioned rather than trusted. See `Transport.revalidate`.
    public func revalidateLinks() async {
        for t in transports.values {
            await t.revalidate()
        }
    }

    /// Same reason as `revalidateLinks`: a browse that failed while the app was
    /// suspended stays failed, and nothing else would replace it.
    public func rediscover() async {
        for t in transports.values {
            await t.rediscover()
        }
    }

    public func start() {
        guard pump == nil else { return }
        let (stream, continuation) = AsyncStream<FfiEvent>.makeStream(bufferingPolicy: .unbounded)
        events = continuation
        pump = Task { [weak self] in
            for await event in stream {
                await self?.step(event)
            }
        }
        Task {
            let txt = [
                FfiTxt(key: "v", value: "1"),
                FfiTxt(key: "fp", value: core.fingerprint()),
                FfiTxt(key: "id", value: core.deviceId()),
            ]
            for t in transports.values {
                await t.advertise(enable: true, txt: txt)
                await t.discover(enable: true)
            }
        }
    }

    public func stop() {
        events?.finish()
        pump?.cancel()
        timer?.cancel()
        pump = nil
    }

    public func peers() -> [FfiPeer] { core.peers() }
    public func deviceId() -> String { core.deviceId() }
    public func fingerprint() -> String { core.fingerprint() }
    public func pendingSas() -> String? { core.pendingSas() }
    public func capsIn() -> [String] { core.capsIn() }
    public func capsOut() -> [String] { core.capsOut() }
    public func capsServed() -> [String] { core.capsServed() }

    /// Unix-epoch milliseconds, for the handshake timestamp only: the peer
    /// compares it against its own clock, so an uptime would read as stale.
    private func wallMs() -> UInt64 {
        UInt64(max(0, Date().timeIntervalSince1970 * 1000))
    }

    /// Monotonic milliseconds since this runtime started. Deadlines only, so
    /// that changing the system clock cannot extend a pairing window.
    private func nowMs() -> UInt64 {
        let elapsed = origin.duration(to: .now)
        let (seconds, attoseconds) = elapsed.components
        return UInt64(max(0, seconds)) * 1000 + UInt64(attoseconds / 1_000_000_000_000_000)
    }

    private func step(_ event: FfiEvent) async {
        let outcome: FfiOutcome
        do {
            outcome = try core.handle(monotonicMs: nowMs(), wallMs: wallMs(), event: event)
        } catch {
            // The core refused the event outright; no peer is involved.
            ui?.emit(.error(peer: nil, code: "bad_input", detail: String(describing: error)))
            return
        }
        for action in outcome.actions {
            await apply(action)
        }
        arm(outcome.nextDeadlineMs)
    }

    /// One timer, re-armed on every outcome: the core hands back a single
    /// absolute deadline so hosts never juggle timer identifiers.
    private func arm(_ deadline: UInt64?) {
        timer?.cancel()
        guard let deadline else { timer = nil; return }
        let delay = deadline > nowMs() ? deadline - nowMs() : 0
        timer = Task { [weak self] in
            try? await Task.sleep(for: .milliseconds(delay))
            guard !Task.isCancelled else { return }
            await self?.submit(.tick)
        }
    }

    private func apply(_ action: FfiAction) async {
        switch action {
        case let .dial(transport, addr, dial):
            await transports[transport]?.dial(addr: addr, token: dial)

        case let .linkSend(link, msg):
            // The core does not track which transport owns a link; offer to each.
            for t in transports.values { await t.send(link: link, msg: msg) }

        case let .close(link):
            for t in transports.values { await t.close(link: link) }

        case let .effect(token, effect):
            // Its own task so a slow effector cannot stall the pump.
            Task { [weak self, effector] in
                let result = await effector.run(effect)
                await self?.submit(.effectDone(token: token, result: result))
            }

        case let .persist(key, value, sensitivity):
            do {
                try store.put(key: key, value: value, sensitivity: sensitivity)
            } catch {
                ui?.emit(.error(peer: nil, code: "persist_failed", detail: "\(key): \(error)"))
            }

        case let .advertise(transport, enable, txt):
            await transports[transport]?.advertise(enable: enable, txt: txt)

        case let .discover(transport, enable):
            await transports[transport]?.discover(enable: enable)

        case let .bulkSend(transfer, endpoint, key):
            // Detached, so a blocking transfer cannot stall the pump. The
            // key comes from the session; anyone holding it could derive them all.
            Task.detached { [weak self, outbox] in
                guard let path = await outbox.path(for: transfer) else {
                    await self?.submit(.bulkFinished(
                        transfer: transfer, ok: false,
                        detail: "there is no file for that transfer"))
                    return
                }
                do {
                    let sent = try bulkSend(
                        transfer: transfer, endpoint: endpoint, key: key, path: path)
                    await self?.submit(.bulkFinished(
                        transfer: transfer, ok: true, detail: "\(sent) bytes"))
                } catch {
                    await self?.submit(.bulkFinished(
                        transfer: transfer, ok: false, detail: "\(error)"))
                }
                await outbox.forget(transfer)
            }

        case let .bulkListen(transfer, offeredAs, key, expectBytes):
            // Bind and answer with the endpoint before blocking in a task of
            // its own. A phone not on Wi-Fi has nowhere to dial, so fail now.
            guard let host = LocalAddress.wifiIPv4() else {
                submit(.bulkFinished(
                    transfer: transfer, ok: false,
                    detail: "this phone is not on Wi-Fi, so there is nowhere to send it"))
                await inbox.forget(transfer)
                return
            }
            guard let path = await inbox.destination(for: transfer) else {
                submit(.bulkFinished(
                    transfer: transfer, ok: false,
                    detail: "there is no offer for that transfer"))
                return
            }
            let listener: BulkListener
            do {
                listener = try BulkListener.bind(host: host)
            } catch {
                submit(.bulkFinished(
                    transfer: transfer, ok: false, detail: "\(error)"))
                await inbox.forget(transfer)
                return
            }
            // Before the receive: the sender cannot connect until told where.
            submit(.bulkListening(transfer: transfer, endpoint: listener.endpoint()))
            Task.detached { [weak self, inbox] in
                do {
                    // accept/receive split: gives up on a sender that never
                    // dials, not on a file still coming. `offeredAs` is the sender's id, not ours.
                    try listener.accept(transfer: offeredAs)
                    await self?.submit(.bulkStarted(transfer: transfer))
                    let got = try listener.receive(
                        key: key, expectBytes: expectBytes, path: path)
                    await self?.submit(.bulkFinished(
                        transfer: transfer, ok: true, detail: "\(got) bytes"))
                } catch {
                    await self?.submit(.bulkFinished(
                        transfer: transfer, ok: false, detail: "\(error)"))
                }
                await inbox.forget(transfer)
            }

        case let .bulkUnsupported(transfer):
            // Answered rather than ignored: a peer can't tell "never coming"
            // from "merely slow".
            submit(.bulkFinished(
                transfer: transfer,
                ok: false,
                detail: "this device cannot carry out that transfer"
            ))

        case let .ui(event):
            ui?.emit(event)
        }
    }
}
