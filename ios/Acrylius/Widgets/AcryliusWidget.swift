//
//  The Home Screen widget.
//
//  Renders the snapshot the app left behind (a widget extension has no Local
//  Network permission and gets a sliver of runtime). The one live action is
//  wake, since a magic packet needs no session or reply. Everything else opens the app.
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
    /// Nil when a snapshot was found; a reason, when one wasn't.
    let missing: Missing?

    enum Missing {
        /// The App Group didn't resolve. Distinct from "no data yet" since
        /// waiting won't fix it.
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
        // Ages render with a self-updating relative style; the app requests a
        // reload when facts change, so this hourly entry is only a backstop.
        Timeline(
            entries: [entry(for: configuration)],
            policy: .after(Date().addingTimeInterval(3600)))
    }

    private func entry(for configuration: SelectPCIntent) -> PCEntry {
        guard let snapshot = SnapshotStore.load() else {
            // No file at all: either the app never ran, or it wrote into an
            // unreachable container.
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

/// A lock screen circle: one glyph, no name or timestamp — room for a single fact.
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
        // Not a question mark: unknown is the normal pre-connection state, not a fault.
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

/// When the app last had this machine on the line. Never a live indicator —
/// the app isn't running while this is on screen.
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
        // Different actions per case: opening the app won't fix a missing container.
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
/// Wake only: a magic packet needs no session, identity, or Keychain access.
/// Lock has no control here — it needs a live Noise session, which a control
/// (running with the widget's sliver of runtime) can't hold.
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

/// The first computer that has told this phone how to wake it, not simply
/// the first peer — nil renders as a generic label rather than the control vanishing.
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
        // Controls arrived in iOS 18; the app's floor is 17.
        if #available(iOS 18.0, *) {
            WakeControl()
        }
    }
}

#endif
