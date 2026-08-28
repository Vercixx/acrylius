//
//  Files offered to this phone, and where they will land.
//
//  The mirror of `FileOutbox`, and the counterpart of the daemon's share
//  directory. Something has to hold the name a peer chose between the offer
//  arriving and the bytes being accepted, because the core carries neither: an
//  offer is a name, a size and an id, and the `BulkListen` that follows an
//  acceptance carries only the id, a key and a byte count.
//
//  Files go in the app's Documents directory, which is the one place iOS lets
//  another app reach with the user's help. With `UIFileSharingEnabled` the
//  Files app shows it as "Acrylius" under On My iPhone, so what arrives here
//  can be opened, moved, or handed to something else — including a sideloader's
//  import picker, which is the only way an `.ipa` on a phone ever becomes an
//  app.
//
//  What it deliberately does not do is decide what a file may be called. That
//  rule is `bulkSafeName`, which is `acrylius_proto::bulk::safe_name` — the
//  same function the daemon uses. A peer picks a name and nothing else; a
//  second implementation of "and nothing else" is how one of the two ends up
//  writing outside its directory.
//

import Foundation

public actor FileInbox {
    /// An offer that has arrived and not yet finished.
    public struct Incoming: Sendable {
        public let peer: String
        public let name: String
        public let size: UInt64
        /// Where it will be written, decided when the offer arrives so that a
        /// name collision is resolved before anyone accepts rather than after
        /// the bytes are already moving.
        let destination: URL
    }

    private var byTransfer: [UInt64: Incoming] = [:]

    public init() {}

    /// Where received files live. Created on first use.
    ///
    /// `Documents` itself rather than a subdirectory of it: the Files app shows
    /// this app as a folder already, and burying everything one level deeper
    /// buys nothing but an extra tap.
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

    /// A path in `dir` nothing is using yet.
    ///
    /// Two photos called the same thing is ordinary; losing one is not. The
    /// daemon's `free_path` does the same, and for the same reason — it is not
    /// shared because it is about a filesystem, and these two do not have one
    /// in common.
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
