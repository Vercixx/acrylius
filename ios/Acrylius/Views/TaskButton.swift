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
    private let action: () async -> Bool
    private let label: Label

    @State private var phase: Phase = .idle

    private enum Phase {
        case idle
        case running
        case ok
        case failed
    }

    /// `action` returns whether the thing actually happened, not whether the
    /// request was delivered. A tick for "sent" is a claim the user cannot
    /// check and will believe — an unlock that the screen locker ignored looked
    /// exactly like one that worked.
    init(role: ButtonRole? = nil,
         action: @escaping () async -> Bool,
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
                phase = await action() ? .ok : .failed
                // Long enough to notice, short enough not to look like state.
                // A failure lingers, because it is worth reading.
                try? await Task.sleep(for: .milliseconds(phase == .ok ? 1200 : 2600))
                if phase != .running { phase = .idle }
            }
        } label: {
            HStack {
                label
                Spacer(minLength: 8)
                switch phase {
                case .running:
                    ProgressView().controlSize(.small)
                case .ok:
                    Image(systemName: "checkmark")
                        .foregroundStyle(.secondary)
                        .transition(.opacity)
                case .failed:
                    Image(systemName: "exclamationmark.triangle")
                        .foregroundStyle(.orange)
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
    init(_ title: String, role: ButtonRole? = nil, action: @escaping () async -> Bool) {
        self.init(role: role, action: action) { Text(title) }
    }
}

#endif
