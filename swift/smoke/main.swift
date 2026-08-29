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

// Ask to pair, and read back what the host would be told to do about it.
let out = try! core.handle(
    monotonicMs: 1000, wallMs: 1_700_000_000_000,
    event: .requestPairing(transport: 1, addr: "127.0.0.1:1971"))
for a in out.actions {
    if case let .dial(transport, addr, _) = a {
        print("ui:             dialling \(addr) on transport \(transport)")
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
