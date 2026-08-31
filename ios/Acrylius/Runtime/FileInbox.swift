//
//  Files offered to this phone, and where they will land. Mirrors `FileOutbox`;
//  holds the name/size/id between an offer arriving and `BulkListen` accepting
//  it, since the core tracks neither together. Lands in Documents
//  (`UIFileSharingEnabled` exposes it to the Files app). Name safety is
//  `bulkSafeName`, shared with the daemon's `safe_name`.
//

import Foundation

public actor FileInbox {
    /// An offer that has arrived and not yet finished.
    public struct Incoming: Sendable {
        public let peer: String
        public let name: String
        public let size: UInt64
        /// Where it will be written; decided at offer time so a name collision
        /// resolves before the bytes start moving, not after.
        let destination: URL
    }

    private var byTransfer: [UInt64: Incoming] = [:]

    public init() {}

    /// Where received files live, created on first use. `Documents` itself,
    /// not a subdirectory — the Files app already shows this app as a folder.
    public nonisolated static func directory() -> URL {
        let docs = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)
        let dir = docs.first ?? FileManager.default.temporaryDirectory
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }

    /// Note an offer, and settle where it would go.
    public func remember(transfer: UInt64, peer: String, name: String, size: UInt64) {
        let safe = bulkSafeName(offered: name)
        byTransfer[transfer] = Incoming(
            peer: peer,
            name: safe,
            size: size,
            destination: Self.freePath(in: Self.directory(), named: safe))
    }

    public func destination(for transfer: UInt64) -> String? {
        byTransfer[transfer]?.destination.path
    }

    public func name(for transfer: UInt64) -> String? {
        byTransfer[transfer]?.name
    }

    public func forget(_ transfer: UInt64) {
        byTransfer.removeValue(forKey: transfer)
    }

    /// A path in `dir` nothing is using yet. Mirrors the daemon's `free_path`
    /// (not shared: filesystems aren't).
    nonisolated static func freePath(in dir: URL, named name: String) -> URL {
        let plain = dir.appendingPathComponent(name)
        guard FileManager.default.fileExists(atPath: plain.path) else { return plain }

        let base = (name as NSString).deletingPathExtension
        let ext = (name as NSString).pathExtension
        for n in 2...999 {
            let candidate = ext.isEmpty ? "\(base) (\(n))" : "\(base) (\(n)).\(ext)"
            let url = dir.appendingPathComponent(candidate)
            if !FileManager.default.fileExists(atPath: url.path) { return url }
        }
        return dir.appendingPathComponent("\(UUID().uuidString)-\(name)")
    }
}
