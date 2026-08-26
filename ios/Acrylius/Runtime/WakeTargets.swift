//
//  What a computer told us about waking it.
//
//  Kept on disk because of when it is needed: by the time somebody wants to wake
//  a machine, that machine is asleep and cannot be asked anything. The daemon
//  sends this the moment a session opens, precisely so it is already here.
//
//  It lives in the shared container, not in the Keychain. A MAC address and a
//  broadcast address are not secrets; they have to be readable by an App Intent
//  in a process that may be running while the phone is locked, and by a widget,
//  which has no Keychain access of its own and needs none for this. Waking is
//  an unauthenticated datagram — a saved target is the only thing it takes.
//

import Foundation

public enum WakeTargets {
    private static func url(for peer: String) -> URL? {
        guard let dir = SharedContainer.directory("wake") else { return nil }
        // A device id is strict base64url: no separators, so it cannot escape
        // the directory. Checked anyway.
        guard !peer.contains("/"), !peer.contains(".") else { return nil }
        return dir.appendingPathComponent(peer)
    }

    /// Every peer with a target on file.
    ///
    /// A saved target is also the record that this peer was paired: the daemon
    /// only sends one over an open session. Nothing else needs asking before a
    /// wake-up goes out.
    public static func known() -> Set<String> {
        guard let dir = SharedContainer.directory("wake"),
              let names = try? FileManager.default.contentsOfDirectory(
                  at: dir, includingPropertiesForKeys: nil)
        else { return [] }
        return Set(names.map { $0.lastPathComponent })
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
