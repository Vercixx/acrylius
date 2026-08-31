//
//  Files this phone has offered, and where they actually are. Mirrors the
//  daemon's `FileBulk`; a peer only ever learns a name, size and id, never a
//  path. Picked documents/photos are copied to a plain file up front.
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

    /// Note a file to send, and give the transfer its id. Ids only need be
    /// unique within this process; the session's key derivation scopes them.
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

    /// Done with a transfer, however it ended. Removes a copy this made;
    /// leaves a user-chosen file untouched.
    public func forget(_ transfer: UInt64) {
        guard let file = byTransfer.removeValue(forKey: transfer) else { return }
        if file.temporary {
            try? FileManager.default.removeItem(at: file.url)
        }
    }

    /// Take a security-scoped URL from a document picker and make it readable.
    /// Copied rather than held open — the scoped grant lapses before a slow transfer finishes.
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

        // A Live Photo is a `.pvt` bundle (a directory), which `copyItem`
        // copies without complaint; guard against a directory explicitly.
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

}

/// A content type for the offer, guessed from the file's extension. Advisory
/// only — the receiver decides what to do with what arrives.
private func mimeType(for url: URL) -> String {
    #if canImport(UniformTypeIdentifiers)
    if let type = UTType(filenameExtension: url.pathExtension),
       let mime = type.preferredMIMEType {
        return mime
    }
    #endif
    return "application/octet-stream"
}
