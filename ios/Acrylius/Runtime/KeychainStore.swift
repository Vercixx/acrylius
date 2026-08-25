//
//  Persistence on Apple platforms.
//
//  Two rules, both learned the hard way in the previous project:
//
//  1. The identity key goes in the Keychain as `WhenUnlockedThisDeviceOnly`,
//     with no biometric ACL. An item behind `.biometryCurrentSet` cannot be
//     read while the phone is locked, which breaks every short-lived extension
//     and every background refresh. Biometrics belong on the action, as an
//     `LAContext` check before sending an unlock, not on the key.
//  2. Peer records are ordinary files in the app container, not Keychain items.
//     They contain a session PSK, so the container is `.completeUntilFirstUserAuthentication`
//     and the files are excluded from backup; but the Keychain is for the one
//     secret that must survive nothing else, and filling it with blobs makes
//     the 7-day reinstall cycle worse rather than better.
//
//  A reinstall wipes the Keychain, and with it the identity. That is not a bug
//  to route around: it means re-pairing, which is a ten-second QR scan by
//  design.
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

    public func identityKey() -> Data? {
        let q: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var out: CFTypeRef?
        guard SecItemCopyMatching(q as CFDictionary, &out) == errSecSuccess else { return nil }
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

    /// Keys are `peer/<device-id>`. A device id is strict base64url, so it holds
    /// no `/` and no `.` and cannot climb out of the directory, but check
    /// anyway rather than depend on that.
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
