//
//  The single serial executor.
//
//  ONE task owns the core. Nothing else ever touches it.
//
//  Every input arrives as an event on a stream this task drains one at a time,
//  and every action it produces is carried out before the next event is read.
//  `handle()` is therefore never called from inside an action handler.
//
//  This is deliberately *not* a plain `actor` method that awaits. Swift actors
//  are reentrant: an actor method that suspends can be interleaved with another
//  call to the same actor, which is exactly the hazard this design exists to
//  remove. A stream with one consumer gives real serialisation.
//

import Foundation

public actor CoreRuntime {
    private let core: AcryliusCore
    private let store: Store
    private let effector: Effector
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
        let key: Data
        if let existing = store.identityKey() {
            key = existing
        } else {
            key = generateIdentity()
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
            // Transports run on their own tasks and only ever yield. This is the
            // one-way door that makes reentrancy impossible.
            Task { await self?.submit(event) }
        }
    }

    /// Hand the core an event. Returns immediately; the pump does the work.
    public func submit(_ event: FfiEvent) {
        events?.yield(event)
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

    /// Milliseconds since the Unix epoch.
    ///
    /// For the handshake timestamp only. The peer compares it against its own
    /// clock, so it has to be a clock they can both name — an uptime means
    /// nothing to anyone else, and sending one had every session refused as
    /// stale.
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
            ui?.emit(.error(code: "bad_input", detail: String(describing: error)))
            return
        }
        for action in outcome.actions {
            await apply(action)
        }
        arm(outcome.nextDeadlineMs)
    }

    /// Exactly one timer, re-armed on every outcome. The core hands back a
    /// single absolute deadline precisely so a host never has to keep a set of
    /// timer identifiers in step with it.
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
            // A link belongs to one transport, but the core does not track
            // which, so offer it to each and let the owner act.
            for t in transports.values { await t.send(link: link, msg: msg) }

        case let .close(link):
            for t in transports.values { await t.close(link: link) }

        case let .effect(token, effect):
            // Off to its own task so a slow effector cannot stall the pump. Its
            // answer arrives as an ordinary event.
            Task { [weak self, effector] in
                let result = await effector.run(effect)
                await self?.submit(.effectDone(token: token, result: result))
            }

        case let .persist(key, value, sensitivity):
            do {
                try store.put(key: key, value: value, sensitivity: sensitivity)
            } catch {
                ui?.emit(.error(code: "persist_failed", detail: "\(key): \(error)"))
            }

        case let .advertise(transport, enable, txt):
            await transports[transport]?.advertise(enable: enable, txt: txt)

        case let .discover(transport, enable):
            await transports[transport]?.discover(enable: enable)

        case let .ui(event):
            ui?.emit(event)
        }
    }
}
