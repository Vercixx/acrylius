#if canImport(SwiftUI)

import Foundation
import Observation
import SwiftUI

/// What the views watch.
///
/// It holds no protocol state of its own — every field here is a projection of
/// something the core said. That keeps the "one implementation" property honest
/// all the way to the screen: the UI cannot disagree with the core, because it
/// has nothing to disagree with.
@Observable
@MainActor
final class AppModel {
    var peers: [FfiPeer] = []
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
        await runtime?.submit(.pluginCommand(
            peer: peer.deviceId, cap: "org.acrylius.ping/1", ty: "ping",
            body: Data("hello".utf8)
        ))
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

#endif
