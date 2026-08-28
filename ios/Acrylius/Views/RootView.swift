#if canImport(SwiftUI)

import SwiftUI
#if canImport(UIKit)
import UIKit
#endif

/// The three things this app is for: the computers, the files moving between
/// them, and this phone.
///
/// It was one `List` with everything stacked in it, which had a real cost
/// rather than only an aesthetic one: an incoming file offer rendered on the
/// root screen alone, so a person standing inside a computer's screen could not
/// see that the machine in front of them was waiting on an answer. A tab can
/// carry a badge; a section further down a list someone has navigated away from
/// cannot.
struct RootView: View {
    @Environment(AppModel.self) private var model

    @State private var pane: Pane = .devices
    @State private var showPair = false
    /// Device ids. A path rather than plain links so a widget tap can push a
    /// computer's screen without the user finding it in the list.
    @State private var devicePath: [String] = []

    /// Not called `Tab`: the iOS 26 SDK has a `SwiftUI.Tab` of its own, and a
    /// nested type with that name would shadow it inside this file for no
    /// reason.
    enum Pane: Hashable {
        case devices, files, status
    }

    var body: some View {
        TabView(selection: $pane) {
            DeviceListView(path: $devicePath, showPair: $showPair)
                .tabItem { Label("Devices", systemImage: "desktopcomputer") }
                .tag(Pane.devices)

            FilesView()
                .tabItem { Label("Files", systemImage: "arrow.up.arrow.down") }
                // Zero renders nothing, so this is only ever a count of things
                // actually waiting on a person.
                .badge(model.incoming.count)
                .tag(Pane.files)

            StatusView()
                .tabItem { Label("Status", systemImage: "gauge.with.dots.needle.bottom.50percent") }
                .tag(Pane.status)
        }
        .modifier(MinimizingTabBar())
        .sheet(isPresented: $showPair) { PairView() }
        .sheet(isPresented: .constant(model.pairingSas != nil)) { ConfirmPairingView() }
        // Above the tabs rather than at the bottom of one list, because an
        // error raised on any tab is worth seeing from all of them — a
        // Bluetooth problem reported while looking at Files used to land in a
        // footnote on Devices.
        .safeAreaInset(edge: .top) {
            if let error = model.lastError {
                ErrorBanner(text: error) { model.dismissError() }
            }
        }
        // Restarted whenever a new error arrives, so the clock is always the
        // current error's. Nothing used to clear these at all.
        .task(id: model.lastErrorAt) {
            guard model.lastError != nil else { return }
            try? await Task.sleep(for: .seconds(AppModel.errorLifetime))
            guard !Task.isCancelled else { return }
            model.dismissError()
        }
        .onOpenURL { url in
            // acrylius://peer/<device-id>, which is what a widget carries.
            guard url.scheme == "acrylius", url.host == "peer" else { return }
            let deviceId = url.lastPathComponent
            guard !deviceId.isEmpty else { return }
            // The tab as well as the path: a widget tap that only set the path
            // would land on whichever tab was last open and look like it did
            // nothing.
            pane = .devices
            devicePath = [deviceId]
        }
    }
}

/// Lets the tab bar shrink out of the way while reading, on the systems that
/// have it.
private struct MinimizingTabBar: ViewModifier {
    func body(content: Content) -> some View {
        if #available(iOS 26, *) {
            content.tabBarMinimizeBehavior(.onScrollDown)
        } else {
            content
        }
    }
}

/// One error, and a way to be rid of it.
private struct ErrorBanner: View {
    let text: String
    let dismiss: () -> Void

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 12) {
            Image(systemName: "exclamationmark.triangle.fill")
                .foregroundStyle(.orange)
            Text(text)
                .font(.footnote)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
            Button("Dismiss", systemImage: "xmark") { dismiss() }
                .labelStyle(.iconOnly)
                .buttonStyle(.borderless)
                .foregroundStyle(.secondary)
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 10)
        .acrylicGlass(in: .rect(cornerRadius: 14))
        .padding(.horizontal, 12)
    }
}

#endif
