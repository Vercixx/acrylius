#if canImport(AppIntents)

import AppIntents
import Foundation

/// Shared plumbing for an intent that has to reach a computer.
///
/// An App Intent gets a few seconds of process life, which is the reason
/// sessions use a one-round-trip handshake. Everything here is
/// connect, do one thing, and stop.
/// What the core said while an intent was running.
///
/// An intent has no screen and no `AppModel`, but it needs the same answer they
/// use: what the machine reported after reading its own state back. Without one
/// of these an intent could only ever know that a request had been handed to a
/// socket, which is not the question Siri is about to answer out loud.
final class IntentSink: UiSink, @unchecked Sendable {
    private let lock = NSLock()
    private var catalog = PeerCatalog()
    /// How a `run` ended, which the catalogue does not keep because no screen
    /// shows it.
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
        runtime.setUi(sink)
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

    /// Wait for the peer to report its screen in the state we asked for.
    ///
    /// The same rule the app's own button uses, and for the same reason: the
    /// answer is what the machine reported after re-reading its own state, never
    /// that the request reached a socket. Sleeping two seconds and saying
    /// "Locked" had Siri confirm a lock that may not have happened — and often
    /// had not, since the desktop is allowed rather longer than that to watch
    /// its screen locker and be sure.
    ///
    /// The budget comes from the core, which is where both halves of it live.
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

    // Deliberately no `authenticationPolicy`.
    //
    // Locking a screen costs whoever is at the machine a password and gives
    // nothing away, so requiring Face ID to lock would be friction with no
    // safety behind it. Unlocking is the opposite, and asks. Do not make these
    // consistent with each other.

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
        // Several screen lockers act on the signal and several do not, so this
        // one really can fail on a machine that is working perfectly.
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

    /// A widget button knows which machine it is for, so it says so rather than
    /// going through the entity query — which in a widget process would mean
    /// reading a snapshot to find what the widget had already read.
    init(pc: PCEntity) {
        self.pc = pc
    }

    func perform() async throws -> some IntentResult & ProvidesDialog {
        // Waking needs no session, no reachable peer, and no identity, which is
        // the point: the machine is asleep. It needs only what that machine told
        // us while it was awake, which is on disk.
        //
        // Nor does it build a core to check the peer is paired. A saved wake
        // target already is that check — the daemon only sends one over an open
        // session — and standing up a core would need the Keychain, which the
        // widget process this also runs in cannot read.
        // Every MAC, not the first.
        //
        // The daemon sends the list in the order it believes in, and a machine
        // with wired and wireless interfaces has more than one — only some of
        // which are the one wake-on-LAN is actually enabled for. Taking the
        // first meant a machine that would wake perfectly well over its other
        // interface simply did not, with nothing to say why.
        guard let config = WakeTargets.load(for: pc.id),
              case let packets = config.macs.compactMap({ try? magicPacket(mac: $0) }),
              !packets.isEmpty
        else {
            // A concatenation is a String, not a literal, so it needs an
            // explicit IntentDialog like every other branch here.
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
            // An id from the computer's own list. There is no way to send a
            // command string, here or anywhere.
            await runtime.submit(.pluginCommand(peer: peer, cap: capCommand(), ty: "run",
                                                body: encodeRunRequest(id: command)))
            // The computer says how it ended, and waiting for that is the only
            // way to know. Sleeping three seconds and saying "Ran it" reported a
            // success for a command that may not have started, may still be
            // running, and may have failed.
            let deadline = ContinuousClock.now.advanced(by: .seconds(15))
            while ContinuousClock.now < deadline {
                if let e = sink.commandOutcome(peer) { return e }
                try? await Task.sleep(for: .milliseconds(150))
            }
            return nil
        }
        let dialog: IntentDialog
        switch outcome??.code {
        case .some(0): dialog = "Ran \(command)."
        case .some(let code): dialog = "\(command) failed on \(pc.name) with code \(code)."
        case nil: dialog = "No answer from \(pc.name)."
        }
        return .result(dialog: dialog)
    }
}

#endif
