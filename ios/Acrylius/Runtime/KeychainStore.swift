//
//  Persistence on Apple platforms. Identity key: Keychain,
//  `WhenUnlockedThisDeviceOnly`, no biometric ACL — that belongs on the
//  action, not the key, or reads fail while locked. Peer records hold a
//  session PSK and are plain files in the app container instead. A reinstall
//  wipes both, by design: re-pairing is a QR scan.
//

#if canImport(Security)

import Foundation
import Security

public final class KeychainStore: Store, @unchecked Sendable {
    private let service: String
    private let account = "identity"
    private let peersDir: URL

    public init(service: String = "org.acrylius.identity") throws {
        self.service = service
        let base = try FileManager.default.url(
            for: .applicationSupportDirectory, in: .userDomainMask,
            appropriateFor: nil, create: true
        )
        peersDir = base.appendingPathComponent("peers", isDirectory: true)
        try FileManager.default.createDirectory(at: peersDir, withIntermediateDirectories: true)
    }

    // MARK: - identity

    /// The stored identity, or `nil` if this device has never had one.
    ///
    /// Every other failure throws: on a locked phone this item throws
    /// `errSecInteractionNotAllowed`, and App Intents/widgets can run from the
    /// lock screen — folding that into `nil` would look like a first run.
    public func identityKey() throws -> Data? {
        let q: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var out: CFTypeRef?
        let status = SecItemCopyMatching(q as CFDictionary, &out)
        if status == errSecItemNotFound { return nil }
        guard status == errSecSuccess else { throw StoreError.keychain(status) }
        return out as? Data
    }

    public func setIdentityKey(_ key: Data) throws {
        let q: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
        SecItemDelete(q as CFDictionary)
        var add = q
        add[kSecValueData as String] = key
        // Not `.biometryCurrentSet`; see the note at the top of this file.
        add[kSecAttrAccessible as String] = kSecAttrAccessibleWhenUnlockedThisDeviceOnly
        let status = SecItemAdd(add as CFDictionary, nil)
        guard status == errSecSuccess else {
            throw StoreError.keychain(status)
        }
    }

    // MARK: - peers

    /// Keys are `peer/<device-id>`. Device ids are strict base64url so this
    /// can't escape the directory, but check anyway rather than rely on that.
    private func url(for key: String) throws -> URL {
        let parts = key.split(separator: "/")
        guard parts.count == 2, parts[0] == "peer", !parts[1].contains(".") else {
            throw StoreError.badKey(key)
        }
        return peersDir.appendingPathComponent(String(parts[1]))
    }

    public func put(key: String, value: Data?, sensitivity: FfiSensitivity) throws {
        let url = try url(for: key)
        guard let value else {
            try? FileManager.default.removeItem(at: url)
            return
        }
        // `.atomic` is a write-then-rename, so a crash mid-write leaves the
        // previous record rather than half a key.
        try value.write(to: url, options: [.atomic, .completeFileProtectionUntilFirstUserAuthentication])
    }

    public func loadPeers() -> [Data] {
        let names = (try? FileManager.default.contentsOfDirectory(
            at: peersDir, includingPropertiesForKeys: nil)) ?? []
        return names.compactMap { try? Data(contentsOf: $0) }
    }
}

public enum StoreError: Error {
    case keychain(OSStatus)
    case badKey(String)
}

#endif
