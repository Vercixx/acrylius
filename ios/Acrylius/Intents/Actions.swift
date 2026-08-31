#if canImport(AppIntents)

import AppIntents
import Foundation

/// Collects what the core reported during an intent run; intents have no
/// screen or `AppModel` but must answer with machine-reported state.
final class IntentSink: UiSink, @unchecked Sendable {
    private let lock = NSLock()
    private var catalog = PeerCatalog()
    private var exited: [String: FfiExited] = [:]

    public func emit(_ event: FfiUiEvent) {
        lock.lock(); defer { lock.unlock() }
        catalog.ingest(event)
        if case let .plugin(peer, cap, ty, body) = event,
           cap == capCommand(), ty == "exited",
           let e = try? decodeExited(body: body)
        {
            exited[peer] = e
        }
    }

    func commandOutcome(_ peer: String) -> FfiExited? {
        lock.lock(); defer { lock.unlock() }
        return exited[peer]
    }

    func screenLocked(_ peer: String) -> Bool? {
        lock.lock(); defer { lock.unlock() }
        return catalog[peer].session?.locked
    }

    func lastError(_ peer: String) -> String? {
        lock.lock(); defer { lock.unlock() }
        return catalog[peer].lastError
    }
}

enum IntentRunner {
    static func withPeer<T: Sendable>(
        _ pc: PCEntity,
        timeout: Duration = .seconds(20),
        _ body: @escaping @Sendable (CoreRuntime, String, IntentSink) async -> T?
    ) async throws -> T? {
        let store = try KeychainStore()
        let runtime = try CoreRuntime.bootstrap(name: "Acrylius", store: store)
        // Retained for the whole call: `setUi` holds it weakly.
        let sink = IntentSink()
        await runtime.setUi(sink)
        await runtime.add(transport: NWTransport(serviceType: serviceType(),
                                                 port: defaultPort()))
        await runtime.start()
        await runtime.submit(.connect(peer: pc.id))
        defer { Task { await runtime.stop() } }

        return try? await withThrowingTaskGroup(of: T?.self) { group in
            group.addTask { await body(runtime, pc.id, sink) }
            group.addTask {
                try await Task.sleep(for: timeout)
                return nil
            }
            let first = try await group.next()
            group.cancelAll()
            return first ?? nil
        }
    }

    /// Wait until the core reports a peer reachable, or give up.
    static func awaitReachable(_ runtime: CoreRuntime, _ peer: String) async -> Bool {
        for _ in 0..<80 {
            if await runtime.peers().contains(where: { $0.deviceId == peer && $0.reachable }) {
                return true
            }
            try? await Task.sleep(for: .milliseconds(100))
        }
        return false
    }

    /// Wait for the peer to report its screen in the requested state; the
    /// answer must be machine-reported, never "the request reached a socket".
    static func awaitScreen(
        _ sink: IntentSink, _ peer: String, locked: Bool
    ) async -> Bool {
        let budget = locked ? sessionLockBudgetMs() : sessionUnlockBudgetMs()
        let deadline = ContinuousClock.now.advanced(by: .milliseconds(Int(budget)))
        while ContinuousClock.now < deadline {
            if sink.screenLocked(peer) == locked { return true }
            try? await Task.sleep(for: .milliseconds(150))
        }
        return false
    }
}

struct LockPCIntent: AppIntent {
    static var title: LocalizedStringResource { "Lock PC" }
    static var description: IntentDescription { IntentDescription("Lock your computer's screen.") }
    static var openAppWhenRun: Bool { false }

    // Deliberately no `authenticationPolicy`: locking gives nothing away.
    // Unlocking asks. Do not make these consistent.

    @Parameter(title: "PC") var pc: PCEntity

    func perform() async throws -> some IntentResult & ProvidesDialog {
        let ok = try await IntentRunner.withPeer(pc) { runtime, peer, sink in
            guard await IntentRunner.awaitReachable(runtime, peer) else { return false }
            await runtime.submit(.pluginCommand(peer: peer, cap: capSession(),
                                                ty: "lock", body: Data()))
            return await IntentRunner.awaitScreen(sink, peer, locked: true)
        }
        let dialog: IntentDialog = ok == true ? "Locked \(pc.name)." : "Could not lock \(pc.name)."
        return .result(dialog: dialog)
    }
}

struct UnlockPCIntent: AppIntent {
    static var title: LocalizedStringResource { "Unlock PC" }
    static var description: IntentDescription { IntentDescription("Unlock your computer's screen.") }
    static var openAppWhenRun: Bool { false }

    /// Unlocking hands over a running session, so the phone must be the owner's.
    static var authenticationPolicy: IntentAuthenticationPolicy { .requiresAuthentication }

    @Parameter(title: "PC") var pc: PCEntity

    func perform() async throws -> some IntentResult & ProvidesDialog {
        let ok = try await IntentRunner.withPeer(pc) { runtime, peer, sink in
            guard await IntentRunner.awaitReachable(runtime, peer) else { return false }
            await runtime.submit(.pluginCommand(peer: peer, cap: capSession(),
                                                ty: "unlock", body: Data()))
            return await IntentRunner.awaitScreen(sink, peer, locked: false)
        }
        // Some screen lockers ignore the unlock signal, so this can fail on a
        // healthy machine.
        let dialog: IntentDialog =
            ok == true ? "Unlocked \(pc.name)." : "\(pc.name) did not unlock."
        return .result(dialog: dialog)
    }
}

struct WakePCIntent: AppIntent {
    static var title: LocalizedStringResource { "Wake PC" }
    static var description: IntentDescription { IntentDescription("Send a wake-up packet to your computer.") }
    static var openAppWhenRun: Bool { false }

    @Parameter(title: "PC") var pc: PCEntity

    init() {}

    /// Widget buttons pass the entity directly; the widget process would
    /// otherwise re-read a snapshot it already has via the entity query.
    init(pc: PCEntity) {
        self.pc = pc
    }

    func perform() async throws -> some IntentResult & ProvidesDialog {
        // No core here: the widget process cannot read the Keychain, and a
        // saved wake target already proves pairing. Try every MAC — only some
        // interfaces have wake-on-LAN enabled.
        guard let config = WakeTargets.load(for: pc.id),
              case let packets = config.macs.compactMap({ try? magicPacket(mac: $0) }),
              !packets.isEmpty
        else {
            let dialog: IntentDialog = "\(pc.name) has not told this phone how to wake it. Open it in the app once while it is awake."
            return .result(dialog: dialog)
        }
        var destinations: [String] = []
        if !config.lastIpv4.isEmpty { destinations.append(config.lastIpv4) }
        if !config.broadcast.isEmpty { destinations.append(config.broadcast) }
        var sent = false
        for packet in packets {
            sent = await MagicPacketSender.send(packet, to: destinations, port: config.port) || sent
        }
        let dialog: IntentDialog = sent ? "Sent a wake-up to \(pc.name)." : "Could not send it."
        return .result(dialog: dialog)
    }
}

struct RunCommandIntent: AppIntent {
    static var title: LocalizedStringResource { "Run a command" }
    static var description: IntentDescription { IntentDescription("Run one of the commands your computer offers.") }
    static var openAppWhenRun: Bool { false }

    @Parameter(title: "PC") var pc: PCEntity
    @Parameter(title: "Command") var command: String

    func perform() async throws -> some IntentResult & ProvidesDialog {
        let outcome = try await IntentRunner.withPeer(pc) { runtime, peer, sink -> FfiExited? in
            guard await IntentRunner.awaitReachable(runtime, peer) else { return nil }
            // Only an id from the computer's own list; command strings cannot
            // be sent, here or anywhere.
            await runtime.submit(.pluginCommand(peer: peer, cap: capCommand(), ty: "run",
                                                body: encodeRunRequest(id: command)))
            // Wait for the computer to report the exit, not for the send.
            let deadline = ContinuousClock.now.advanced(by: .seconds(15))
            while ContinuousClock.now < deadline {
                if let e = sink.commandOutcome(peer) { return e }
                try? await Task.sleep(for: .milliseconds(150))
            }
            return nil
        }
        let dialog: IntentDialog
        switch outcome?.code {
        case .some(0): dialog = "Ran \(command)."
        case .some(let code): dialog = "\(command) failed on \(pc.name) with code \(code)."
        case nil: dialog = "No answer from \(pc.name)."
        }
        return .result(dialog: dialog)
    }
}

#endif
