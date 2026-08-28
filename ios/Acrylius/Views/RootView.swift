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
        // A system alert, not a banner drawn by hand.
        //
        // It was a glass capsule floating over the tab bar, which looked like
        // something the app had invented — because it was. An alert is the
        // control iOS uses to say this, so it inherits the platform's shape,
        // its dimming, its Dynamic Type and its VoiceOver behaviour, none of
        // which the banner had.
        .alert(
            "Something went wrong",
            isPresented: Binding(
                get: { model.lastError != nil },
                set: { if !$0 { model.dismissError() } }
            ),
            presenting: model.lastError
        ) { _ in
            Button("OK", role: .cancel) { model.dismissError() }
        } message: { text in
            Text(text)
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

#endif
