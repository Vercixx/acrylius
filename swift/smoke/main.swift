// A Linux smoke test of the FFI seam: exercise the same calls the iOS app will.
let key = try! generateIdentity()
print("identity bytes: \(key.count)")
print("fingerprint:    \(try! fingerprintOf(identityKey: key))")

let core = try! AcryliusCore(
    config: defaultConfig(name: "linux-smoke", platform: "linux"),
    identityKey: key,
    peers: [],
    effects: []
)
print("device id:      \(core.deviceId())")
print("service type:   \(serviceType())  port \(defaultPort())")
print("caps in/out:    \(core.capsIn()) / \(core.capsOut())")

// Open a pairing window and read back what the UI would show.
let out = try! core.handle(monotonicMs: 1000, wallMs: 1_700_000_000_000, event: .openPairingWindow(code: "ABCD1234"))
for a in out.actions {
    if case let .ui(event) = a, case let .pairingWindowOpen(code, ms) = event {
        print("ui:             window open, code \(code), \(ms/1000)s")
    }
}
print("deadline:       \(out.nextDeadlineMs.map(String.init) ?? "none")")
print("pending sas:    \(core.pendingSas() ?? "none")")

// A malformed device id must be refused at the boundary, not turned into a
// lookup that quietly matches nothing.
do {
    _ = try core.handle(monotonicMs: 1001, wallMs: 1_700_000_000_000, event: .connect(peer: "not-a-device-id"))
    print("FAIL: bad device id was accepted")
} catch {
    print("bad id refused: ok")
}
