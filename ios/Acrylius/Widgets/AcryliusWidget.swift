//
//  The Home Screen widget.
//
//  It asks nothing and connects to nothing. A widget extension has no Local
//  Network permission of its own, gets a sliver of runtime, and is drawn at
//  moments the app is not running — and on a free Apple account the daemon
//  cannot reach a phone whose app is closed anyway. So this renders the
//  snapshot the app left behind and is honest about its age.
//
//  The one thing it does do is wake a machine. That needs no session, no
//  identity and no reply: a magic packet is a datagram sent into the void, and
//  everything it takes is a MAC address already on disk. It is also the action
//  most worth having on a Home Screen, because the machine it is aimed at is
//  by definition not running anything that could offer a button.
//
//  Everything else is a tap that opens the app, which is where the permission,
//  the identity and the session live.
//

#if canImport(WidgetKit) && canImport(SwiftUI) && canImport(AppIntents)

import AppIntents
import SwiftUI
import WidgetKit

// MARK: - configuration

/// Which computer this instance of the widget is about.
///
/// Optional, and nil means the first one. Most people pair a single computer
/// and should never have to choose; someone with two can long-press and pick.
struct SelectPCIntent: WidgetConfigurationIntent {
    static var title: LocalizedStringResource { "Choose a PC" }
    static var description: IntentDescription {
        IntentDescription("Pick which computer this widget shows.")
    }

    @Parameter(title: "PC") var pc: PCEntity?
}

// MARK: - timeline

struct PCEntry: TimelineEntry {
    let date: Date
    let peer: PeerSnapshot?
    /// Nil when a snapshot was found. A reason, when one was not.
    let missing: Missing?

    enum Missing {
        /// The App Group did not resolve, so the app's snapshot is somewhere
        /// this process cannot see. Distinguished from "no data yet" because
        /// waiting will not fix it and the wording must not suggest it will.
        case noSharedContainer
        case notPairedYet
    }
}

struct Provider: AppIntentTimelineProvider {
    func placeholder(in context: Context) -> PCEntry {
        PCEntry(
            date: Date(),
            peer: PeerSnapshot(
                deviceId: "", name: "Desktop", platform: "linux",
                lastSeen: Date(), locked: false, canWake: true),
            missing: nil)
    }

    func snapshot(for configuration: SelectPCIntent, in context: Context) async -> PCEntry {
        entry(for: configuration)
    }

    func timeline(for configuration: SelectPCIntent, in context: Context) async -> Timeline<PCEntry> {
        // Ages are drawn with a relative style that updates itself, so a
        // reload is only needed when the facts change — which is when the app
        // runs, and the app asks for one then. The hourly entry is a backstop
        // for the case where it never does.
        Timeline(
            entries: [entry(for: configuration)],
            policy: .after(Date().addingTimeInterval(3600)))
    }

    private func entry(for configuration: SelectPCIntent) -> PCEntry {
        guard let snapshot = SnapshotStore.load() else {
            // No file at all. Either the app has never run, or it ran and wrote
            // into a container this process cannot reach.
            return PCEntry(
                date: Date(), peer: nil,
                missing: SharedContainer.isShared ? .notPairedYet : .noSharedContainer)
        }
        let chosen = configuration.pc.flatMap { wanted in
            snapshot.peers.first { $0.deviceId == wanted.id }
        } ?? snapshot.peers.first
        guard let chosen else {
            return PCEntry(date: Date(), peer: nil, missing: .notPairedYet)
        }
        return PCEntry(date: Date(), peer: chosen, missing: nil)
    }
}

// MARK: - views

struct AcryliusWidgetView: View {
    @Environment(\.widgetFamily) private var family
    let entry: PCEntry

    var body: some View {
        switch entry.peer {
        case let .some(peer):
            content(peer)
                .widgetURL(URL(string: "acrylius://peer/\(peer.deviceId)"))
        case .none:
            EmptyStateView(missing: entry.missing)
        }
    }

    @ViewBuilder
    private func content(_ peer: PeerSnapshot) -> some View {
        switch family {
        case .accessoryCircular:
            CircularView(peer: peer)
        case .accessoryRectangular, .accessoryInline:
            AccessoryView(peer: peer)
        case .systemMedium:
            MediumView(peer: peer)
        default:
            SmallView(peer: peer)
        }
    }
}

/// A lock screen circle, which is one glyph and nothing else.
///
/// No name and no timestamp: at this size there is room for a single fact, so
/// it is the one the machine is actually in — locked, unlocked, or never
/// having said. A tap opens the app, where all three have detail behind them.
private struct CircularView: View {
    let peer: PeerSnapshot

    var body: some View {
        ZStack {
            AccessoryWidgetBackground()
            Image(systemName: symbol)
                .font(.title2)
        }
    }

    private var symbol: String {
        switch peer.locked {
        case .some(true): "lock.fill"
        case .some(false): "lock.open.fill"
        // Never described a session. `desktopcomputer` rather than a question
        // mark: not knowing is the ordinary state before the first connection,
        // not a fault worth drawing as one.
        case nil: "desktopcomputer"
        }
    }
}

/// The name, what its screen is doing, and how long ago that was true.
private struct Heading: View {
    let peer: PeerSnapshot
    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(peer.name).font(.headline).lineLimit(1)
            HStack(spacing: 4) {
                Image(systemName: peer.locked == true ? "lock.fill" : "lock.open")
                Text(peer.locked == true ? "Locked" : "Unlocked")
            }
            .font(.caption)
            .foregroundStyle(.secondary)
            .opacity(peer.locked == nil ? 0 : 1)
            Seen(at: peer.lastSeen)
        }
    }
}

/// When the app last had this machine on the line.
///
/// Never a live indicator. The app is not running while this is on screen, so a
/// dot claiming "connected" would be showing something nobody has checked.
private struct Seen: View {
    let at: Date?
    var body: some View {
        Group {
            if let at {
                Text("Seen ") + Text(at, style: .relative) + Text(" ago")
            } else {
                Text("Open the app to connect")
            }
        }
        .font(.caption2)
        .foregroundStyle(.tertiary)
        .lineLimit(1)
    }
}

private struct SmallView: View {
    let peer: PeerSnapshot
    var body: some View {
        VStack(alignment: .leading) {
            Heading(peer: peer)
            Spacer(minLength: 4)
            if peer.canWake { WakeButton(peer: peer) }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

private struct MediumView: View {
    let peer: PeerSnapshot
    var body: some View {
        HStack(alignment: .top) {
            VStack(alignment: .leading) {
                Heading(peer: peer)
                if let playing = peer.nowPlaying {
                    Label(playing, systemImage: "music.note")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(2)
                        .padding(.top, 4)
                }
                Spacer(minLength: 4)
            }
            Spacer()
            if peer.canWake {
                WakeButton(peer: peer)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

private struct AccessoryView: View {
    let peer: PeerSnapshot
    var body: some View {
        ViewThatFits {
            HStack(spacing: 4) {
                Image(systemName: peer.locked == true ? "lock.fill" : "lock.open")
                Text(peer.name).lineLimit(1)
            }
            Text(peer.name).lineLimit(1)
        }
    }
}

/// The only button here that does its own work.
///
/// If a widget extension turns out to need a Local Network permission the app
/// was granted separately, this is where that shows up — as a wake that never
/// lands. The intent says so rather than reporting success it cannot know.
private struct WakeButton: View {
    let peer: PeerSnapshot
    var body: some View {
        Button(intent: WakePCIntent(pc: PCEntity(id: peer.deviceId, name: peer.name))) {
            Label("Wake", systemImage: "power")
                .font(.caption.weight(.semibold))
        }
        .buttonStyle(.bordered)
    }
}

private struct EmptyStateView: View {
    let missing: PCEntry.Missing?
    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Image(systemName: "desktopcomputer.trianglebadge.exclamationmark")
                .foregroundStyle(.secondary)
            Text(title).font(.caption.weight(.semibold))
            Text(detail).font(.caption2).foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private var title: String {
        missing == .noSharedContainer ? "Not shared" : "No PC yet"
    }

    private var detail: String {
        // The two cases need different words because they need different
        // actions. Opening the app will not fix a container that does not
        // exist, and telling someone it will is worse than saying nothing.
        missing == .noSharedContainer
            ? "This build cannot share data with the app."
            : "Pair a computer in Acrylius."
    }
}

// MARK: - the widget

struct StatusWidget: Widget {
    var body: some WidgetConfiguration {
        AppIntentConfiguration(
            kind: "org.acrylius.widget.status",
            intent: SelectPCIntent.self,
            provider: Provider()
        ) { entry in
            AcryliusWidgetView(entry: entry)
                .containerBackground(.fill.tertiary, for: .widget)
        }
        .configurationDisplayName("PC")
        .description("What your computer was doing, and a way to wake it.")
        .supportedFamilies([
            .systemSmall, .systemMedium,
            .accessoryCircular, .accessoryRectangular, .accessoryInline,
        ])
    }
}

// MARK: - a control

/// Wake, from Control Centre or the lock screen's own buttons.
///
/// Wake and only wake. It is the one thing this app does that a separate
/// process can genuinely finish: a magic packet needs no session, no identity
/// and no Keychain, and everything it takes is already on disk — which is
/// exactly why the Home Screen widget has been allowed to send one since M2.
///
/// Lock deliberately has no control. It needs a live Noise session, and a
/// control runs where the widget runs: no Local Network permission of its own
/// and a sliver of runtime. A control that silently does nothing is worse than
/// no control, and the honest alternative — opening the app to do it — is not
/// a control, it is a shortcut to the app with extra steps.
@available(iOS 18.0, *)
struct WakeControl: ControlWidget {
    var body: some ControlWidgetConfiguration {
        StaticControlConfiguration(
            kind: "org.acrylius.control.wake",
            provider: WakeTargetProvider()
        ) { peer in
            ControlWidgetButton(
                action: WakePCIntent(
                    pc: PCEntity(id: peer?.deviceId ?? "", name: peer?.name ?? "PC")
                )
            ) {
                Label(peer?.name ?? "Wake PC", systemImage: "power")
            }
        }
        .displayName("Wake PC")
        .description("Send a wake-up packet without unlocking your phone.")
    }
}

/// The first computer that has told this phone how to wake it.
///
/// Not simply the first peer: a machine with no wake target on file produces a
/// button that can only ever apologise. Nil when there is none, which the
/// button renders as a generic label rather than refusing to exist — a control
/// that vanishes from the gallery is harder to explain than one that says it
/// has nothing to aim at yet.
@available(iOS 18.0, *)
struct WakeTargetProvider: ControlValueProvider {
    var previewValue: PeerSnapshot? { nil }

    func currentValue() async throws -> PeerSnapshot? {
        SnapshotStore.load()?.peers.first { $0.canWake }
    }
}

@main
struct AcryliusWidgets: WidgetBundle {
    var body: some Widget {
        StatusWidget()
        // Controls arrived in iOS 18 and the app's floor is 17.
        if #available(iOS 18.0, *) {
            WakeControl()
        }
    }
}

#endif
