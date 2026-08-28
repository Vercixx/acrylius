//
//  Files this phone has offered, and where they actually are.
//
//  The mirror of the daemon's `FileBulk`, and it exists for the same reason:
//  something has to know both which transfer is which and where the bytes are,
//  and nothing above this is allowed to. What crosses the session is a name, a
//  size and an id — a peer never learns a path, and neither does the core.
//
//  Picking a file on iOS is not the same as having one. A document comes back
//  as a security-scoped URL that is only readable between
//  `startAccessingSecurityScopedResource()` and its stop, and a photo is not a
//  file at all until it is written out. Both are resolved to a plain readable
//  file here, before an offer is made, so a transfer cannot fail halfway
//  through for a reason the person who chose the file would never guess at.
//

import Foundation
#if canImport(UniformTypeIdentifiers)
import UniformTypeIdentifiers
#endif

/// Why a chosen thing could not be turned into something sendable.
public enum OutboxError: Error, LocalizedError, Sendable {
    /// A bundle rather than a file: a Live Photo, or anything else iOS keeps as
    /// a directory with an extension on it.
    case notAFile(String)

    public var errorDescription: String? {
        switch self {
        case let .notAFile(name):
            "\(name) is a bundle rather than a single file, so there is nothing to send."
        }
    }
}

public actor FileOutbox {
    public struct Outgoing: Sendable {
        public let url: URL
        public let name: String
        public let size: UInt64
        public let mime: String
        /// True when this is our copy in the temporary directory and deleting
        /// it after the transfer is ours to do.
        let temporary: Bool
    }

    private var byTransfer: [UInt64: Outgoing] = [:]
    private var next: UInt64 = 0

    public init() {}

    /// Note a file to send, and give the transfer its id.
    ///
    /// Ids are unique within this process. They are scoped to a session by the
    /// key derivation, so they need be no cleverer than a counter.
    public func offer(_ file: Outgoing) -> FfiOffer {
        next += 1
        byTransfer[next] = file
        return FfiOffer(
            transfer: next, name: file.name, size: file.size, mime: file.mime)
    }

    public func path(for transfer: UInt64) -> String? {
        byTransfer[transfer]?.url.path
    }

    public func name(for transfer: UInt64) -> String? {
        byTransfer[transfer]?.name
    }

    /// Done with a transfer, however it ended.
    ///
    /// A copy this made is removed. A file the user chose is left exactly where
    /// it was: it is theirs, and sending it is not a reason to touch it.
    public func forget(_ transfer: UInt64) {
        guard let file = byTransfer.removeValue(forKey: transfer) else { return }
        if file.temporary {
            try? FileManager.default.removeItem(at: file.url)
        }
    }

    /// Take a security-scoped URL from a document picker and make it readable.
    ///
    /// Copied rather than held open. A scoped URL stops being readable the
    /// moment the picker's grant lapses, and a transfer outlives the tap that
    /// started it — a large file over a slow network by minutes.
    public nonisolated static func fromPicked(_ url: URL) throws -> Outgoing {
        #if canImport(Darwin)
        let scoped = url.startAccessingSecurityScopedResource()
        defer { if scoped { url.stopAccessingSecurityScopedResource() } }
        #endif

        let name = url.lastPathComponent
        let copy = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        try FileManager.default.createDirectory(at: copy, withIntermediateDirectories: true)
        let destination = copy.appendingPathComponent(name.isEmpty ? "file" : name)
        try FileManager.default.copyItem(at: url, to: destination)

        // A directory is not a file, and refusing one here is the difference
        // between saying so and sending nothing.
        //
        // Not hypothetical: a Live Photo is a `.pvt` bundle — a directory
        // holding a still and a movie — and `copyItem` copies it happily.
        // `fileSizeKey` is absent for a directory, so it became an offer of
        // zero bytes, and the far end got a zero-byte `.pvt`. Whatever else is
        // wrong, an offer whose size was never read is not one to make.
        let values = try destination.resourceValues(forKeys: [.fileSizeKey, .isRegularFileKey])
        guard values.isRegularFile == true, let size = values.fileSize else {
            throw OutboxError.notAFile(destination.lastPathComponent)
        }
        return Outgoing(
            url: destination,
            name: destination.lastPathComponent,
            size: UInt64(size),
            mime: mimeType(for: destination),
            temporary: true)
    }

    // `fromData` used to live here, taking bytes and a name made up by the
    // caller because the photo picker had handed over one and not the other.
    // Every photo went out as `photo.<ext>`. A picker item can be loaded as a
    // file instead, which arrives with its own name, so there is nothing left
    // that needs to invent one — and a function that takes a name on trust is
    // an invitation to invent one again.
}

/// A content type for the offer, from the file's own extension.
///
/// Advisory. The receiver decides what to do with what arrives and is not
/// entitled to trust this; it is here so a computer can show a sensible icon.
private func mimeType(for url: URL) -> String {
    #if canImport(UniformTypeIdentifiers)
    if let type = UTType(filenameExtension: url.pathExtension),
       let mime = type.preferredMIMEType {
        return mime
    }
    #endif
    return "application/octet-stream"
}
