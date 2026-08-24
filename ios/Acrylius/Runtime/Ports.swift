//
//  The host-side seams, all platform-free.
//
//  These are Swift protocols the *host* implements. Nothing here crosses the
//  FFI: the Rust core never calls Swift. It hands back actions, and CoreRuntime
//  routes them to one of these. That is what keeps the boundary one-directional
//  and reentrancy structurally impossible rather than merely discouraged.
//

import Foundation

/// Moves bytes between two devices. On iOS this is Network.framework; on Linux
/// it is whatever a test needs.
///
/// A transport must never call into the core. It reports what happened by
/// yielding events, and the runtime's single consumer picks them up.
public protocol Transport: AnyObject, Sendable {
    var transportId: UInt16 { get }

    func start(events: @escaping @Sendable (FfiEvent) -> Void) async
    func dial(addr: String, token: UInt64) async
    func send(link: UInt64, msg: Data) async
    func close(link: UInt64) async
    func advertise(enable: Bool, txt: [FfiTxt]) async
    func discover(enable: Bool) async
}

/// The platform half of a plugin.
public protocol Effector: AnyObject, Sendable {
    /// Effects this host can actually carry out. The core drops plugins whose
    /// requirements are unmet and never advertises their capabilities, so this
    /// is what decides the device's feature set.
    func run(_ effect: FfiEffect) async -> FfiEffectResult
}

/// Persistence. `Secret` values must go to the Keychain, never a plain file.
public protocol Store: AnyObject, Sendable {
    func put(key: String, value: Data?, sensitivity: FfiSensitivity) throws
    func loadPeers() -> [Data]
    func identityKey() -> Data?
    func setIdentityKey(_ key: Data) throws
}

/// Somewhere for a UI to watch what the core is saying.
public protocol UiSink: AnyObject, Sendable {
    func emit(_ event: FfiUiEvent)
}

/// A host that can do nothing, so a capability simply never negotiates.
public final class NullEffector: Effector {
    public init() {}
    public func run(_ effect: FfiEffect) async -> FfiEffectResult { .unsupported }
}

/// Keeps nothing. For tests and previews.
public final class MemoryStore: Store, @unchecked Sendable {
    private let lock = NSLock()
    private var entries: [String: Data] = [:]
    private var identity: Data?

    public init() {}

    // Synchronous by protocol, so the lock is never held across a suspension.
    public func put(key: String, value: Data?, sensitivity: FfiSensitivity) throws {
        lock.lock(); defer { lock.unlock() }
        if let value { entries[key] = value } else { entries.removeValue(forKey: key) }
    }

    public func loadPeers() -> [Data] {
        lock.lock(); defer { lock.unlock() }
        return entries.filter { $0.key.hasPrefix("peer/") }.map(\.value)
    }

    public func identityKey() -> Data? {
        lock.lock(); defer { lock.unlock() }
        return identity
    }

    public func setIdentityKey(_ key: Data) throws {
        lock.lock(); defer { lock.unlock() }
        identity = key
    }
}
