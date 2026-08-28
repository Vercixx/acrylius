//
//  Sending a file to a computer.
//
//  Two pickers rather than one, because iOS keeps photos and files apart and a
//  person looking for a photo will not find it behind "Browse". Both end up in
//  the same place: a readable file, an offer, and then the computer's answer.
//
//  There is no share-sheet extension. Every extension is another App ID against
//  a free account's ten a week, and one is already spent on the widget; the
//  pickers cost nothing and reach the same files.
//

#if canImport(SwiftUI) && canImport(PhotosUI)

import PhotosUI
import SwiftUI
// Named rather than leant on: `.item` happened to resolve through PhotosUI, and
// `.livePhoto`, `.movie` and `conforms(to:)` are this module's, not that one's.
import UniformTypeIdentifiers

/// A photo or video as the picker hands it over, name and all.
///
/// Loading it as `Data` gave bytes and nothing else, so a name had to be
/// invented — every photo went out as `photo.<ext>`, with the extension guessed
/// from `supportedContentTypes.first`. Generic, and not reliably even right: the
/// picker may hand over something other than the original, so a photo could
/// arrive called `.heic` with JPEG inside it, and the far end decides what a
/// file is by its extension.
///
/// A file representation keeps the two together. The name is the one the item
/// actually has — `IMG_0123.HEIC`, `IMG_0124.MOV` — and the bytes are the ones
/// that name describes.
///
/// Two of these rather than one asking for `.item`, which is the root type and
/// therefore accepts anything — including a Live Photo, which iOS hands over as
/// a `.pvt` *bundle*: a directory holding a still and a movie. That copied
/// happily and offered zero bytes. Asking for a still or a movie by name is what
/// keeps a bundle from ever being the answer.
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
            // Copied inside the closure on purpose: the received file is removed
            // as soon as it returns, and a transfer outlives the tap that
            // started it by however long the file takes.
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
                // A Live Photo declares a movie type as well as a still, so it
                // is asked about first: what a person picked is the photo, and
                // sending only the video half of it would be a stranger answer
                // than sending the still.
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

    /// Resolve a pick into a readable file, then offer it.
    ///
    /// Copying happens before the offer, so a transfer cannot fail halfway
    /// through because a picker's grant lapsed — which would look, to the
    /// person who chose the file, like the network dropping.
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
