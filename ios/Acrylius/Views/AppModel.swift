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
    /// What the app as a whole is doing.
    ///
    /// Not per-peer — that is `FfiPeer.state`, and conflating the two is what
    /// this replaced. The old `status` was a free-form string written from a
    /// dozen places, so "Receiving holiday.jpg" overwrote "connected to
    /// desktop" in the one row that displayed either, and neither could be
    /// tested for.
    var status: AppStatus = .starting

    /// The last thing that happened worth a line, and nothing a screen should
    /// reason from. Transfers and one-shot actions land here.
    var activity: String?

    /// Something went wrong and the person should know.
    ///
    /// Stamped on write so it can expire. Nothing used to clear this: an error
    /// from ten minutes ago sat at the bottom of a list as a grey footnote,
    /// indistinguishable from one that had just happened.
    var lastError: String? {
        didSet { lastErrorAt = lastError == nil ? nil : Date() }
    }

    private(set) var lastErrorAt: Date?

    // No lifetime any more. An error is an alert now, and an alert that closed
    // itself on a timer would be one you could miss by looking away — the
    // expiry existed because a banner had no other way to leave.

    func dismissError() {
        lastError = nil
    }

    /// What the app itself is doing. Deliberately small: anything that varies
    /// per peer belongs on the peer.
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

    /// A computer on this network that this phone is not paired with.
    struct Nearby: Identifiable, Equatable {
        /// The advertised fingerprint. Not an identity — a hint for telling
        /// two rows apart, which is all a list needs.
        var id: String { fingerprint }
        let fingerprint: String
        let name: String
        let addr: String
        /// Whether it says it has a pairing window open right now.
        var pairing: Bool
        var seen: Date
    }

    /// What discovery has turned up, newest sighting first.
    ///
    /// Empty until M3: the core kept every sighting in a private map with no
    /// accessor and no event, so the daemon knew every acrylius machine on the
    /// network and the phone could not be told about one.
    var nearby: [Nearby] = []

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
    /// Come back from the background properly.
    ///
    /// Two things, in this order. The links this process woke up believing in
    /// are questioned — a suspended app notices nothing, so a socket that died
    /// while it was away is still in the table and, because TCP outranks
    /// Bluetooth, is still the route everything is sent down. Then the peers
    /// are re-read, because whatever was retired has changed their state.
    ///
    /// The dialling itself is the core's: retiring a link makes the peer
    /// unreachable, and the reconnect heartbeat picks it up from there.
    func cameToForeground() async {
        await runtime?.revalidateLinks()
        await refresh()
    }

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
                    Task { @MainActor in
                        self?.ble.apply(update)
                        // A Bluetooth problem with a fix worth naming does not
                        // belong only on a debug screen someone has to go
                        // looking for. It is the same channel every other
                        // failure reports through.
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

    func pair(withCode code: String, at addr: String) async {
        await runtime?.submit(.requestPairing(transport: 1, addr: addr, code: code))
    }

    func confirmPairing(_ accept: Bool) async {
        await runtime?.submit(.confirmPairing(accept: accept))
        pairingSas = nil
    }

    /// Ask for a computer now, rather than waiting for the next heartbeat.
    ///
    /// Dialling is automatic and has been since Stage B, but "Not connected"
    /// reads as *gave up* — reasonably, since the state right next to it is
    /// "Connecting". So there is a button again. It is not the old Connect
    /// button: that one submitted a request and reported success without
    /// looking. This waits for the peer to actually become reachable and says
    /// so, and a request a person made is the one kind the core reports the
    /// failure of out loud.
    ///
    /// Returns whether a session opened.
    @discardableResult
    func retry(_ peer: FfiPeer) async -> Bool {
        await runtime?.submit(.connect(peer: peer.deviceId))
        // Generous: a dial walks every route it knows, and a handshake over
        // Bluetooth is not quick.
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
    ///
    /// How long to wait is not a number chosen here. The desktop watches its own
    /// screen locker for up to its confirm window before it answers at all, so a
    /// budget picked independently on this side can be shorter than the machine
    /// is allowed to take — and then a lock that worked is reported as a failure,
    /// intermittently, depending on how quick the locker is. It was eight seconds
    /// against a host allowed eight, leaving nothing at all for the answer to
    /// travel back over Bluetooth. The core hands out both numbers now.
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

    /// Send a transport command and wait for the peer to say what happened.
    ///
    /// Every one of these is answered with the state afterwards rather than an
    /// acknowledgement, because a player may ignore a command, clamp a seek, or
    /// stop of its own accord — and only reading it back says which.
    ///
    /// Whether the command landed is the core's rule, asked of it rather than
    /// decided here. Waiting for the whole state to differ was wrong in both
    /// directions: a playing track's position moves between any two readings, so
    /// every command looked like it had landed, while a paused one never changed
    /// at all and nothing ever looked like it had.
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
            // `mediaAt` moves on every reading, so this is "something arrived",
            // which a comparison of the readings themselves cannot tell us.
            if features.mediaAt != beforeAt, let now = features.media {
                guard let before else { return true }
                guard let landed = mediaCommandLanded(
                    verb: verb, player: player, value: value, before: before, now: now
                ) else {
                    // A reading cannot answer this one — a seek moves a position
                    // that also moves on its own — so the reading is the answer.
                    return true
                }
                // Not yet is not no. Keep waiting rather than answering: the
                // two-second poll can land between the command and its reply,
                // and it would be showing the state we started from.
                if landed { return true }
            }
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

    /// Ask a peer that has just come back where things stand.
    ///
    /// Losing a peer discards its session state — `PeerCatalog.ingest` clears it
    /// on `peerUnreachable`, because a lock screen remembered from ten minutes
    /// ago is worse than no answer. Nothing put it back. The device screen asks
    /// once, in `.task` when it appears, and a peer that goes away and returns
    /// while you are already looking at it never triggers that again — so the
    /// Session controls vanished and stayed vanished until the screen was left
    /// and re-entered.
    ///
    /// Watching a computer switch from Wi-Fi to Bluetooth is exactly when
    /// somebody is looking at that screen. Media hid the same bug: its state is
    /// broadcast whenever a track or position changes, so it refilled itself
    /// within seconds and only looked briefly stale, while the session state —
    /// which changes when someone locks their screen, and not otherwise — had
    /// nothing to refill it.
    private func reacquaint(with peerId: String) async {
        guard let peer = peers.first(where: { $0.deviceId == peerId }) else { return }
        await refreshSession(peer)
        await refreshMedia(peer)
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
    /// Returns whether an answer actually came back.
    ///
    /// It used to return nothing and the button reported `true` regardless, so
    /// asking a computer that was not listening drew a tick and then showed the
    /// value from the last time it worked — which is the worst of the three
    /// possible outcomes to be confident about.
    @discardableResult
    func fetchClipboard(_ peer: FfiPeer) async -> Bool {
        let before = catalog[peer.deviceId].clipboardAt
        await send(peer, cap: capClipboard(), ty: "get", body: Data())
        let deadline = ContinuousClock.now.advanced(
            by: .milliseconds(Int(mediaCommandBudgetMs())))
        while ContinuousClock.now < deadline {
            // The arrival time, not the value: fetching the same text twice is
            // a success both times, so what is being waited for is a *reply*
            // having landed and not the text having changed.
            if catalog[peer.deviceId].clipboardAt != before { return true }
            try? await Task.sleep(for: .milliseconds(120))
        }
        return false
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
        activity = "Sent to \(peer.name)"
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
        activity = "Offered \(offer.name)"
    }

    /// Say yes to a file. This is what makes the phone bind a port and wait.
    /// Transfers this phone has agreed to and not yet seen the end of.
    ///
    /// The row needs it: an accepted offer stays in the list while the bytes
    /// move, and without this it goes on offering the same two buttons as
    /// though nothing had been decided.
    var accepting: Set<UInt64> = []

    func accept(_ offer: IncomingOffer) async {
        // Left in the list until the transfer actually ends, so the row does
        // not vanish the instant it is tapped and leave nothing to look at
        // while the bytes move.
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
        case let .discovered(fingerprint, name, addr, _, pairing):
            // Keyed by fingerprint, because mDNS re-resolves the same machine
            // whenever anything about it changes — including the `pair` flag
            // going up — and a list that appended would show one computer
            // several times, each row claiming something different.
            let found = Nearby(
                fingerprint: fingerprint, name: name, addr: addr,
                pairing: pairing, seen: Date())
            if let at = nearby.firstIndex(where: { $0.fingerprint == fingerprint }) {
                nearby[at] = found
            } else {
                nearby.append(found)
            }
            // A machine that is waiting for somebody sorts first: it is the one
            // the person is most likely holding the phone for.
            nearby.sort { ($0.pairing ? 0 : 1, $0.name) < ($1.pairing ? 0 : 1, $1.name) }
        case let .pairingComplete(_, name):
            pairingSas = nil
            pairingCode = nil
            status = .ready
            activity = "Paired with \(name)"
            Task { await refresh() }
        case let .pairingFailed(reason):
            pairingSas = nil
            pairingCode = nil
            lastError = reason
        case let .peerReachable(peer, name):
            activity = "Connected to \(name)"
            Task {
                await refresh()
                await reacquaint(with: peer)
            }
        case .peerUnreachable:
            // Nothing said here on purpose. Which peer went is already in
            // `peers`, and why it went is `FfiPeer.trouble` — read by the row
            // that draws it, so a peer dropping does not overwrite a line about
            // some other peer.
            Task { await refresh() }
        // The peer is bound now: answering an offer means naming who made it,
        // and until this phone could receive a file there was nothing here that
        // needed to know.
        case let .plugin(peer, cap, ty, body):
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
                activity = "\(offer.name) offered"
            }
            // Both ends report a transfer, because each knows only its own
            // half: this phone knows it finished writing, and only the computer
            // knows whether it kept the file.
            if cap == capShare(), ty == "finished" || ty == "reject",
               let end = try? decodeShareFinished(body: body) {
                incoming.removeAll { $0.transfer == end.transfer }
                accepting.remove(end.transfer)
                let name = sending.removeValue(forKey: end.transfer) ?? "the file"
                if ty == "reject" {
                    activity = "\(name) was refused"
                } else if end.ok {
                    activity = "Sent \(name)"
                } else {
                    activity = "\(name) failed"
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
                activity = "Copied to this phone"
            }
        case let .error(peer, code, detail):
            // Local Network permission is the failure users actually hit, and
            // it is silent: iOS offers no API to query it. Say what to do.
            lastError = detail.contains("local network")
                ? "Allow local network access in Settings → Privacy → Local Network."
                : "\(code): \(detail)"
            // And put it on the device it is about, so the screen for that
            // computer can show it in place rather than only the banner over
            // everything. `nil` means the failure belongs to this phone.
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
