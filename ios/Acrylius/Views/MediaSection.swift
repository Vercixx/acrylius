#if canImport(SwiftUI)

import SwiftUI

/// Transport controls for whatever is playing on a computer.
///
/// Shown only when the peer has told us it has a player. A machine with nothing
/// open sends an empty list, and a remote offering buttons for nothing is a
/// remote people conclude is broken.
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
                        // Redrawn every second against the reading's own
                        // timestamp, and re-read from the computer on the
                        // interval below. Showing the last reported position
                        // alone froze the clock until you pressed something;
                        // advancing it without ever re-asking would drift and
                        // would keep running after the music stopped somewhere
                        // this phone cannot see. The refresh below is what
                        // makes the estimate safe to draw.
                        TimelineView(.periodic(from: .now, by: 1)) { tick in
                            timeline(player, at: features.positionMs(at: tick.date))
                        }
                    }
                }
                .padding(.vertical, 2)

                controls(for: player)

                // The machine's volume when it reports one, the player's own
                // when it does not. Showing neither is how this control
                // disappeared: a computer running an older daemon sends no
                // machine volume, and a row that only knew about that had
                // nothing left to draw.
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
            // A computer announces a media change on its own, but not a
            // position: a track playing normally changes nothing else for
            // minutes at a time, and broadcasting once a second forever so a
            // clock can tick would be absurd. So the side that cares asks, and
            // only while someone is looking at it.
            //
            // The rate is not fixed. It used to be a flat two seconds written
            // into this file, which was both too slow to look live and too fast
            // to be spending on a paused track over Bluetooth — and it went on
            // running at that rate with the phone in a pocket, because a `.task`
            // on a view that is merely off screen is not cancelled.
            .task(id: pollKey) {
                guard scenePhase == .active else { return }
                while !Task.isCancelled {
                    await model.refreshMedia(peer)
                    try? await Task.sleep(for: .milliseconds(Int(interval)))
                }
            }
        }
    }

    /// Where the track is, and — where the player allows it — where to put it.
    ///
    /// `canSeek` has been decoded off the wire since M2 and read by nothing.
    /// A player that cannot seek gets the bar it always had: a slider that
    /// silently refuses to move the track is worse than an honest readout.
    @ViewBuilder
    private func timeline(_ player: FfiMediaPlayer, at reported: UInt64) -> some View {
        // While a finger is down, the drag is the truth. Otherwise the peer is.
        let shown = scrubbing ?? Double(reported)
        VStack(alignment: .leading, spacing: 4) {
            if player.canSeek {
                Slider(
                    value: Binding(get: { shown }, set: { scrubbing = $0 }),
                    in: 0...Double(player.lengthMs)
                ) { editing in
                    // Sent on release. A drag emits a value per frame and each
                    // one here would be a round trip to another machine.
                    guard !editing, let target = scrubbing else { return }
                    Task {
                        _ = await model.media(peer, "position", value: Int64(target))
                        // Held until the answer lands, so the knob does not
                        // snap back to the old position for the half second
                        // before the new reading arrives — which reads exactly
                        // like a seek that failed.
                        scrubbing = nil
                    }
                }
                .tint(.secondary)
            } else {
                ProgressView(value: shown, total: Double(player.lengthMs))
                    .tint(.secondary)
            }
            Text("\(clock(UInt64(shown))) / \(clock(player.lengthMs))")
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
        // The row is buttons, not a row that is one button: without this a tap
        // anywhere in a List row activates the first control in it.
        .buttonStyle(.borderless)
    }

    /// One transport button.
    ///
    /// Disabled from what the player said it can do, so a control that would be
    /// ignored is visibly unavailable rather than silently inert. And a
    /// `TaskButton`, because `model.media` already answers whether the player
    /// actually acted: this used to throw that away and every press looked like
    /// it worked, which is the one thing `TaskButton` exists to stop.
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
    ///
    /// MPRIS gives every player a writable `Volume` and a great many ignore it
    /// while still reporting that they accept control — so a per-player slider
    /// works for some of what you play and silently not for the rest. The
    /// machine's always moves something, which is why it wins when it is there.
    private func volumeRow(_ volume: UInt8, machine: Bool) -> some View {
        HStack {
            Image(systemName: "speaker.fill").foregroundStyle(.secondary)
            Slider(
                value: $dragging,
                in: 0...100,
                onEditingChanged: { editing in
                    // Sent on release, not while dragging. A slider dragged
                    // across its range emits a value per frame, and every one of
                    // them here is a round trip to another machine.
                    guard !editing else { return }
                    Task {
                        // Naming the player is what asks for its own volume
                        // rather than the machine's.
                        await model.media(
                            peer, "volume",
                            player: machine ? "" : (player?.id ?? ""),
                            value: Int64(dragging))
                    }
                }
            )
            Image(systemName: "speaker.wave.3.fill").foregroundStyle(.secondary)
            // The number the slider was always hiding. Whole percents is what
            // the wire carries, so this is the value itself and not a
            // rendering of one. Fixed width, or the row twitches as the digits
            // change under a dragging thumb.
            Text("\(Int(dragging.rounded()))%")
                .font(.caption.monospacedDigit())
                .foregroundStyle(.secondary)
                .frame(width: 40, alignment: .trailing)
        }
        // Follow the peer while the user is not holding it. Binding straight to
        // the reported value would fight the drag: every answer that arrived
        // mid-gesture would yank the knob back.
        .onAppear { dragging = Double(volume) }
        .onChange(of: volume) { _, new in dragging = Double(new) }
    }

    /// Everything else that is open, so a command can be aimed somewhere else.
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

    /// Where the timeline is while a finger is on it, `nil` when nobody is
    /// holding it and the peer's own reading should win.
    @State private var scrubbing: Double?

    @Environment(\.scenePhase) private var scenePhase

    /// How long to wait between readings.
    ///
    /// Every number comes from the core, next to the budgets it has to stay
    /// sensible against — the lesson from two independently chosen timeouts
    /// that had to be ordered and were not.
    private var interval: UInt64 {
        guard player?.status == "playing" else { return mediaIdleIntervalMs() }
        // A round trip over Bluetooth costs several fragments each way. The
        // transport exists so the phone still works with Wi-Fi off, not so a
        // progress bar can be smooth.
        return peer.transport == .bleGatt
            ? mediaWatchSlowIntervalMs()
            : mediaWatchIntervalMs()
    }

    /// Restarting the poll whenever any of these changes is what makes the
    /// rate adaptive: `.task(id:)` cancels and begins again, so a track that
    /// starts playing moves to the fast rate without waiting out a slow sleep,
    /// and backgrounding the app stops it entirely.
    private var pollKey: String {
        "\(peer.deviceId)|\(interval)|\(scenePhase == .active)"
    }

    private func clock(_ ms: UInt64) -> String {
        let secs = ms / 1000
        return String(format: "%d:%02d", secs / 60, secs % 60)
    }
}

#endif
