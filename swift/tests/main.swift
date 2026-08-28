//
//  The Swift host runtime, tested on Linux.
//
//  Two CoreRuntimes joined by an in-memory transport pair, connect and ping:
//  exercising the exact code the iOS app runs, minus SwiftUI and
//  Network.framework. Without this it would all be unverifiable until an IPA
//  reached a phone.
//

import Foundation

// MARK: - a transport that is two objects in one process

final class Loopback: Transport, @unchecked Sendable {
    let transportId: UInt16 = 1
    let name: String
    weak var peer: Loopback?

    private let lock = NSLock()
    private var emit: (@Sendable (FfiEvent) -> Void)?
    private var links: [UInt64: UInt64] = [:]

    private static let counter = NSLock()
    // Guarded by `counter` on every access, which the compiler cannot see.
    nonisolated(unsafe) private static var next: UInt64 = 0
    nonisolated static func freshPair() -> (UInt64, UInt64) {
        counter.lock(); defer { counter.unlock() }
        next += 2
        return (next, next + 1)
    }

    init(name: String) { self.name = name }

    // Locking is confined to these synchronous helpers. Taking a lock directly
    // in an async function is an error under the Swift 6 language mode, because
    // a suspension while holding one can deadlock.
    private func setEmit(_ f: @escaping @Sendable (FfiEvent) -> Void) {
        lock.lock(); emit = f; lock.unlock()
    }

    private func fire(_ e: FfiEvent) {
        lock.lock(); let f = emit; lock.unlock()
        f?(e)
    }

    private func mapLink(_ mine: UInt64, to theirs: UInt64) {
        lock.lock(); links[mine] = theirs; lock.unlock()
    }

    private func lookup(_ mine: UInt64) -> UInt64? {
        lock.lock(); defer { lock.unlock() }
        return links[mine]
    }

    private func drop(_ mine: UInt64) -> UInt64? {
        lock.lock(); defer { lock.unlock() }
        return links.removeValue(forKey: mine)
    }

    func start(events: @escaping @Sendable (FfiEvent) -> Void) async {
        setEmit(events)
    }

    func dial(addr: String, token: UInt64) async {
        guard let peer else { fire(.dialFailed(dial: token, reason: "nobody there")); return }
        let (mine, theirs) = Loopback.freshPair()
        mapLink(mine, to: theirs)
        peer.mapLink(theirs, to: mine)

        let attrs = tcpLanAttrs(transport: transportId)
        fire(.linkUp(link: mine, attrs: attrs, dial: token))
        peer.fire(.linkUp(link: theirs, attrs: attrs, dial: nil))
    }

    func send(link: UInt64, msg: Data) async {
        guard let target = lookup(link), let peer else { return }
        peer.fire(.linkRecv(link: target, msg: msg))
    }

    func close(link: UInt64) async {
        guard let target = drop(link), let peer else { return }
        _ = peer.drop(target)
        peer.fire(.linkDown(link: target, reason: .closed))
    }

    func advertise(enable: Bool, txt: [FfiTxt]) async {}
    func discover(enable: Bool) async {}
}

// MARK: - collecting what a UI would see

final class Recorder: UiSink, @unchecked Sendable {
    private let lock = NSLock()
    private(set) var events: [FfiUiEvent] = []
    nonisolated func emit(_ event: FfiUiEvent) {
        lock.lock(); events.append(event); lock.unlock()
    }
    func snapshot() -> [FfiUiEvent] {
        lock.lock(); defer { lock.unlock() }
        return events
    }
    func sas() -> String? {
        snapshot().compactMap { if case let .pairingSas(_, _, s) = $0 { return s } else { return nil } }.last
    }
    func has(_ p: (FfiUiEvent) -> Bool) -> Bool { snapshot().contains(where: p) }
}

// MARK: - harness

var failures = 0

// Top-level code is main-actor isolated, so the helpers that touch `failures`
// must be too; a global function would be nonisolated and could not.
@MainActor
func check(_ ok: Bool, _ what: String) {
    if ok { print("  ok   \(what)") } else { print("  FAIL \(what)"); failures += 1 }
}

/// Poll until `cond` holds. The runtimes are asynchronous, so a test must wait
/// for quiescence rather than assume it.
func until(_ what: String, timeoutMs: Int = 3000, _ cond: @escaping () -> Bool) async -> Bool {
    var waited = 0
    while waited < timeoutMs {
        if cond() { return true }
        try? await Task.sleep(for: .milliseconds(20))
        waited += 20
    }
    print("  (timed out waiting for \(what))")
    return false
}

func makeRuntime(_ name: String) throws -> (CoreRuntime, Recorder, Loopback, Store) {
    let store = MemoryStore()
    let rt = try CoreRuntime.bootstrap(name: name, platform: "linux", store: store)
    let rec = Recorder()
    let lb = Loopback(name: name)
    return (rt, rec, lb, store)
}

print("Swift runtime tests (Linux)")

let (alpha, aRec, aNet, aStore) = try makeRuntime("alpha")
let (bravo, bRec, bNet, bStore) = try makeRuntime("bravo")
aNet.peer = bNet
bNet.peer = aNet

await alpha.setUi(aRec)
await bravo.setUi(bRec)
await alpha.add(transport: aNet)
await bravo.add(transport: bNet)
await alpha.start()
await bravo.start()

let aId = await alpha.deviceId()
let bId = await bravo.deviceId()
check(aId != bId, "two runtimes have distinct identities")

// --- the capabilities a phone offers -------------------------------------
// The FFI was left registering ping alone from the skeleton, so a phone
// advertised nothing and a computer would not send it a clipboard. The failure
// read as a missing clipboard implementation and was a missing registration.
let offered = Set(await alpha.capsIn())
for cap in [
    "org.acrylius.ping/1",
    "org.acrylius.session/1",
    "org.acrylius.clipboard/1",
    "org.acrylius.command/1",
    "org.acrylius.wol/1",
] {
    check(offered.contains(cap), "a phone advertises \(cap)")
}

// --- pairing -------------------------------------------------------------
await bravo.submit(.openPairingWindow(code: "ABCD1234"))
_ = await until("bravo's window") { bRec.has { if case .pairingWindowOpen = $0 { return true }; return false } }

await alpha.submit(.requestPairing(transport: 1, addr: "bravo", code: "ABCD1234"))
let sawSas = await until("a code on both screens") { aRec.sas() != nil && bRec.sas() != nil }
check(sawSas, "both ends showed a code")
check(sawSas && aRec.sas() == bRec.sas(), "the codes match: \(aRec.sas() ?? "-")")

await alpha.submit(.confirmPairing(accept: true))
await bravo.submit(.confirmPairing(accept: true))
let paired = await until("pairing to complete") {
    aRec.has { if case .pairingComplete = $0 { return true }; return false }
        && bRec.has { if case .pairingComplete = $0 { return true }; return false }
}
check(paired, "both ends completed pairing")

let aPeers = await alpha.peers()
check(aPeers.count == 1 && aPeers.first?.deviceId == bId, "alpha stored bravo")
check(aStore.loadPeers().count == 1, "alpha persisted one peer record")
check(bStore.loadPeers().count == 1, "bravo persisted one peer record")

// --- session and ping ----------------------------------------------------
await alpha.submit(.setPeerAddress(peer: bId, transport: 1, addr: "bravo"))
await alpha.submit(.connect(peer: bId))
let reachable = await until("a session") {
    aRec.has { if case .peerReachable = $0 { return true }; return false }
}
check(reachable, "alpha reached bravo")

await alpha.submit(.pluginCommand(peer: bId, cap: "org.acrylius.ping/1", ty: "ping",
                                  body: Data("hello".utf8)))
let ponged = await until("a pong") {
    aRec.has { if case let .plugin(_, _, ty, body) = $0 { return ty == "pong" && body == Data("hello".utf8) }
               return false }
}
check(ponged, "ping round-tripped through the Swift runtime")

// --- a stranger ----------------------------------------------------------
let (mallory, mRec, mNet, _) = try makeRuntime("mallory")
mNet.peer = bNet
await mallory.setUi(mRec)
await mallory.add(transport: mNet)
await mallory.start()
let mId = await mallory.deviceId()
await mallory.submit(.setPeerAddress(peer: bId, transport: 1, addr: "bravo"))
await mallory.submit(.connect(peer: bId))
try? await Task.sleep(for: .milliseconds(400))
check(!mRec.has { if case .peerReachable = $0 { return true }; return false },
      "an unpaired stranger got no session")
let bPeersAfter = await bravo.peers()
check(!bPeersAfter.contains { $0.deviceId == mId }, "bravo did not learn the stranger")

// --- the peer catalogue ------------------------------------------------
// What a screen shows is driven by what a peer announced, not by the
// handshake. A capability that may be exchanged is not the same as a
// feature the peer actually has.
var catalog = PeerCatalog()
check(!catalog["someone"].canLock, "an unknown peer offers nothing")
check(!catalog["someone"].canRunCommands, "and no commands")

let state = FfiSessionState(locked: true, sessionId: "2", kind: "wayland", active: true)
// Bodies are built through the FFI. Swift does not know the wire format and
// must not learn it.
_ = catalog.ingest(.plugin(peer: "p", cap: capSession(), ty: "state",
                           body: encodeSessionState(state: state)))
check(catalog["p"].canLock, "a peer that described a session can be locked")
check(catalog["p"].session?.locked == true, "and it is reported as locked")

let commands = [FfiCommand(id: "screenshot", name: "Screenshot", needsConfirm: false)]
_ = catalog.ingest(.plugin(peer: "p", cap: capCommand(), ty: "list",
                           body: encodeCommandList(commands: commands)))
check(catalog["p"].canRunCommands, "a peer that published a catalogue can run things")
check(catalog["p"].commands.first?.id == "screenshot", "and the ids come through")

_ = catalog.ingest(.plugin(peer: "p", cap: capClipboard(), ty: "set",
                           body: encodeClipboard(text: "hello from the pc")))
check(catalog["p"].clipboard == "hello from the pc", "a clipboard value is kept")

check(!catalog["p"].canWake, "a peer that never offered wake targets cannot be woken")

// --- where the track has got to -----------------------------------------
// A computer announces a track change but not a position; broadcasting one
// every second so a clock can tick would be absurd. So the phone advances it
// against the moment the reading was taken, and only while it is playing.
func withMedia(_ status: String, positionMs: UInt64, lengthMs: UInt64) -> PeerCatalog {
    var c = PeerCatalog()
    let state = FfiMediaState(
        players: [FfiMediaPlayer(
            id: "p1", name: "Player", status: status, title: "A Song", artist: "", album: "",
            lengthMs: lengthMs, positionMs: positionMs, volumePercent: nil,
            canGoNext: true, canGoPrevious: true, canSeek: true, canControl: true)],
        active: "p1", systemVolume: 40)
    _ = c.ingest(.plugin(peer: "p", cap: capMedia(), ty: "state",
                         body: encodeMediaState(state: state)))
    return c
}

let playing = withMedia("playing", positionMs: 10_000, lengthMs: 200_000)
let then = playing["p"].mediaAt ?? Date()
check(playing["p"].positionMs(at: then) == 10_000, "at the instant it arrived, it is what was reported")
check(playing["p"].positionMs(at: then.addingTimeInterval(5)) == 15_000,
      "five seconds later, five seconds further in")

let paused = withMedia("paused", positionMs: 10_000, lengthMs: 200_000)
let pausedAt = paused["p"].mediaAt ?? Date()
check(paused["p"].positionMs(at: pausedAt.addingTimeInterval(60)) == 10_000,
      "a paused track does not move on its own")

// A track that ended while nobody was asking must not report a time longer
// than itself.
let ended = withMedia("playing", positionMs: 195_000, lengthMs: 200_000)
let endedAt = ended["p"].mediaAt ?? Date()
check(ended["p"].positionMs(at: endedAt.addingTimeInterval(9)) == 200_000,
      "and never past the end of it")

// `position` is reported, never counted — the core says so, and gives the
// reason: a receiver that keeps counting goes on counting after the media
// stopped somewhere it cannot see. Readings arrive every couple of seconds
// while anyone is looking, so one this old means they have stopped coming.
// Freezing where it was last actually seen is the honest answer; extrapolating
// ten minutes on is inventing a position nobody reported.
let abandoned = withMedia("playing", positionMs: 190_000, lengthMs: 200_000)
let abandonedAt = abandoned["p"].mediaAt ?? Date()
check(abandoned["p"].positionMs(at: abandonedAt.addingTimeInterval(600)) == 190_000,
      "a reading nobody refreshed stops being counted forward")

// The same thing said outright: the peer is gone, so the clock under the track
// stops. This is the case that ran forever — a stream reports no length, so the
// clamp above would never have caught it either.
var wentAway = withMedia("playing", positionMs: 10_000, lengthMs: 0)
let wentAwayAt = wentAway["p"].mediaAt ?? Date()
check(wentAway["p"].positionMs(at: wentAwayAt.addingTimeInterval(5)) == 15_000,
      "still counting while the peer is there")
_ = wentAway.ingest(.peerUnreachable(peer: "p"))
check(wentAway["p"].media != nil, "the track stays on screen")
check(wentAway["p"].positionMs(at: wentAwayAt.addingTimeInterval(600)) == 10_000,
      "but an unreachable peer's timeline stops where it was last seen")

check(playing["p"].media?.systemVolume == 40, "the machine's own volume comes through")

// --- the widget's snapshot ---------------------------------------------
// The widget renders this and nothing else. It runs in a process that can
// open no session, so anything wrong here is a widget that is confidently
// wrong with no way to notice.
SnapshotStore.save(peers: [
    PeerSnapshot(deviceId: "p", name: "desktop", platform: "linux",
                 lastSeen: Date(timeIntervalSince1970: 1000), locked: false,
                 canWake: true, nowPlaying: "Someone — A Song"),
])
let first = SnapshotStore.load()
check(first?.peers.count == 1, "a snapshot round-trips")
check(first?.peers.first?.nowPlaying == "Someone — A Song", "and what was playing")
check(first?.shared == SharedContainer.isShared, "and says whether it is shared at all")

// A peer that is not reachable right now keeps the time it last was. The
// running app is the only thing that ever knows, so losing it on the next
// write would mean a widget that can only ever say "open the app".
SnapshotStore.save(peers: [
    PeerSnapshot(deviceId: "p", name: "desktop", platform: "linux",
                 lastSeen: nil, locked: true, canWake: true),
])
let second = SnapshotStore.load()
check(second?.peers.first?.lastSeen == Date(timeIntervalSince1970: 1000),
      "an unreachable peer keeps when it was last seen")
check(second?.peers.first?.locked == true, "while everything else is replaced")

// A peer that came back sets a new time rather than keeping the old one.
SnapshotStore.save(peers: [
    PeerSnapshot(deviceId: "p", name: "desktop", platform: "linux",
                 lastSeen: Date(timeIntervalSince1970: 2000), canWake: true),
])
check(SnapshotStore.load()?.peers.first?.lastSeen == Date(timeIntervalSince1970: 2000),
      "and a peer seen again moves it forward")

// --------------------------------------------------------------- diagnostics

// The trouble channel: a Bluetooth failure a person can act on has to survive
// to somewhere they will read it, and clear itself once acted on. The mapping
// from a CBError lives in BLETransport, which no compiler here can see; this
// is the half that can be checked.
let diag = await BLEDiagnostics()
await diag.apply(.trouble("forget the device in Settings"))
check(await diag.trouble == "forget the device in Settings",
      "a problem worth acting on is kept, not only logged")
check(await diag.transcript().contains("forget the device"),
      "and it is in what gets copied out")
await diag.apply(.trouble(nil))
check(await diag.trouble == nil,
      "and it clears, so an instruction does not outlive being carried out")
check(await diag.notes.count == 1,
      "clearing leaves the record of what happened rather than a second entry")

await alpha.stop(); await bravo.stop(); await mallory.stop()

print(failures == 0 ? "\nall passed" : "\n\(failures) FAILED")
exit(failures == 0 ? 0 : 1)
