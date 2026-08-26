#if canImport(SwiftUI)

import Foundation
import Observation
import SwiftUI
#if canImport(WidgetKit)
import WidgetKit
#endif

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
    /// What this phone will accept, and what it may send. The same list every
    /// device registers; what differs is which of them it can actually serve.
    var capsIn: [String] = []
    var capsOut: [String] = []
    /// The subset of `capsIn` this phone can act on rather than only ask for.
    var capsServed: [String] = []

    /// The code shown during pairing. Both ends show it; the user compares.
    var pairingSas: String?
    var pairingPeerName: String?
    var pairingPeerFingerprint: String?
    var pairingCode: String?
    var status: String = "starting"
    var lastError: String?
    /// Files offered but not yet finished, by transfer id, so a screen can say
    /// what is in flight rather than going quiet between the tap and the reply.
    var sending: [UInt64: String] = [:]

    /// A file a computer has offered this phone, waiting on a person.
    struct IncomingOffer: Identifiable, Sendable, Equatable {
        var id: UInt64 { transfer }
        let transfer: UInt64
        let peer: String
        let name: String
        let size: UInt64
    }

    /// Offers nobody has answered yet.
    ///
    /// Deliberately not answered automatically. A paired computer can put a
    /// file on this phone only because someone here said so, each time — the
    /// same rule the desktop follows, and worth keeping on the device that
    /// travels.
    var incoming: [IncomingOffer] = []

    /// What Bluetooth is doing. A projection like everything else here, filled
    /// in by the probe; the model decides nothing about the radio itself.
    let ble = BLEDiagnostics()

    private var runtime: CoreRuntime?
    #if canImport(CoreBluetooth)
        private var bluetooth: BLETransport?
    #endif

    /// Start scanning, raising the permission prompt if it has not been asked.
    ///
    /// Called when the Bluetooth screen appears rather than at launch, because
    /// `CBCentralManager` prompts the moment it is constructed and a prompt with
    /// nothing on screen to explain it is one people refuse — which only Settings
    /// undoes. Once allowed, `start()` brings it up on every launch without
    /// anyone visiting this screen again.
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
                // Transport 2, matching the daemon. The core tries routes in
                // ascending transport order, so Wi-Fi is preferred and this is
                // the fallback. Added before `start()`, which sends
                // advertise/discover to whatever transports exist at that moment.
                let ble = BLETransport(transportId: 2) { [weak self] update in
                    Task { @MainActor in self?.ble.apply(update) }
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

    @discardableResult
    func lock(_ peer: FfiPeer) async -> Bool {
        await send(peer, cap: capSession(), ty: "lock", body: Data())
        return await awaitScreen(peer, locked: true)
    }

    /// Unlocking hands over a running session, so it asks for Face ID first.
    ///
    /// Locking deliberately does not, and that asymmetry is not an oversight:
    /// locking costs whoever is at the machine a password, while unlocking
    /// gives a session away. Do not "fix" this by making them consistent.
    @discardableResult
    func unlock(_ peer: FfiPeer) async -> Bool {
        guard await confirmIdentity(reason: "Unlock \(peer.name)") else { return false }
        await send(peer, cap: capSession(), ty: "unlock", body: Data())
        let ok = await awaitScreen(peer, locked: false)
        if !ok {
            // logind only emits a signal; acting on it is the screen locker's
            // choice, and several do not. Saying "unlocked" because the request
            // was delivered would be a lie the user cannot check from here.
            lastError = "\(peer.name) did not unlock. Possible reason is that the locker does not support unlocking."
        }
        return ok
    }

    /// Wait for the peer to report the screen in the state we asked for.
    ///
    /// The answer comes from what the machine reports after re-reading its own
    /// state, never from the request having been delivered.
    private func awaitScreen(_ peer: FfiPeer, locked: Bool) async -> Bool {
        let deadline = ContinuousClock.now.advanced(by: .seconds(8))
        while ContinuousClock.now < deadline {
            if catalog[peer.deviceId].session?.locked == locked { return true }
            try? await Task.sleep(for: .milliseconds(150))
        }
        return false
    }

    // MARK: - media

    /// Send a transport command and wait for the peer to say what happened.
    ///
    /// Every one of these is answered with the state afterwards rather than an
    /// acknowledgement, because a player may ignore a command, clamp a seek, or
    /// stop of its own accord — and only reading it back says which.
    @discardableResult
    func media(_ peer: FfiPeer, _ verb: String, player: String = "", value: Int64 = 0) async -> Bool {
        let before = catalog[peer.deviceId].media
        await send(
            peer,
            cap: capMedia(),
            ty: verb,
            body: encodeMediaCommand(player: player, value: value)
        )
        // The answer replaces the state, so waiting for it to differ is waiting
        // for the command to have landed.
        let deadline = ContinuousClock.now.advanced(by: .seconds(5))
        while ContinuousClock.now < deadline {
            if catalog[peer.deviceId].media != before { return true }
            try? await Task.sleep(for: .milliseconds(120))
        }
        return false
    }

    func refreshMedia(_ peer: FfiPeer) async {
        await send(peer, cap: capMedia(), ty: "query", body: Data())
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
    @discardableResult
    func pushClipboard(_ text: String, to peer: FfiPeer) async -> Bool {
        // "push", not "changed". The latter is the host reporting a change it
        // noticed by itself, and is gated on a switch this phone keeps off,
        // because reading the pasteboard here raises a system alert. A person
        // pressing Paste is not that, and was silently discarded by it.
        await send(peer, cap: capClipboard(), ty: "push", body: Data(text.utf8))
        status = "Sent to \(peer.name)"
        return true
    }

    /// Wake a sleeping machine.
    ///
    /// The phone sends the packet, because a sleeping machine is running
    /// nothing that could receive a request to wake. Unicast to the last known
    /// address first: a network interface matches the packet's payload and
    /// ignores its destination, and iOS cannot broadcast without an entitlement
    /// a free account cannot get.
    func wake(_ peer: FfiPeer) async -> Bool {
        // Disk before the live catalogue, which is empty until a session opens
        // — and a machine that is asleep will never open one. The computer
        // handed this over while it was awake, for exactly this moment.
        guard let config = catalog[peer.deviceId].wake
            ?? WakeTargets.load(for: peer.deviceId)
        else { return false }

        var destinations: [String] = []
        if !config.lastIpv4.isEmpty { destinations.append(config.lastIpv4) }
        if !config.broadcast.isEmpty { destinations.append(config.broadcast) }

        // Every card it named, not the first. A laptop routes over Wi-Fi and
        // Wake-on-Wireless is rarely enabled; the ethernet card that would
        // actually answer is often the one with no route on it. One small
        // datagram each, and whichever is listening wakes the machine.
        var sent = false
        for mac in config.macs {
            guard let packet = try? magicPacket(mac: mac) else { continue }
            if await MagicPacketSender.send(packet, to: destinations, port: config.port) {
                sent = true
            }
        }
        return sent
    }

    /// Offer a file to a computer.
    ///
    /// The path stays on this phone. What goes out is a name, a size and an id;
    /// the bytes follow on their own connection, once the computer has said yes
    /// and named somewhere to send them.
    func sendFile(_ file: FileOutbox.Outgoing, to peer: FfiPeer) async {
        guard let runtime else { return }
        let offer = await runtime.outbox.offer(file)
        sending[offer.transfer] = offer.name
        await send(peer, cap: capShare(), ty: "offer",
                   body: encodeShareOffer(offer: offer))
        status = "Offered \(offer.name)"
    }

    /// Say yes to a file. This is what makes the phone bind a port and wait.
    func accept(_ offer: IncomingOffer) async {
        // Left in the list until the transfer actually ends, so the row does
        // not vanish the instant it is tapped and leave nothing to look at
        // while the bytes move.
        await answer(offer, ty: "accept")
        status = "Receiving \(offer.name)"
    }

    func decline(_ offer: IncomingOffer) async {
        incoming.removeAll { $0.transfer == offer.transfer }
        await answer(offer, ty: "reject")
        status = "Declined \(offer.name)"
    }

    /// An answer to an offer is the transfer's id and nothing more; the plugin
    /// looks the rest up from what it already holds.
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
            // No passcode set. Refusing outright would make the app unusable on
            // a device its owner chose not to lock, so this proceeds and says so.
            lastError = "You cannot use session controls if your device has no passcode."
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
        publishSnapshot()
    }

    /// Leave the widget something to draw.
    ///
    /// Called wherever the peer list or what a peer told us changes. The widget
    /// runs in a process that can open no session and reach no computer, so
    /// this is the only way anything ever gets there.
    func publishSnapshot() {
        let wakeable = WakeTargets.known()
        SnapshotStore.save(peers: peers.map { peer in
            let features = catalog[peer.deviceId]
            return PeerSnapshot(
                deviceId: peer.deviceId,
                name: peer.name,
                platform: peer.platform,
                // Only ever set while it is true. Nothing writes a "no longer
                // reachable" time, because the reading is taken continuously
                // and the last true one is the answer.
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
        // A peer announcing its session, its players or how to wake it is
        // exactly what the widget draws, so anything the catalog accepted is
        // worth writing down. The daemon already withholds a media state that
        // changed only its position, so this is not once a second.
        if catalog.ingest(event) { publishSnapshot() }
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
        // The peer is bound now: answering an offer means naming who made it,
        // and until this phone could receive a file there was nothing here that
        // needed to know.
        case let .plugin(peer, cap, ty, body):
            if ty == "pong" { status = "pong" }
            // A computer wants to send this phone something. Recorded in two
            // places at once: the inbox settles where it would land, so a name
            // collision is resolved before anyone agrees to anything, and the
            // list here is what a person is actually shown.
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
                status = "\(offer.name) offered"
            }
            // Both ends report a transfer, because each knows only its own
            // half: this phone knows it finished writing, and only the computer
            // knows whether it kept the file.
            if cap == capShare(), ty == "finished" || ty == "reject",
               let end = try? decodeShareFinished(body: body) {
                incoming.removeAll { $0.transfer == end.transfer }
                let name = sending.removeValue(forKey: end.transfer) ?? "the file"
                if ty == "reject" {
                    status = "\(name) was refused"
                } else if end.ok {
                    status = "Sent \(name)"
                } else {
                    status = "\(name) failed"
                    lastError = end.detail.isEmpty ? nil : end.detail
                }
            }
            // A clipboard value that came back from a peer goes onto this
            // phone's pasteboard. Asking for a computer's clipboard and only
            // being shown it is not much use on a device whose whole point is
            // that you then paste it somewhere. Writing is always allowed; it
            // is reading that raises a prompt.
            if cap == capClipboard(), ty == "set",
               let value = try? decodeClipboard(body: body), !value.text.isEmpty {
                #if canImport(UIKit)
                UIPasteboard.general.string = value.text
                #endif
                status = "Copied to this phone"
            }
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
