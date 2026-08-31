#if canImport(SwiftUI)

import SwiftUI

/// Transport controls for whatever is playing on a computer.
/// Hidden entirely when the peer reports no active player.
struct MediaSection: View {
    @Environment(AppModel.self) private var model
    let peer: FfiPeer

    private var features: PeerFeatures { model.catalog[peer.deviceId] }
    private var player: FfiMediaPlayer? { features.activePlayer }

    var body: some View {
        if let player {
            Section {
                VStack(alignment: .leading, spacing: 4) {
                    Text(player.title.isEmpty ? player.name : player.title)
                        .font(.headline)
                        .lineLimit(2)
                    if !player.artist.isEmpty {
                        Text(player.artist)
                            .font(.subheadline)
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                    }
                    if player.lengthMs > 0 {
                        timeline(player)
                    }
                }
                .padding(.vertical, 2)

                controls(for: player)

                // Machine volume when reported, else the player's own.
                if let volume = features.media?.systemVolume {
                    volumeRow(volume, machine: true)
                } else if let volume = player.volumePercent {
                    volumeRow(volume, machine: false)
                }

                if features.media?.players.count ?? 0 > 1 {
                    otherPlayers()
                }
            } header: {
                Text("Playing on \(player.name)")
            } footer: {
                if !player.canControl {
                    Text("This player reports what it is doing but does not accept control.")
                }
            }
            // Position isn't pushed by the peer, only polled while this view is
            // visible and active; the rate is adaptive (see `interval`).
            .task(id: pollKey) {
                guard scenePhase == .active, scrubbing == nil else { return }
                while !Task.isCancelled {
                    await model.refreshMedia(peer)
                    try? await Task.sleep(for: .milliseconds(Int(interval)))
                }
            }
            // Separate from the network poll: this just extrapolates the shown
            // position locally, and only this loop needs to be smooth.
            .task(id: peer.deviceId) {
                while !Task.isCancelled {
                    // Skipped while dragging, or the slider springs back under the finger.
                    if scrubbing == nil {
                        shownMs = features.positionMs(at: Date())
                    }
                    try? await Task.sleep(for: Self.tick)
                }
            }
        }
    }

    /// Where the track is, and — where the player allows it — where to put it.
    ///
    /// Not inside a `TimelineView`: its periodic rebuild replaced the `Slider`
    /// mid-gesture, breaking drags. Position advances from `shownMs` instead.
    @ViewBuilder
    private func timeline(_ player: FfiMediaPlayer) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            if player.canSeek {
                Slider(
                    // While dragging, the local value wins over the peer's report.
                    value: Binding(
                        get: { scrubbing ?? Double(shownMs) },
                        set: { scrubbing = $0 }
                    ),
                    in: 0...Double(max(player.lengthMs, 1))
                ) { editing in
                    // Sent on release only — a drag emits a value per frame.
                    guard !editing, let target = scrubbing else { return }
                    // Feedback trigger: a drag release has no button press to attach it to.
                    seeks &+= 1
                    Task {
                        // Optimistic update, so the knob doesn't spring back while the peer catches up.
                        shownMs = UInt64(target)
                        _ = await model.media(peer, "position", value: Int64(target))
                        scrubbing = nil
                    }
                }
                .tint(.secondary)
                .sensoryFeedback(.impact(weight: .light), trigger: seeks)
            } else {
                ProgressView(value: Double(shownMs), total: Double(max(player.lengthMs, 1)))
                    .tint(.secondary)
            }
            Text("\(clock(scrubbing.map { UInt64($0) } ?? shownMs)) / \(clock(player.lengthMs))")
                .font(.caption2.monospacedDigit())
                .foregroundStyle(.tertiary)
        }
    }

    @ViewBuilder
    private func controls(for player: FfiMediaPlayer) -> some View {
        HStack {
            Spacer()
            transport("backward.end.fill", enabled: player.canGoPrevious) {
                await model.media(peer, "previous")
            }
            Spacer()
            transport(
                player.status == "playing" ? "pause.fill" : "play.fill",
                enabled: player.canControl,
                size: .title
            ) {
                await model.media(peer, "playpause")
            }
            Spacer()
            transport("forward.end.fill", enabled: player.canGoNext) {
                await model.media(peer, "next")
            }
            Spacer()
        }
        // Without this a tap anywhere in a List row activates its first control.
        .buttonStyle(.borderless)
    }

    /// One transport button, disabled per the player's reported capabilities.
    /// Uses `TaskButton` since `model.media` reports whether the action took effect.
    private func transport(
        _ symbol: String,
        enabled: Bool,
        size: Font = .title2,
        action: @escaping () async -> Bool
    ) -> some View {
        TaskButton(symbol: symbol, action: action)
            .font(size)
            .disabled(!enabled)
    }

    /// The computer's output volume where there is one, the player's otherwise.
    /// Many MPRIS players report control support but ignore volume, so the
    /// machine's volume wins when available.
    private func volumeRow(_ volume: UInt8, machine: Bool) -> some View {
        HStack {
            Image(systemName: "speaker.fill").foregroundStyle(.secondary)
            Slider(
                value: $dragging,
                in: 0...100,
                onEditingChanged: { editing in
                    // Sent on release only, not per drag frame.
                    guard !editing else { return }
                    Task {
                        // Naming the player asks for its own volume rather than the machine's.
                        await model.media(
                            peer, "volume",
                            player: machine ? "" : (player?.id ?? ""),
                            value: Int64(dragging))
                    }
                }
            )
            Image(systemName: "speaker.wave.3.fill").foregroundStyle(.secondary)
            // Whole percents come straight off the wire. Fixed width so digits
            // don't twitch under a dragging thumb.
            Text("\(Int(dragging.rounded()))%")
                .font(.caption.monospacedDigit())
                .foregroundStyle(.secondary)
                .frame(width: 40, alignment: .trailing)
        }
        // Not a direct binding to the reported value: that would yank the knob
        // back on every answer that arrives mid-gesture.
        .onAppear { dragging = Double(volume) }
        .onChange(of: volume) { _, new in dragging = Double(new) }
    }

    /// The other open players, so a command can be aimed at one of them.
    @ViewBuilder
    private func otherPlayers() -> some View {
        if let media = features.media {
            ForEach(media.players.filter { $0.id != media.active }, id: \.id) { other in
                Button {
                    Task { await model.media(peer, "playpause", player: other.id) }
                } label: {
                    HStack {
                        VStack(alignment: .leading) {
                            Text(other.name)
                            if !other.title.isEmpty {
                                Text(other.title)
                                    .font(.caption)
                                    .foregroundStyle(.secondary)
                                    .lineLimit(1)
                            }
                        }
                        Spacer()
                        Text(other.status).font(.caption).foregroundStyle(.tertiary)
                    }
                }
                .disabled(!other.canControl)
            }
        }
    }

    /// Where the volume knob is while a finger is on it.
    @State private var dragging: Double = 0

    /// Where the timeline is while a finger is on it, `nil` when the peer's
    /// own reading should win.
    @State private var scrubbing: Double?

    /// Counts seek requests to trigger feedback per seek; only changes
    /// matter, never the value, so it wraps freely.
    @State private var seeks: UInt = 0

    /// The position on screen, extrapolated from the last reading rather than
    /// redrawn on a `TimelineView` tick (which visibly lagged by up to a second).
    @State private var shownMs: UInt64 = 0

    /// How often the displayed position is recomputed locally, independent
    /// of the network poll interval below.
    private static let tick = Duration.milliseconds(250)

    @Environment(\.scenePhase) private var scenePhase

    /// How long to wait between readings; values come from the core.
    private var interval: UInt64 {
        guard player?.status == "playing" else { return mediaIdleIntervalMs() }
        // BLE round trips cost several fragments each way, hence the slower interval.
        return peer.transport == .bleGatt
            ? mediaWatchSlowIntervalMs()
            : mediaWatchIntervalMs()
    }

    /// Restarting `.task(id:)` on any change makes polling adaptive and stops
    /// it while backgrounded. `scrubbing` is included because a poll landing
    /// mid-drag rebuilds the `Slider` and breaks the gesture.
    private var pollKey: String {
        "\(peer.deviceId)|\(interval)|\(scenePhase == .active)|\(scrubbing == nil)"
    }

    private func clock(_ ms: UInt64) -> String {
        let secs = ms / 1000
        return String(format: "%d:%02d", secs / 60, secs % 60)
    }
}

#endif
