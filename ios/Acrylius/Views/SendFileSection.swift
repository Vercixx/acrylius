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
            Text("Files")
        }
        .fileImporter(isPresented: $browsing, allowedContentTypes: [.item]) { result in
            guard case let .success(url) = result else { return }
            offer { try FileOutbox.fromPicked(url) }
        }
        .onChange(of: photo) { _, item in
            guard let item else { return }
            Task {
                // A photo is not a file until it is written out, and the picker
                // hands over bytes rather than somewhere to read them from.
                guard let data = try? await item.loadTransferable(type: Data.self) else {
                    model.lastError = "That photo could not be read."
                    photo = nil
                    return
                }
                let name = item.supportedContentTypes.first?.preferredFilenameExtension
                    .map { "photo.\($0)" } ?? "photo.jpg"
                offer { try FileOutbox.fromData(data, suggestedName: name) }
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
