#if canImport(SwiftUI)

import Foundation
import Observation
import SwiftUI

/// What the views watch.
///
/// It holds no protocol state of its own. Every field here is a projection of
/// something the core said. That keeps the "one implementation" property honest
/// all the way to the screen: the UI cannot disagree with the core, because it
/// has nothing to disagree with.
@Observable
@MainActor
final class AppModel {
    var peers: [FfiPeer] = []
    /// What each peer has announced it can do. A screen shows a button only
    /// when the peer said it has the thing behind it.
    var catalog = PeerCatalog()
    var deviceId: String = ""
    var fingerprint: String = ""

    /// The code shown during pairing. Both ends show it; the user compares.
    var pairingSas: String?
    var pairingPeerName: String?
    var pairingPeerFingerprint: String?
    var pairingCode: String?
    var status: String = "starting"
    var lastError: String?

    private var runtime: CoreRuntime?

    func start() async {
        guard runtime == nil else { return }
        do {
            let store = try KeychainStore()
            let rt = try CoreRuntime.bootstrap(name: deviceName(), store: store)
            let sink = Sink { [weak self] event in
                Task { @MainActor in self?.on(event) }
            }
            await rt.setUi(sink)
            self.sink = sink
            await rt.add(transport: NWTransport(
                serviceType: serviceType(),
                port: defaultPort()
            ))
            await rt.start()
            runtime = rt
            deviceId = await rt.deviceId()
            fingerprint = await rt.fingerprint()
            peers = await rt.peers()
            status = peers.isEmpty ? "not paired" : "ready"
        } catch {
            status = "failed to start"
            lastError = String(describing: error)
        }
    }

    /// Retained because `UiSink` is held weakly by the runtime.
    private var sink: Sink?

    func pair(withCode code: String, at addr: String) async {
        await runtime?.submit(.requestPairing(transport: 1, addr: addr, code: code))
    }

    func confirmPairing(_ accept: Bool) async {
        await runtime?.submit(.confirmPairing(accept: accept))
        pairingSas = nil
    }

    func connect(_ peer: FfiPeer) async {
        await runtime?.submit(.connect(peer: peer.deviceId))
    }

    func ping(_ peer: FfiPeer) async {
        await send(peer, cap: capPing(), ty: "ping", body: Data("hello".utf8))
    }

    func lock(_ peer: FfiPeer) async {
        await send(peer, cap: capSession(), ty: "lock", body: Data())
    }

    /// Unlocking hands over a running session, so it asks for Face ID first.
    ///
    /// Locking deliberately does not, and that asymmetry is not an oversight:
    /// locking costs whoever is at the machine a password, while unlocking
    /// gives a session away. Do not "fix" this by making them consistent.
    func unlock(_ peer: FfiPeer) async {
        guard await confirmIdentity(reason: "Unlock \(peer.name)") else { return }
        await send(peer, cap: capSession(), ty: "unlock", body: Data())
    }

    func refreshSession(_ peer: FfiPeer) async {
        await send(peer, cap: capSession(), ty: "query", body: Data())
    }

    func run(_ command: FfiCommand, on peer: FfiPeer) async {
        await send(peer, cap: capCommand(), ty: "run",
                   body: encodeRunRequest(id: command.id))
    }

    /// Fetch the peer's clipboard so it can be shown.
    ///
    /// The answer is displayed, not written to this device's pasteboard. Taking
    /// a value the user only asked to look at would be surprising, and it is
    /// what makes two devices fight over a selection.
    func fetchClipboard(_ peer: FfiPeer) async {
        await send(peer, cap: capClipboard(), ty: "get", body: Data())
    }

    /// Push text to the peer.
    ///
    /// The text has to come from a `PasteButton` or a text field, never from
    /// reading `UIPasteboard` ourselves: since iOS 16 a programmatic read of
    /// content that came from another app raises a system prompt. `changeCount`
    /// can be polled without one, so the app can notice a change and offer a
    /// button, but it cannot sync silently.
    func pushClipboard(_ text: String, to peer: FfiPeer) async {
        await send(peer, cap: capClipboard(), ty: "changed", body: Data(text.utf8))
    }

    /// Wake a sleeping machine.
    ///
    /// The phone sends the packet, because a sleeping machine is running
    /// nothing that could receive a request to wake. Unicast to the last known
    /// address first: a network interface matches the packet's payload and
    /// ignores its destination, and iOS cannot broadcast without an entitlement
    /// a free account cannot get.
    func wake(_ peer: FfiPeer) async -> Bool {
        guard let config = catalog[peer.deviceId].wake, let mac = config.macs.first
        else { return false }
        guard let packet = try? magicPacket(mac: mac) else { return false }
        var destinations: [String] = []
        if !config.lastIpv4.isEmpty { destinations.append(config.lastIpv4) }
        if !config.broadcast.isEmpty { destinations.append(config.broadcast) }
        return await MagicPacketSender.send(packet, to: destinations, port: config.port)
    }

    private func send(_ peer: FfiPeer, cap: String, ty: String, body: Data) async {
        await runtime?.submit(.pluginCommand(peer: peer.deviceId, cap: cap, ty: ty, body: body))
    }

    private func confirmIdentity(reason: String) async -> Bool {
        #if canImport(LocalAuthentication)
        let context = LAContext()
        var error: NSError?
        guard context.canEvaluatePolicy(.deviceOwnerAuthentication, error: &error) else {
            // No passcode set. Refusing outright would make the app unusable on
            // a device its owner chose not to lock, so this proceeds and says so.
            lastError = "This device has no passcode; unlocking was not confirmed."
            return true
        }
        return (try? await context.evaluatePolicy(.deviceOwnerAuthentication,
                                                  localizedReason: reason)) ?? false
        #else
        return true
        #endif
    }

    func forget(_ peer: FfiPeer) async {
        await runtime?.submit(.revoke(peer: peer.deviceId))
        await refresh()
    }

    private func refresh() async {
        guard let runtime else { return }
        peers = await runtime.peers()
    }

    private func on(_ event: FfiUiEvent) {
        catalog.ingest(event)
        switch event {
        case let .pairingWindowOpen(code, _):
            pairingCode = code
        case let .pairingSas(name, fp, sas):
            pairingPeerName = name
            pairingPeerFingerprint = fp
            pairingSas = sas
        case let .pairingComplete(_, name):
            pairingSas = nil
            pairingCode = nil
            status = "paired with \(name)"
            Task { await refresh() }
        case let .pairingFailed(reason):
            pairingSas = nil
            pairingCode = nil
            lastError = reason
        case let .peerReachable(_, name):
            status = "connected to \(name)"
            Task { await refresh() }
        case .peerUnreachable:
            status = "not connected"
            Task { await refresh() }
        case let .plugin(_, _, ty, _):
            if ty == "pong" { status = "pong" }
        case let .error(code, detail):
            // Local Network permission is the failure users actually hit, and
            // it is silent: iOS offers no API to query it. Say what to do.
            lastError = detail.contains("local network")
                ? "Allow local network access in Settings → Privacy → Local Network."
                : "\(code): \(detail)"
        }
    }

    private func deviceName() -> String {
        #if canImport(UIKit)
        return UIDevice.current.name
        #else
        return "iPhone"
        #endif
    }
}

/// Bridges the runtime's `UiSink` onto the main actor.
private final class Sink: UiSink, @unchecked Sendable {
    private let handler: @Sendable (FfiUiEvent) -> Void
    init(_ handler: @escaping @Sendable (FfiUiEvent) -> Void) { self.handler = handler }
    func emit(_ event: FfiUiEvent) { handler(event) }
}

#if canImport(UIKit)
import UIKit
#endif
#if canImport(LocalAuthentication)
import LocalAuthentication
#endif

#endif
