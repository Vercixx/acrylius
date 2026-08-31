//
//  The host-side seams the *host* implements, all platform-free. Nothing here
//  crosses the FFI: the core hands back actions and CoreRuntime routes them
//  to one of these, keeping the boundary one-directional.
//

import Foundation

/// Moves bytes between two devices. Must never call into the core — it
/// reports by yielding events, which the runtime's single consumer picks up.
public protocol Transport: AnyObject, Sendable {
    var transportId: UInt16 { get }

    func start(events: @escaping @Sendable (FfiEvent) -> Void) async
    func dial(addr: String, token: UInt64) async
    func send(link: UInt64, msg: Data) async
    func close(link: UInt64) async
    func advertise(enable: Bool, txt: [FfiTxt]) async
    func discover(enable: Bool) async

    /// Check the links this transport is holding, and report any that have
    /// died silently. iOS suspends a backgrounded app, so sockets can die with
    /// no handler running to notice; called on the way back to foreground.
    func revalidate() async

    /// Start discovery over, whatever state it was in. A browse can fail
    /// outright and stay failed, silently blocking the Bluetooth-to-Wi-Fi upgrade.
    func rediscover() async
}

public extension Transport {
    func revalidate() async {}
    func rediscover() async {}
}

/// The platform half of a plugin.
public protocol Effector: AnyObject, Sendable {
    /// Effects this host can actually carry out; decides the device's feature
    /// set, since the core drops plugins whose requirements go unmet.
    func run(_ effect: FfiEffect) async -> FfiEffectResult
}

/// Persistence. `Secret` values must go to the Keychain, never a plain file.
public protocol Store: AnyObject, Sendable {
    func put(key: String, value: Data?, sensitivity: FfiSensitivity) throws
    func loadPeers() -> [Data]
    /// The stored identity, or `nil` only when this device has never had one.
    /// Must throw rather than return `nil` if it can't answer right now — a
    /// caller reads `nil` as "first run" and overwrites the real identity.
    func identityKey() throws -> Data?
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
