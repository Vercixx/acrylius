//
//  What a computer told us about waking it.
//
//  Kept on disk because of when it is needed: by the time somebody wants to wake
//  a machine, that machine is asleep and cannot be asked anything. The daemon
//  sends this the moment a session opens, precisely so it is already here.
//
//  It lives beside the peer records in the app container, not in the Keychain.
//  A MAC address and a broadcast address are not secrets, and an App Intent has
//  to read them in a process that may be running while the phone is locked.
//

import Foundation

public enum WakeTargets {
    private static func url(for peer: String) -> URL? {
        guard let base = try? FileManager.default.url(
            for: .applicationSupportDirectory, in: .userDomainMask,
            appropriateFor: nil, create: true
        ) else { return nil }
        let dir = base.appendingPathComponent("wake", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        // A device id is strict base64url: no separators, so it cannot escape
        // the directory. Checked anyway.
        guard !peer.contains("/"), !peer.contains(".") else { return nil }
        return dir.appendingPathComponent(peer)
    }

    public static func save(_ config: FfiWolConfig, for peer: String) {
        guard let url = url(for: peer) else { return }
        // Encoded through the FFI, so there is one definition of the shape.
        let body = encodeWolConfig(config: config)
        try? body.write(to: url, options: .atomic)
    }

    public static func load(for peer: String) -> FfiWolConfig? {
        guard let url = url(for: peer), let body = try? Data(contentsOf: url) else { return nil }
        return try? decodeWolConfig(body: body)
    }
}
