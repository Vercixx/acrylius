#if canImport(SwiftUI)

import SwiftUI
#if canImport(UIKit)
import UIKit
#endif

/// The three things this app is for: the computers, the files moving between
/// them, and this phone. Split into tabs so a badge can surface a waiting
/// offer even when the user has navigated away from it.
struct RootView: View {
    @Environment(AppModel.self) private var model

    @State private var pane: Pane = .devices
    @State private var showPair = false
    /// A path of device ids, so a widget tap can push a computer's screen directly.
    @State private var devicePath: [String] = []

    /// Held apart from the model; see the alert below.
    @State private var showingError = false
    @State private var shownError = ""

    /// Not `Tab`: the iOS 26 SDK's own `SwiftUI.Tab` would be shadowed.
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
                .badge(model.incoming.count)
                .tag(Pane.files)

            StatusView()
                .tabItem { Label("Status", systemImage: "gauge.with.dots.needle.bottom.50percent") }
                .tag(Pane.status)
        }
        .modifier(MinimizingTabBar())
        .sheet(isPresented: $showPair) { PairView() }
        .sheet(isPresented: .constant(model.pairingSas != nil)) { ConfirmPairingView() }
        // Presented from local state, not bound to `model.lastError != nil`
        // directly: that binding re-evaluates on every model change (several
        // times a second during media playback) and dismisses itself early.
        .alert("Something went wrong", isPresented: $showingError) {
            Button("OK", role: .cancel) { model.dismissError() }
        } message: {
            Text(shownError)
        }
        .onChange(of: model.lastErrorAt) {
            guard let text = model.lastError else { return }
            shownError = text
            showingError = true
        }
        .onOpenURL { url in
            // acrylius://peer/<device-id>, as carried by a widget.
            guard url.scheme == "acrylius", url.host == "peer" else { return }
            let deviceId = url.lastPathComponent
            guard !deviceId.isEmpty else { return }
            // Switch the tab too, or the path change lands on whichever tab was last open.
            pane = .devices
            devicePath = [deviceId]
        }
    }
}

/// Lets the tab bar shrink while scrolling, on systems that support it.
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
