#if canImport(SwiftUI)

import SwiftUI

/// A button for something that takes a moment and happens somewhere else.
///
/// Every action on these screens is a round trip to another machine, so a tap
/// looks like nothing happened until the answer arrives. The default press
/// highlight is a few milliseconds long and says only that the tap landed, not
/// that anything came of it — so this adds a spinner while the request is out
/// and a brief tick when it comes back.
///
/// It keeps the ordinary button styling rather than imposing its own. A custom
/// `ButtonStyle` would take over the tint and the row highlight too, and
/// replacing the platform's behaviour is a poor trade for adding to it.
struct TaskButton<Label: View>: View {
    private let role: ButtonRole?
    private let action: () async -> Void
    private let label: Label

    @State private var phase: Phase = .idle

    private enum Phase {
        case idle
        case running
        case done
    }

    init(role: ButtonRole? = nil,
         action: @escaping () async -> Void,
         @ViewBuilder label: () -> Label) {
        self.role = role
        self.action = action
        self.label = label()
    }

    var body: some View {
        Button(role: role) {
            guard phase != .running else { return }
            phase = .running
            Task {
                await action()
                phase = .done
                // Long enough to notice, short enough not to look like state.
                try? await Task.sleep(for: .milliseconds(1200))
                if phase == .done { phase = .idle }
            }
        } label: {
            HStack {
                label
                Spacer(minLength: 8)
                switch phase {
                case .running:
                    ProgressView().controlSize(.small)
                case .done:
                    Image(systemName: "checkmark")
                        .foregroundStyle(.secondary)
                        .transition(.opacity)
                case .idle:
                    EmptyView()
                }
            }
            .animation(.easeOut(duration: 0.15), value: phase)
        }
        // Not disabled while running: disabling greys the label out, which
        // reads as "unavailable" rather than "working". The guard above already
        // stops a second tap.
    }
}

extension TaskButton where Label == Text {
    init(_ title: String, role: ButtonRole? = nil, action: @escaping () async -> Void) {
        self.init(role: role, action: action) { Text(title) }
    }
}

#endif
