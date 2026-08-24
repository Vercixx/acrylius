//
//  The Swift host runtime, tested on Linux.
//
//  Two CoreRuntimes joined by an in-memory transport pair, connect and ping —
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
    private static var next: UInt64 = 0
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

await alpha.stop(); await bravo.stop(); await mallory.stop()

print(failures == 0 ? "\nall passed" : "\n\(failures) FAILED")
exit(failures == 0 ? 0 : 1)
