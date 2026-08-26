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
                        // timestamp, and re-read from the computer every two.
                        // Showing the last reported position alone froze the
                        // clock until you pressed something; advancing it
                        // without ever re-asking would drift and would keep
                        // running after the music stopped somewhere this phone
                        // cannot see. The refresh below is what makes the
                        // estimate safe to draw.
                        TimelineView(.periodic(from: .now, by: 1)) { tick in
                            let at = features.positionMs(at: tick.date)
                            VStack(alignment: .leading, spacing: 4) {
                                ProgressView(
                                    value: Double(at),
                                    total: Double(player.lengthMs)
                                )
                                .tint(.secondary)
                                Text("\(clock(at)) / \(clock(player.lengthMs))")
                                    .font(.caption2.monospacedDigit())
                                    .foregroundStyle(.tertiary)
                            }
                        }
                    }
                }
                .padding(.vertical, 2)

                controls(for: player)

                if let volume = features.media?.systemVolume {
                    volumeRow(volume)
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
            .task(id: peer.deviceId) {
                while !Task.isCancelled {
                    await model.refreshMedia(peer)
                    try? await Task.sleep(for: .seconds(2))
                }
            }
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
    /// ignored is visibly unavailable rather than silently inert.
    private func transport(
        _ symbol: String,
        enabled: Bool,
        size: Font = .title2,
        action: @escaping () async -> Bool
    ) -> some View {
        Button {
            Task { await action() }
        } label: {
            Image(systemName: symbol).font(size)
        }
        .disabled(!enabled)
    }

    /// The computer's output volume, not the player's.
    ///
    /// MPRIS gives every player a writable `Volume` and a great many ignore it
    /// while still reporting that they accept control — so a per-player slider
    /// worked for some of what you play and silently not for the rest. This one
    /// moves the machine, which is what a person means by "turn it down" and is
    /// the one control that works whatever is playing.
    private func volumeRow(_ volume: UInt8) -> some View {
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
                    Task { await model.media(peer, "volume", value: Int64(dragging)) }
                }
            )
            Image(systemName: "speaker.wave.3.fill").foregroundStyle(.secondary)
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

    /// Where the knob is while a finger is on it.
    @State private var dragging: Double = 0

    private func clock(_ ms: UInt64) -> String {
        let secs = ms / 1000
        return String(format: "%d:%02d", secs / 60, secs % 60)
    }
}

#endif
