#if canImport(AppIntents)

import AppIntents
import Foundation

/// Shared plumbing for an intent that has to reach a computer.
///
/// An App Intent gets a few seconds of process life, which is the reason
/// sessions use a one-round-trip handshake. Everything here is
/// connect, do one thing, and stop.
enum IntentRunner {
    static func withPeer<T: Sendable>(
        _ pc: PCEntity,
        timeout: Duration = .seconds(8),
        _ body: @escaping @Sendable (CoreRuntime, String) async -> T?
    ) async throws -> T? {
        let store = try KeychainStore()
        let runtime = try CoreRuntime.bootstrap(name: "Acrylius", store: store)
        await runtime.add(transport: NWTransport(serviceType: serviceType(),
                                                 port: defaultPort()))
        await runtime.start()
        await runtime.submit(.connect(peer: pc.id))
        defer { Task { await runtime.stop() } }

        return try? await withThrowingTaskGroup(of: T?.self) { group in
            group.addTask { await body(runtime, pc.id) }
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
        let ok = try await IntentRunner.withPeer(pc) { runtime, peer in
            guard await IntentRunner.awaitReachable(runtime, peer) else { return false }
            await runtime.submit(.pluginCommand(peer: peer, cap: capSession(),
                                                ty: "lock", body: Data()))
            try? await Task.sleep(for: .seconds(2))
            return true
        }
        let dialog: IntentDialog = ok == true ? "Locked \(pc.name)." : "Could not reach \(pc.name)."
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
        let ok = try await IntentRunner.withPeer(pc) { runtime, peer in
            guard await IntentRunner.awaitReachable(runtime, peer) else { return false }
            await runtime.submit(.pluginCommand(peer: peer, cap: capSession(),
                                                ty: "unlock", body: Data()))
            try? await Task.sleep(for: .seconds(2))
            return true
        }
        let dialog: IntentDialog = ok == true ? "Unlocked \(pc.name)." : "Could not reach \(pc.name)."
        return .result(dialog: dialog)
    }
}

struct WakePCIntent: AppIntent {
    static var title: LocalizedStringResource { "Wake PC" }
    static var description: IntentDescription { IntentDescription("Send a wake-up packet to your computer.") }
    static var openAppWhenRun: Bool { false }

    @Parameter(title: "PC") var pc: PCEntity

    func perform() async throws -> some IntentResult & ProvidesDialog {
        // Waking needs no session and no reachable peer, which is the point: the
        // machine is asleep. It needs only what that machine told us while it
        // was awake, which is on disk.
        let store = try KeychainStore()
        guard let key = store.identityKey() else {
            let dialog: IntentDialog = "Acrylius is not set up yet."
            return .result(dialog: dialog)
        }
        let core = try AcryliusCore(
            config: defaultConfig(name: "Acrylius", platform: "ios"),
            identityKey: key,
            peers: store.loadPeers()
        )
        guard core.peers().contains(where: { $0.deviceId == pc.id }) else {
            let dialog: IntentDialog = "\(pc.name) is not paired."
            return .result(dialog: dialog)
        }
        guard let config = WakeTargets.load(for: pc.id),
              let mac = config.macs.first,
              let packet = try? magicPacket(mac: mac)
        else {
            // A concatenation is a String, not a literal, so it needs an
            // explicit IntentDialog like every other branch here.
            let dialog: IntentDialog = "\(pc.name) has not told this phone how to wake it. Open it in the app once while it is awake."
            return .result(dialog: dialog)
        }
        var destinations: [String] = []
        if !config.lastIpv4.isEmpty { destinations.append(config.lastIpv4) }
        if !config.broadcast.isEmpty { destinations.append(config.broadcast) }
        let sent = await MagicPacketSender.send(packet, to: destinations, port: config.port)
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
        let ok = try await IntentRunner.withPeer(pc) { runtime, peer in
            guard await IntentRunner.awaitReachable(runtime, peer) else { return false }
            // An id from the computer's own list. There is no way to send a
            // command string, here or anywhere.
            await runtime.submit(.pluginCommand(peer: peer, cap: capCommand(), ty: "run",
                                                body: encodeRunRequest(id: command)))
            try? await Task.sleep(for: .seconds(3))
            return true
        }
        let dialog: IntentDialog = ok == true ? "Ran \(command)." : "Could not reach \(pc.name)."
        return .result(dialog: dialog)
    }
}

struct AcryliusShortcuts: AppShortcutsProvider {
    static var appShortcuts: [AppShortcut] {
        AppShortcut(intent: WakePCIntent(), phrases: [
            "Wake my PC with \(.applicationName)",
            "Wake up my computer with \(.applicationName)",
        ], shortTitle: "Wake PC", systemImageName: "power")

        AppShortcut(intent: LockPCIntent(), phrases: [
            "Lock my PC with \(.applicationName)",
        ], shortTitle: "Lock PC", systemImageName: "lock")

        AppShortcut(intent: UnlockPCIntent(), phrases: [
            "Unlock my PC with \(.applicationName)",
        ], shortTitle: "Unlock PC", systemImageName: "lock.open")
    }
}

#endif
