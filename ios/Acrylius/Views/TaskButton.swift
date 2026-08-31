#if canImport(SwiftUI)

import SwiftUI

/// A button for an action that is a round trip to another machine: shows a
/// spinner while in flight and a brief tick or warning on completion, since
/// the default press highlight only says the tap landed, not that it worked.
struct TaskButton<Label: View>: View {
    private let role: ButtonRole?
    private let action: () async -> Bool
    private let label: Label
    private let feedback: Feedback

    /// Where the outcome is drawn.
    enum Feedback {
        /// Beside the label, for list rows with width to spare.
        case trailing
        /// In place of the label, for controls with no width to give away.
        /// No success tick either — a transport control's success is already
        /// visible in what it changed (play becomes pause), only failure needs drawing.
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
    /// request was delivered.
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
                // A failure lingers longer, since it's worth reading.
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
                    // Hidden, not removed, so the row doesn't jump by the icon's width.
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
        // Not disabled while running: disabling greys the label out, reading as
        // "unavailable" rather than "working". The guard above stops a second tap.
        //
        // `.sensoryFeedback` over `UIFeedbackGenerator`: honours the system
        // setting, does nothing in the background, no availability guard needed.
        .sensoryFeedback(trigger: phase) { _, now in
            switch now {
            case .ok: .success
            case .failed: .error
            // Nothing for the press itself — iOS already gives the button its own.
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
    /// Size with `.font()` on the button; the spinner uses `controlSize` instead.
    init(symbol: String, action: @escaping () async -> Bool) {
        self.init(feedback: .inPlace, action: action) {
            Image(systemName: symbol)
        }
    }
}

#endif
