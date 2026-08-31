//
//  Sending a file to a computer.
//
//  Two pickers, since iOS keeps photos and files apart. No share-sheet
//  extension: pickers cost no extra App ID against the free account's limit.
//

#if canImport(SwiftUI) && canImport(PhotosUI)

import PhotosUI
import SwiftUI
// Named rather than leant on: `.livePhoto`, `.movie`, `conforms(to:)` are this
// module's, not PhotosUI's.
import UniformTypeIdentifiers

/// A photo or video as the picker hands it over, name and all.
///
/// A file representation (rather than raw `Data`) keeps the real name attached
/// — the far end decides file type from the extension, and a guessed one can
/// be wrong (e.g. HEIC content under a `.jpg` name).
///
/// Asks for `.image`/`.movie` specifically, not the root `.item` type: a Live
/// Photo is a `.pvt` bundle under `.item` and copies as zero bytes.
private struct PickedStill: Transferable {
    let file: FileOutbox.Outgoing

    static var transferRepresentation: some TransferRepresentation {
        FileRepresentation(importedContentType: .image) { received in
            PickedStill(file: try FileOutbox.fromPicked(received.file))
        }
    }
}

/// The same, for something that moves. See [`PickedStill`].
private struct PickedMovie: Transferable {
    let file: FileOutbox.Outgoing

    static var transferRepresentation: some TransferRepresentation {
        FileRepresentation(importedContentType: .movie) { received in
            // Copied here, before `received.file` is removed on return.
            PickedMovie(file: try FileOutbox.fromPicked(received.file))
        }
    }
}

struct SendFileSection: View {
    @Environment(AppModel.self) private var model
    let peer: FfiPeer

    @State private var browsing = false
    @State private var photo: PhotosPickerItem?
    @State private var busy = false

    var body: some View {
        Section {
            Button {
                browsing = true
            } label: {
                Label("Send a file", systemImage: "doc")
            }
            .disabled(busy)

            PhotosPicker(selection: $photo, matching: .any(of: [.images, .videos])) {
                Label("Send a photo or video", systemImage: "photo")
            }
            .disabled(busy)

            ForEach(inFlight, id: \.self) { name in
                HStack {
                    ProgressView().controlSize(.small)
                    Text(name).font(.callout).lineLimit(1)
                }
            }
        } header: {
            Text("Send")
        }
        .fileImporter(isPresented: $browsing, allowedContentTypes: [.item]) { result in
            guard case let .success(url) = result else { return }
            offer { try FileOutbox.fromPicked(url) }
        }
        .onChange(of: photo) { _, item in
            guard let item else { return }
            Task {
                // A Live Photo declares a movie type too; checked first so it
                // sends as the still, which is what was actually picked.
                let types = item.supportedContentTypes
                let isLive = types.contains { $0.conforms(to: .livePhoto) }
                let moving = !isLive && types.contains { $0.conforms(to: .movie) }

                let file =
                    moving
                    ? (try? await item.loadTransferable(type: PickedMovie.self))?.file
                    : (try? await item.loadTransferable(type: PickedStill.self))?.file
                guard let file else {
                    model.lastError = "That photo could not be read."
                    photo = nil
                    return
                }
                offer { file }
                photo = nil
            }
        }
    }

    private var inFlight: [String] {
        model.sending.sorted { $0.key < $1.key }.map(\.value)
    }

    /// Resolve a pick into a readable file, then offer it. Copying happens
    /// before the offer so a lapsed picker grant can't fail a transfer midway.
    private func offer(_ resolve: @escaping () throws -> FileOutbox.Outgoing) {
        busy = true
        Task {
            defer { busy = false }
            do {
                let file = try resolve()
                await model.sendFile(file, to: peer)
            } catch {
                model.lastError = "That file could not be read: \(error.localizedDescription)"
            }
        }
    }
}

#endif
