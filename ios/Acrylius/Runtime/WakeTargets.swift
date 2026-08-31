//
//  What a computer told us about waking it, kept on disk since by the time
//  it's needed the machine is asleep and can't be asked. Lives in the shared
//  container, not the Keychain: a MAC/broadcast address isn't a secret, and
//  an App Intent or widget needs to read it while the phone is locked.
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

    /// Every peer with a target on file — also the record that it was paired,
    /// since the daemon only sends one over an open session.
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
