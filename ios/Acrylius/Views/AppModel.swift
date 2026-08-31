#if canImport(SwiftUI)

import Foundation
import Observation
import SwiftUI
#if canImport(WidgetKit)
import WidgetKit
#endif

/// What the views watch. Every field is a projection of something the core
/// said; it holds no protocol state of its own.
@Observable
@MainActor
final class AppModel {
    var peers: [FfiPeer] = []
    var catalog = PeerCatalog()
    var deviceId: String = ""
    var fingerprint: String = ""
    var capsIn: [String] = []
    var capsOut: [String] = []
    /// The subset of `capsIn` this phone can act on rather than only ask for.
    var capsServed: [String] = []

    /// The six digits both ends show during pairing; comparing them is what
    /// authenticates it. See `ConfirmPairingView`.
    var pairingSas: String?
    var pairingPeerName: String?
    var pairingPeerFingerprint: String?
    /// App-wide status; per-peer state is `FfiPeer.state`, not this.
    var status: AppStatus = .starting

    /// The last thing worth a line; nothing a screen should reason from.
    var activity: String?

    var lastError: String? {
        didSet { lastErrorAt = lastError == nil ? nil : Date() }
    }

    private(set) var lastErrorAt: Date?

    func dismissError() {
        lastError = nil
    }

    enum AppStatus: Equatable {
        case starting
        case failedToStart
        /// Running, with nothing paired yet.
        case noDevices
        case ready

        var text: String {
            switch self {
            case .starting: "Starting…"
            case .failedToStart: "Could not start"
            case .noDevices: "No devices paired"
            case .ready: "Ready"
            }
        }
    }
    /// Files offered but not yet finished, by transfer id.
    var sending: [UInt64: String] = [:]

    /// A file a computer has offered this phone, waiting on a person.
    struct IncomingOffer: Identifiable, Sendable, Equatable {
        var id: UInt64 { transfer }
        let transfer: UInt64
        let peer: String
        let name: String
        let size: UInt64
    }

    /// Offers nobody has answered yet. Deliberately never answered
    /// automatically: each transfer needs a person to say yes.
    var incoming: [IncomingOffer] = []

    /// A computer on this network that this phone is not paired with.
    struct Nearby: Identifiable, Equatable {
        var id: String { fingerprint }
        let fingerprint: String
        let name: String
        let addr: String
        /// Which transport saw it, and therefore which one can reach it — a
        /// `ble:` address must not be handed to the Wi-Fi transport.
        let transport: UInt16
        /// Whether it says it is already busy pairing with somebody.
        var pairing: Bool
        var seen: Date
    }

    /// What discovery has turned up.
    var nearby: [Nearby] = []

    /// Bluetooth diagnostics, filled in by the probe.
    let ble = BLEDiagnostics()

    private var runtime: CoreRuntime?
    #if canImport(CoreBluetooth)
        private var bluetooth: BLETransport?
    #endif

    /// A suspended app notices nothing: retire dead links, restart discovery,
    /// then have the core re-check the routes it already knows.
    func cameToForeground() async {
        await runtime?.revalidateLinks()
        await runtime?.rediscover()
        await runtime?.submit(.reconsiderRoutes)
        await refresh()
    }

    /// Called when the Bluetooth screen appears, not at launch:
    /// `CBCentralManager` raises the permission prompt on construction.
    func startBluetooth() {
        #if canImport(CoreBluetooth)
            bluetooth?.startManager()
        #else
            ble.managerState = "no CoreBluetooth in this build"
        #endif
    }

    func start() async {
        guard runtime == nil else { return }
        do {
            let store = try KeychainStore()
            let rt = try CoreRuntime.bootstrap(
                name: deviceName(),
                store: store,
                effector: IosEffector(),
                effects: IosEffector.kinds
            )
            let sink = Sink { [weak self] event in
                Task { @MainActor in self?.on(event) }
            }
            await rt.setUi(sink)
            self.sink = sink
            await rt.add(transport: NWTransport(
                serviceType: serviceType(),
                port: defaultPort()
            ))
            #if canImport(CoreBluetooth)
                // Transport 2, matching the daemon; routes are tried in
                // ascending transport order, so Wi-Fi wins. Must be added
                // before `start()`.
                let ble = BLETransport(transportId: 2) { [weak self] update in
                    Task { @MainActor in
                        self?.ble.apply(update)
                        if case let .trouble(message) = update {
                            self?.lastError = message
                        }
                    }
                }
                bluetooth = ble
                await rt.add(transport: ble)
            #endif
            await rt.start()
            runtime = rt
            deviceId = await rt.deviceId()
            fingerprint = await rt.fingerprint()
            peers = await rt.peers()
            capsIn = await rt.capsIn()
            capsOut = await rt.capsOut()
            capsServed = await rt.capsServed()
            status = peers.isEmpty ? .noDevices : .ready
        } catch {
            status = .failedToStart
            lastError = String(describing: error)
        }
    }

    /// Retained because `UiSink` is held weakly by the runtime.
    private var sink: Sink?

    /// Start pairing; six digits come back and each end confirms they match.
    func pair(at addr: String, transport: UInt16 = 1) async {
        await runtime?.submit(.requestPairing(transport: transport, addr: addr))
    }

    func confirmPairing(_ accept: Bool) async {
        await runtime?.submit(.confirmPairing(accept: accept))
        pairingSas = nil
    }

    /// Dial a peer now and wait until it is actually reachable; returns
    /// whether a session opened.
    @discardableResult
    func retry(_ peer: FfiPeer) async -> Bool {
        await runtime?.submit(.connect(peer: peer.deviceId))
        // Generous: a dial walks every route, and a Bluetooth handshake is slow.
        let deadline = ContinuousClock.now.advanced(by: .seconds(12))
        while ContinuousClock.now < deadline {
            await refresh()
            if peers.first(where: { $0.deviceId == peer.deviceId })?.reachable == true {
                return true
            }
            try? await Task.sleep(for: .milliseconds(400))
        }
        return false
    }

    func ping(_ peer: FfiPeer) async {
        await send(peer, cap: capPing(), ty: "ping", body: Data("hello".utf8))
    }

    @discardableResult
    func lock(_ peer: FfiPeer) async -> Bool {
        await send(peer, cap: capSession(), ty: "lock", body: Data())
        return await awaitScreen(peer, locked: true)
    }

    /// Unlocking hands over a running session, so it asks for Face ID first.
    /// Locking deliberately does not; keep the asymmetry.
    @discardableResult
    func unlock(_ peer: FfiPeer) async -> Bool {
        guard await confirmIdentity(reason: "Unlock \(peer.name)") else { return false }
        await send(peer, cap: capSession(), ty: "unlock", body: Data())
        let ok = await awaitScreen(peer, locked: false)
        if !ok {
            // logind only emits a signal; some lockers ignore it.
            lastError = "\(peer.name) did not unlock. Possible reason is that the locker does not support unlocking."
        }
        return ok
    }

    /// Wait for the peer to report the screen in the requested state. The
    /// budget comes from the core: a locally chosen one can be shorter than
    /// the desktop is allowed to take, reporting a lock that worked as failed.
    private func awaitScreen(_ peer: FfiPeer, locked: Bool) async -> Bool {
        let budget = locked ? sessionLockBudgetMs() : sessionUnlockBudgetMs()
        let deadline = ContinuousClock.now.advanced(by: .milliseconds(Int(budget)))
        while ContinuousClock.now < deadline {
            if catalog[peer.deviceId].session?.locked == locked { return true }
            try? await Task.sleep(for: .milliseconds(150))
        }
        return false
    }

    // MARK: - media

    /// Send a media command and wait for the peer to report the state after —
    /// a player may ignore a command or clamp a seek; only reading back says.
    @discardableResult
    func media(_ peer: FfiPeer, _ verb: String, player: String = "", value: Int64 = 0) async -> Bool {
        let before = catalog[peer.deviceId].media
        let beforeAt = catalog[peer.deviceId].mediaAt
        await send(
            peer,
            cap: capMedia(),
            ty: verb,
            body: encodeMediaCommand(player: player, value: value)
        )
        let deadline = ContinuousClock.now.advanced(by: .milliseconds(Int(mediaCommandBudgetMs())))
        while ContinuousClock.now < deadline {
            let features = catalog[peer.deviceId]
            // `mediaAt` moves on every reading: "something arrived".
            if features.mediaAt != beforeAt, let now = features.media {
                guard let before else { return true }
                guard let landed = mediaCommandLanded(
                    verb: verb, player: player, value: value, before: before, now: now
                ) else {
                    // Unanswerable from a reading (a seek moves a position
                    // that also moves on its own); the reading is the answer.
                    return true
                }
                // Not yet is not no: the poll can land between the command
                // and its reply, so keep waiting.
                if landed { return true }
            }
            try? await Task.sleep(for: .milliseconds(120))
        }
        return false
    }

    func refreshMedia(_ peer: FfiPeer) async {
        // Noted before sending so the reply can be timed at mid round trip.
        // See `PeerCatalog.measuredAt`.
        catalog.noteMediaQuery(for: peer.deviceId)
        await send(peer, cap: capMedia(), ty: "query", body: Data())
    }

    func refreshSession(_ peer: FfiPeer) async {
        await send(peer, cap: capSession(), ty: "query", body: Data())
    }

    /// Re-query a peer that has just come back: `PeerCatalog.ingest` clears
    /// session state on `peerUnreachable`, and nothing else refills it.
    private func reacquaint(with peerId: String) async {
        guard let peer = peers.first(where: { $0.deviceId == peerId }) else { return }
        await refreshSession(peer)
        await refreshMedia(peer)
    }

    func run(_ command: FfiCommand, on peer: FfiPeer) async {
        await send(peer, cap: capCommand(), ty: "run",
                   body: encodeRunRequest(id: command.id))
    }

    /// Fetch the peer's clipboard for display; returns whether an answer came
    /// back.
    @discardableResult
    func fetchClipboard(_ peer: FfiPeer) async -> Bool {
        let before = catalog[peer.deviceId].clipboardAt
        await send(peer, cap: capClipboard(), ty: "get", body: Data())
        let deadline = ContinuousClock.now.advanced(
            by: .milliseconds(Int(mediaCommandBudgetMs())))
        while ContinuousClock.now < deadline {
            // Arrival time, not value: fetching the same text twice succeeds
            // both times.
            if catalog[peer.deviceId].clipboardAt != before { return true }
            try? await Task.sleep(for: .milliseconds(120))
        }
        return false
    }

    /// Push text to the peer. The text must come from a `PasteButton` or text
    /// field: since iOS 16 a programmatic pasteboard read raises a prompt.
    @discardableResult
    func pushClipboard(_ text: String, to peer: FfiPeer) async -> Bool {
        // "push", not "changed": the latter is gated on a switch this phone
        // keeps off, and silently discarded a person pressing Paste.
        await send(peer, cap: capClipboard(), ty: "push", body: Data(text.utf8))
        activity = "Sent to \(peer.name)"
        return true
    }

    /// Send a wake packet. Unicast first: iOS cannot broadcast without an
    /// entitlement a free account cannot get.
    func wake(_ peer: FfiPeer) async -> Bool {
        // Catalogue falls back to disk: a sleeping machine opens no session.
        guard let config = catalog[peer.deviceId].wake
            ?? WakeTargets.load(for: peer.deviceId)
        else { return false }

        var destinations: [String] = []
        if !config.lastIpv4.isEmpty { destinations.append(config.lastIpv4) }
        if !config.broadcast.isEmpty { destinations.append(config.broadcast) }

        // Every MAC, not the first: wake-on-LAN is often enabled only on an
        // interface with no route.
        var sent = false
        for mac in config.macs {
            guard let packet = try? magicPacket(mac: mac) else { continue }
            if await MagicPacketSender.send(packet, to: destinations, port: config.port) {
                sent = true
            }
        }
        return sent
    }

    /// Offer a file: name, size and id go out; the bytes follow on their own
    /// connection once the computer accepts.
    func sendFile(_ file: FileOutbox.Outgoing, to peer: FfiPeer) async {
        guard let runtime else { return }
        let offer = await runtime.outbox.offer(file)
        sending[offer.transfer] = offer.name
        await send(peer, cap: capShare(), ty: "offer",
                   body: encodeShareOffer(offer: offer))
        activity = "Offered \(offer.name)"
    }

    /// Transfers this phone has agreed to and not yet seen the end of.
    var accepting: Set<UInt64> = []

    func accept(_ offer: IncomingOffer) async {
        accepting.insert(offer.transfer)
        await answer(offer, ty: "accept")
        activity = "Receiving \(offer.name)"
    }

    func decline(_ offer: IncomingOffer) async {
        incoming.removeAll { $0.transfer == offer.transfer }
        accepting.remove(offer.transfer)
        await answer(offer, ty: "reject")
        activity = "Declined \(offer.name)"
    }

    private func answer(_ offer: IncomingOffer, ty: String) async {
        let body = encodeShareEnd(
            end: FfiTransferEnd(transfer: offer.transfer, ok: true, detail: ""))
        await runtime?.submit(
            .pluginCommand(peer: offer.peer, cap: capShare(), ty: ty, body: body))
    }

    private func send(_ peer: FfiPeer, cap: String, ty: String, body: Data) async {
        await runtime?.submit(.pluginCommand(peer: peer.deviceId, cap: cap, ty: ty, body: body))
    }

    private func confirmIdentity(reason: String) async -> Bool {
        #if canImport(LocalAuthentication)
        let context = LAContext()
        var error: NSError?
        guard context.canEvaluatePolicy(.deviceOwnerAuthentication, error: &error) else {
            // No passcode set: proceed and say so rather than refuse outright.
            lastError = "You cannot use session controls if your device has no passcode."
            return true
        }
        return (try? await context.evaluatePolicy(.deviceOwnerAuthentication,
                                                  localizedReason: reason)) ?? false
        #else
        return true
        #endif
    }

    /// No refresh here on purpose: `submit` only queues, so a read-back races
    /// the core. `.revoked` is what redraws.
    func forget(_ peer: FfiPeer) async {
        await runtime?.submit(.revoke(peer: peer.deviceId))
    }

    private func refresh() async {
        guard let runtime else { return }
        peers = await runtime.peers()
        publishSnapshot()
    }

    /// The widget process can reach no computer; this snapshot is the only way
    /// anything gets to it.
    func publishSnapshot() {
        let wakeable = WakeTargets.known()
        SnapshotStore.save(peers: peers.map { peer in
            let features = catalog[peer.deviceId]
            return PeerSnapshot(
                deviceId: peer.deviceId,
                name: peer.name,
                platform: peer.platform,
                lastSeen: peer.reachable ? Date() : nil,
                locked: features.session?.locked,
                canWake: wakeable.contains(peer.deviceId),
                nowPlaying: features.activePlayer.flatMap { player in
                    guard !player.title.isEmpty else { return nil }
                    return player.artist.isEmpty
                        ? player.title
                        : "\(player.artist) — \(player.title)"
                }
            )
        })
        #if canImport(WidgetKit)
        WidgetCenter.shared.reloadAllTimelines()
        #endif
    }

    private func on(_ event: FfiUiEvent) {
        if catalog.ingest(event) { publishSnapshot() }
        switch event {
        case let .pairingSas(name, fp, sas):
            pairingPeerName = name
            pairingPeerFingerprint = fp
            pairingSas = sas
        case let .discovered(fingerprint, name, addr, transport, pairing):
            // Keyed by fingerprint: mDNS re-resolves the same machine on any
            // change, and appending would duplicate rows.
            let found = Nearby(
                fingerprint: fingerprint, name: name, addr: addr,
                transport: transport, pairing: pairing, seen: Date())
            if let at = nearby.firstIndex(where: { $0.fingerprint == fingerprint }) {
                nearby[at] = found
            } else {
                nearby.append(found)
            }
            // A machine already busy pairing sorts last: a tap would be refused.
            nearby.sort { ($0.pairing ? 1 : 0, $0.name) < ($1.pairing ? 1 : 0, $1.name) }
        case let .undiscovered(fingerprint):
            nearby.removeAll { $0.fingerprint == fingerprint }
        case let .pairingComplete(_, name):
            pairingSas = nil
            status = .ready
            activity = "Paired with \(name)"
            Task { await refresh() }
        case let .revoked(peer):
            // Dropped here rather than re-read; a round trip would race.
            peers.removeAll { $0.deviceId == peer }
            publishSnapshot()
        case let .pairingFailed(reason):
            pairingSas = nil
            lastError = reason
        case let .peerReachable(peer, name):
            activity = "Connected to \(name)"
            Task {
                await refresh()
                await reacquaint(with: peer)
            }
        case .peerUnreachable:
            // Nothing said on purpose: the why is `FfiPeer.trouble`, read by
            // the row that draws it.
            Task { await refresh() }
        case let .plugin(peer, cap, ty, body):
            // Recorded twice: the inbox settles where the file lands (name
            // collisions resolved before agreeing); this list is what is shown.
            if cap == capShare(), ty == "offer",
               let offer = try? decodeShareOffer(body: body) {
                let from = peer
                Task { [weak self] in
                    await self?.runtime?.inbox.remember(
                        transfer: offer.transfer, peer: from,
                        name: offer.name, size: offer.size)
                }
                incoming.removeAll { $0.transfer == offer.transfer }
                incoming.append(IncomingOffer(
                    transfer: offer.transfer, peer: peer,
                    name: bulkSafeName(offered: offer.name), size: offer.size))
                activity = "\(offer.name) offered"
            }
            if cap == capShare(), ty == "finished" || ty == "reject",
               let end = try? decodeShareFinished(body: body) {
                // Direction decided before either record is torn down:
                // `sending` holds only what this phone offered.
                let offered = incoming.first { $0.transfer == end.transfer }?.name
                incoming.removeAll { $0.transfer == end.transfer }
                accepting.remove(end.transfer)
                let sent = sending.removeValue(forKey: end.transfer)
                let name = sent ?? offered ?? "the file"
                if ty == "reject" {
                    activity = "\(name) was refused"
                } else if end.ok {
                    activity = sent == nil ? "Saved \(name)" : "Sent \(name)"
                } else {
                    activity = "\(name) failed"
                    lastError = end.detail.isEmpty ? nil : end.detail
                }
            }
            // A fetched clipboard value goes onto the pasteboard: writing is
            // always allowed, only reading raises a prompt.
            if cap == capClipboard(), ty == "set",
               let value = try? decodeClipboard(body: body), !value.text.isEmpty {
                #if canImport(UIKit)
                UIPasteboard.general.string = value.text
                #endif
                activity = "Copied to this phone"
            }
        case let .error(peer, code, detail):
            // Local Network denial is silent — iOS offers no API to query it.
            lastError = detail.contains("local network")
                ? "Allow local network access in Settings → Privacy → Local Network."
                : "\(code): \(detail)"
            // `nil` peer means the failure belongs to this phone.
            if let peer {
                catalog.note(error: detail, for: peer)
            }
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
