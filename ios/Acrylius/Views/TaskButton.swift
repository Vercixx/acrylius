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
    private let feedback: Feedback

    /// Where the outcome is drawn.
    enum Feedback {
        /// Beside the label, across the width of the row. For list rows, which
        /// have width to spare and nothing else competing for it.
        case trailing
        /// In place of the label. For a control standing in a row with others,
        /// which has no width to give away — three transport buttons each
        /// growing a spacer would push each other off the screen.
        ///
        /// No tick, either. Success for a transport control is already visible
        /// in the thing it changed: play becomes pause, the track title
        /// changes. Only the failure needs drawing, because a player that
        /// ignored the command looks exactly like one that was never asked.
        case inPlace
    }

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
         feedback: Feedback = .trailing,
         action: @escaping () async -> Bool,
         @ViewBuilder label: () -> Label) {
        self.role = role
        self.feedback = feedback
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
            switch feedback {
            case .trailing:
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
            case .inPlace:
                label
                    // Hidden rather than removed, so the row does not jump by
                    // the width of an icon every time something is pressed.
                    .opacity(phase == .running ? 0 : 1)
                    .overlay {
                        if phase == .running {
                            ProgressView().controlSize(.small)
                        }
                    }
                    .foregroundStyle(phase == .failed ? AnyShapeStyle(.orange) : AnyShapeStyle(.tint))
                    .animation(.easeOut(duration: 0.15), value: phase)
            }
        }
        // Not disabled while running: disabling greys the label out, which
        // reads as "unavailable" rather than "working". The guard above already
        // stops a second tap.
        //
        // Felt as well as seen, and keyed to the same thing the tick is: what
        // came *back*, not that a tap was registered. Every action on these
        // screens is a round trip, so the interesting moment is a second or two
        // after the finger has gone — which is exactly the moment a person has
        // looked away from the phone, and the one a screen cannot report to
        // them. Two outcomes, told apart: an unlock the far end ignored must
        // not feel like one it carried out.
        //
        // `.sensoryFeedback` rather than a `UIFeedbackGenerator`: it honours
        // the system setting, does nothing in the background, and needs no
        // availability guard at this deployment target.
        .sensoryFeedback(trigger: phase) { _, now in
            switch now {
            case .ok: .success
            case .failed: .error
            // Nothing for the press itself. iOS already gives a button its own
            // feedback, and a second buzz on the way out would say "sent",
            // which is the claim this whole type exists to avoid making.
            case .idle, .running: nil
            }
        }
    }
}

extension TaskButton where Label == Text {
    init(_ title: String, role: ButtonRole? = nil, action: @escaping () async -> Bool) {
        self.init(role: role, action: action) { Text(title) }
    }
}

extension TaskButton where Label == Image {
    /// A symbol that reports its own failure. See ``Feedback/inPlace``.
    ///
    /// Size it with `.font()` on the button: the symbol picks that up from the
    /// environment, while the spinner that replaces it is sized by
    /// `controlSize` and stays put either way.
    init(symbol: String, action: @escaping () async -> Bool) {
        self.init(feedback: .inPlace, action: action) {
            Image(systemName: symbol)
        }
    }
}

#endif
