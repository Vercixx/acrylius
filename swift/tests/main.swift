//
//  The Swift host runtime, tested on Linux.
//
//  Two CoreRuntimes joined by an in-memory transport pair: the same code the
//  iOS app runs, minus SwiftUI and Network.framework.
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
    // Guarded by `counter`; the compiler can't see that.
    nonisolated(unsafe) private static var next: UInt64 = 0
    nonisolated static func freshPair() -> (UInt64, UInt64) {
        counter.lock(); defer { counter.unlock() }
        next += 2
        return (next, next + 1)
    }

    init(name: String) { self.name = name }

    // Locking confined to sync helpers; holding a lock across a suspension
    // point is an error under the Swift 6 language mode.
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

// Top-level code is main-actor isolated, so helpers touching `failures` must be too.
@MainActor
func check(_ ok: Bool, _ what: String) {
    if ok { print("  ok   \(what)") } else { print("  FAIL \(what)"); failures += 1 }
}

/// Poll until `cond` holds; the runtimes are asynchronous.
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
// Alpha asks, six digits appear on both ends; the codes matching is the
// whole authentication, and the assertion that matters most here.
await alpha.submit(.requestPairing(transport: 1, addr: "bravo"))
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
// Driven by what a peer announced, not the handshake — an exchangeable
// capability isn't the same as a feature the peer actually has.
var catalog = PeerCatalog()
check(!catalog["someone"].canLock, "an unknown peer offers nothing")
check(!catalog["someone"].canRunCommands, "and no commands")

let state = FfiSessionState(locked: true, sessionId: "2", kind: "wayland", active: true)
// Bodies built through the FFI; Swift must not learn the wire format.
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

// A timestamp, not the value, marks arrival: asking twice for the same text
// is a success both times, so the value alone can't signal a fresh answer.
let firstArrival = catalog["p"].clipboardAt
check(firstArrival != nil, "an arriving clipboard value is timestamped")
_ = catalog.ingest(.plugin(peer: "p", cap: capClipboard(), ty: "set",
                           body: encodeClipboard(text: "hello from the pc")))
check(
    catalog["p"].clipboardAt != firstArrival,
    "the same text arriving again is still an answer, and must not look like silence")

check(!catalog["p"].canWake, "a peer that never offered wake targets cannot be woken")

// --- where the track has got to -----------------------------------------
// Position isn't broadcast every second; the phone extrapolates it from the
// last reading's timestamp, only while playing.
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

// A reading this old (readings normally arrive every couple of seconds) means
// they've stopped coming; freezing at the last known position beats extrapolating.
let abandoned = withMedia("playing", positionMs: 190_000, lengthMs: 200_000)
let abandonedAt = abandoned["p"].mediaAt ?? Date()
check(abandoned["p"].positionMs(at: abandonedAt.addingTimeInterval(600)) == 190_000,
      "a reading nobody refreshed stops being counted forward")

// Same idea via peer disconnect. Zero length (e.g. a stream) means the
// end-of-track clamp above can't catch this case either.
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
// The widget renders only this; its process can't open a session to double-check it.
SnapshotStore.save(peers: [
    PeerSnapshot(deviceId: "p", name: "desktop", platform: "linux",
                 lastSeen: Date(timeIntervalSince1970: 1000), locked: false,
                 canWake: true, nowPlaying: "Someone — A Song"),
])
let first = SnapshotStore.load()
check(first?.peers.count == 1, "a snapshot round-trips")
check(first?.peers.first?.nowPlaying == "Someone — A Song", "and what was playing")
check(first?.shared == SharedContainer.isShared, "and says whether it is shared at all")

// An unreachable peer keeps the time it was last seen rather than losing it on the next write.
SnapshotStore.save(peers: [
    PeerSnapshot(deviceId: "p", name: "desktop", platform: "linux",
                 lastSeen: nil, locked: true, canWake: true),
])
let second = SnapshotStore.load()
check(second?.peers.first?.lastSeen == Date(timeIntervalSince1970: 1000),
      "an unreachable peer keeps when it was last seen")
check(second?.peers.first?.locked == true, "while everything else is replaced")

// A peer seen again moves the time forward.
SnapshotStore.save(peers: [
    PeerSnapshot(deviceId: "p", name: "desktop", platform: "linux",
                 lastSeen: Date(timeIntervalSince1970: 2000), canWake: true),
])
check(SnapshotStore.load()?.peers.first?.lastSeen == Date(timeIntervalSince1970: 2000),
      "and a peer seen again moves it forward")

// --------------------------------------------------------------- diagnostics

// Trouble channel: an actionable Bluetooth failure persists until read, then clears.
// The CBError mapping lives in BLETransport, which this compiler cannot see.
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

// --- when was that reading actually taken ----------------------------------
// A reading is already one leg of the round trip old by the time it lands;
// stamping arrival time alone runs the clock that far behind (worse over BLE).
let sent = Date(timeIntervalSince1970: 1_000)
let arrived = sent.addingTimeInterval(0.8)
check(PeerCatalog.measuredAt(sent: sent, arrived: arrived) == sent.addingTimeInterval(0.4),
      "a reading is placed halfway back down its round trip")
check(PeerCatalog.measuredAt(sent: nil, arrived: arrived) == arrived,
      "with nothing to measure against, arrival is the best guess there is")
check(PeerCatalog.measuredAt(sent: arrived, arrived: sent) == sent,
      "a reply that predates its query is not a round trip")
check(PeerCatalog.measuredAt(sent: sent, arrived: sent.addingTimeInterval(60)) ==
        sent.addingTimeInterval(60),
      "and neither is one a minute late, which halved would run the clock fast")

// End to end: the estimate must not sit behind where the track really is.
var timed = PeerCatalog()
timed.noteMediaQuery(for: "pc", at: sent)
check(timed["pc"].mediaQuerySentAt == sent, "the query's departure is noted")

// --- which build is this ---------------------------------------------------
// Xcode leaves an unset build setting as an empty string, not an absent key,
// so that's the case worth pinning — a misreported commit is worse than none.
let stamped = BuildInfo.from([
    "ACRBuildCommit": "86f96f3a1b2c3d4e5f60718293a4b5c6d7e8f900",
    "ACRBuildDate": "2026-08-29T18:20:49Z",
    "CFBundleShortVersionString": "0.1.0",
])
check(stamped.commit == "86f96f3a1b2c", "a commit is abbreviated, not shown whole")
check(stamped.version == "0.1.0", "the version comes through")
check(stamped.builtAt != nil, "an ISO 8601 instant parses")
check(stamped.summary.hasPrefix("86f96f3a1b2c ·"), "and the summary leads with it")

let unstamped = BuildInfo.from(["ACRBuildCommit": "", "ACRBuildDate": ""])
check(unstamped.commit == nil, "an unexpanded build setting is not a commit")
check(unstamped.summary == "Development build",
      "and it says so rather than showing an empty row")
check(BuildInfo.from(nil).commit == nil, "no Info.plist at all is the same answer")

check(BuildInfo.from(["ACRBuildCommit": "abc123", "ACRBuildDate": "not a date"]).builtAt == nil,
      "a date that will not parse loses the date, not the commit")
check(BuildInfo.from(["ACRBuildCommit": "abc123", "ACRBuildDate": "not a date"]).summary == "abc123",
      "and the commit is still worth showing on its own")
check(BuildInfo.from(["ACRBuildDate": "2026-08-29T18:20:49.123Z"]).builtAt != nil,
      "fractional seconds parse too, since `date` can emit them")

await alpha.stop(); await bravo.stop(); await mallory.stop()

print(failures == 0 ? "\nall passed" : "\n\(failures) FAILED")
exit(failures == 0 ? 0 : 1)
