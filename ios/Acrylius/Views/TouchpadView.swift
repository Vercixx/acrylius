#if canImport(SwiftUI)

import SwiftUI
#if canImport(UIKit)
import UIKit
#endif

#if canImport(UIKit)

/// Full-screen touch surface for one peer. Gestures are libinput's and the
/// compositor's job, derived from raw finger positions, not this code's.
struct TouchpadView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    let peer: FfiPeer

    /// Looked up live, not from `peer`: that is a snapshot from when this
    /// screen opened, and a takeover mid-session must still show here.
    private var liveTransport: FfiTransportKind? {
        model.peers.first { $0.deviceId == peer.deviceId }?.transport
    }

    private var overUSB: Bool { model.usbHost == peer.deviceId }

    @State private var holdingClose = false
    private static let holdToClose = 0.6

    var body: some View {
        TouchSurface(peer: peer, model: model)
            .background(.black)
            .ignoresSafeArea()
            .statusBarHidden()
            .persistentSystemOverlays(.hidden)
            .overlay(alignment: .topLeading) {
                Image(systemName: "xmark.circle.fill")
                    .font(.title2)
                    .foregroundStyle(.white.opacity(holdingClose ? 1 : 0.6))
                    .scaleEffect(holdingClose ? 1.5 : 1)
                    .animation(.easeIn(duration: Self.holdToClose), value: holdingClose)
                    .padding()
                    .contentShape(Rectangle())
                    .onLongPressGesture(minimumDuration: Self.holdToClose) {
                        dismiss()
                    } onPressingChanged: { holdingClose = $0 }
                    .accessibilityLabel("Close touchpad")
                    .accessibilityHint("Touch and hold to close.")
                    .accessibilityAddTraits(.isButton)
                    .accessibilityAction { dismiss() }
            }
            .onAppear { AppOrientation.allow(.allButUpsideDown) }
            .onDisappear { AppOrientation.allow(.portrait) }
            .overlay(alignment: .topTrailing) {
                if overUSB || model.lastPingRTTms != nil || liveTransport != nil {
                    HStack(spacing: 6) {
                        if overUSB {
                            Text("USB")
                        } else {
                            if let over = carrying(liveTransport) {
                                Text(over)
                            }
                            if let ms = model.lastPingRTTms {
                                Text("\(Int(ms)) ms")
                            }
                        }
                    }
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(.white.opacity(0.6))
                    .padding(.horizontal, 8)
                    .padding(.vertical, 4)
                    .background(.black.opacity(0.3), in: Capsule())
                    .padding()
                }
            }
            .task {
                while !Task.isCancelled {
                    if !overUSB { await model.ping(peer) }
                    try? await Task.sleep(for: .seconds(2))
                }
            }
    }
}

private struct TouchSurface: UIViewRepresentable {
    let peer: FfiPeer
    let model: AppModel

    func makeUIView(context: Context) -> TouchpadUIView {
        let view = TouchpadUIView()
        view.onBegin = context.coordinator.begin
        view.onFrames = context.coordinator.frames
        view.onEnd = context.coordinator.end
        return view
    }

    func updateUIView(_ uiView: TouchpadUIView, context: Context) {}

    func makeCoordinator() -> Coordinator {
        Coordinator(peer: peer, model: model)
    }

    @MainActor
    final class Coordinator {
        private enum Out {
            case begin(wMm: UInt16, hMm: UInt16)
            case frame(TouchpadUIView.Frame)
            case end
        }

        private let peer: FfiPeer
        private let model: AppModel

        /// Sent strictly in order, so a `begin` never trails the frames after it.
        private var queue: [Out] = []
        private var sending = false
        private static let maxQueued = 120

        init(peer: FfiPeer, model: AppModel) {
            self.peer = peer
            self.model = model
        }

        func begin(size: CGSize) {
            let (wMm, hMm) = ScreenDensity.millimeters(for: size)
            queue.append(.begin(wMm: wMm, hMm: hMm))
            pump()
        }

        /// Every sample over USB, where the desktop paces them. Elsewhere only
        /// the newest, replacing a stale one still queued.
        func frames(_ batch: [TouchpadUIView.Frame]) {
            if model.usbHost == peer.deviceId {
                queue += batch.map(Out.frame)
                if queue.count > Self.maxQueued {
                    queue.removeFirst(queue.count - Self.maxQueued)
                }
            } else if let newest = batch.last {
                if case .frame = queue.last { queue.removeLast() }
                queue.append(.frame(newest))
            }
            pump()
        }

        func end() {
            queue.removeAll {
                switch $0 {
                case .frame: true
                default: false
                }
            }
            queue.append(.end)
            pump()
        }

        private func pump() {
            guard !sending, !queue.isEmpty else { return }
            sending = true
            let next = queue.removeFirst()
            Task {
                switch next {
                case let .begin(wMm, hMm):
                    await model.touchpadBegin(peer, wMm: wMm, hMm: hMm)
                case let .frame(f):
                    await model.touchpadFrame(
                        peer, seq: f.seq, ids: Data(f.ids), xs: f.xs, ys: f.ys, tUs: f.tUs
                    )
                case .end:
                    await model.touchpadEnd(peer)
                }
                sending = false
                pump()
            }
        }
    }
}

/// A bare, multitouch `UIView` that reports every digitizer sample iOS
/// coalesces into a touch event, stamped with when it was taken.
final class TouchpadUIView: UIView {
    struct Frame {
        let seq: UInt32
        let ids: [UInt8]
        let xs: [UInt16]
        let ys: [UInt16]
        let tUs: UInt32
    }

    var onBegin: ((CGSize) -> Void)?
    var onFrames: (([Frame]) -> Void)?
    var onEnd: (() -> Void)?

    /// Kernel-style slots: which touch (by identity) owns each small id.
    private var slots: [ObjectIdentifier?] = Array(repeating: nil, count: 10)
    private var points: [ObjectIdentifier: CGPoint] = [:]
    private var seq: UInt32 = 0
    /// Resent at a gesture's start once this stale, so a silent reconnect heals.
    private var lastBeginAt: Date?
    private static let beginInterval: TimeInterval = 3
    /// A finger held still raises no events, so its frame is repeated to keep
    /// the desktop's 300 ms stall guard from lifting it.
    private var heartbeat: Timer?
    private var lastSentAt: TimeInterval = 0
    private static let heartbeatInterval: TimeInterval = 0.1
    private var laidOutSize: CGSize = .zero

    override init(frame: CGRect) {
        super.init(frame: frame)
        isMultipleTouchEnabled = true
        backgroundColor = .black
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("not used from a storyboard")
    }

    override func didMoveToWindow() {
        super.didMoveToWindow()
        UIApplication.shared.isIdleTimerDisabled = window != nil
        heartbeat?.invalidate()
        heartbeat = nil
        if window != nil {
            let timer = Timer(
                timeInterval: Self.heartbeatInterval / 2, target: self,
                selector: #selector(beat), userInfo: nil, repeats: true
            )
            RunLoop.main.add(timer, forMode: .common)
            heartbeat = timer
        } else {
            if lastBeginAt != nil { onEnd?() }
            reset()
        }
    }

    /// Opening and rotating both change the size; with no finger down the new
    /// one goes out at once, otherwise at the next gesture.
    override func layoutSubviews() {
        super.layoutSubviews()
        guard bounds.size != laidOutSize else { return }
        laidOutSize = bounds.size
        if lastBeginAt != nil { lastBeginAt = .distantPast }
        if points.isEmpty { beginIfStale() }
    }

    private func reset() {
        lastBeginAt = nil
        seq = 0
        points.removeAll()
        slots = Array(repeating: nil, count: slots.count)
    }

    private func beginIfStale() {
        guard bounds.width > 0, bounds.height > 0 else { return }
        let now = Date()
        if let last = lastBeginAt, now.timeIntervalSince(last) < Self.beginInterval { return }
        lastBeginAt = now
        onBegin?(bounds.size)
    }

    private func slot(for touch: UITouch) -> UInt8? {
        let id = ObjectIdentifier(touch)
        if let existing = slots.firstIndex(of: id) {
            return UInt8(existing)
        }
        guard let free = slots.firstIndex(of: nil) else { return nil }
        slots[free] = id
        return UInt8(free)
    }

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        if points.isEmpty { beginIfStale() }
        for touch in touches { _ = slot(for: touch) }
        report(event)
    }

    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        report(event)
    }

    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
        lift(touches, event)
    }

    // A cancel (an incoming call, a system gesture taking over) is a lift.
    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
        lift(touches, event)
    }

    private func lift(_ touches: Set<UITouch>, _ event: UIEvent?) {
        report(event)
        for touch in touches {
            let id = ObjectIdentifier(touch)
            points[id] = nil
            if let owned = slots.firstIndex(of: id) { slots[owned] = nil }
        }
        send([frame(at: event?.timestamp ?? CACurrentMediaTime())])
    }

    /// One frame per sample, oldest first. Fingers sampled together share a
    /// timestamp, so they share a frame.
    private func report(_ event: UIEvent?) {
        guard let event, bounds.width > 0, bounds.height > 0 else { return }
        var byTime: [TimeInterval: [(ObjectIdentifier, CGPoint)]] = [:]
        for touch in event.allTouches ?? [] {
            let id = ObjectIdentifier(touch)
            guard slots.contains(id) else { continue }
            for sample in event.coalescedTouches(for: touch) ?? [touch] {
                byTime[sample.timestamp, default: []].append((id, sample.location(in: self)))
            }
        }
        let frames = byTime.sorted { $0.key < $1.key }.map { time, samples in
            for (id, point) in samples { points[id] = point }
            return frame(at: time)
        }
        send(frames)
    }

    @objc private func beat() {
        let now = CACurrentMediaTime()
        guard !points.isEmpty, now - lastSentAt >= Self.heartbeatInterval else { return }
        send([frame(at: now)])
    }

    private func send(_ frames: [Frame]) {
        guard !frames.isEmpty else { return }
        lastSentAt = CACurrentMediaTime()
        onFrames?(frames)
    }

    /// `time` is seconds since boot, as `UITouch.timestamp` and
    /// `CACurrentMediaTime` both count it.
    private func frame(at time: TimeInterval) -> Frame {
        var ids: [UInt8] = []
        var xs: [UInt16] = []
        var ys: [UInt16] = []
        for (index, owner) in slots.enumerated() {
            guard let owner, let point = points[owner] else { continue }
            ids.append(UInt8(index))
            xs.append(Self.normalize(point.x, extent: bounds.width))
            ys.append(Self.normalize(point.y, extent: bounds.height))
        }
        seq &+= 1
        let tUs = UInt32(truncatingIfNeeded: Int64(time * 1_000_000))
        return Frame(seq: seq, ids: ids, xs: xs, ys: ys, tUs: tUs)
    }

    private static func normalize(_ value: CGFloat, extent: CGFloat) -> UInt16 {
        let unit = max(0, min(value / extent, 1))
        return UInt16(unit * CGFloat(UInt16.max))
    }
}

private enum ScreenDensity {
    /// Point-width to pixels per inch. An unlisted width falls back to a low
    /// estimate, which assumes a larger surface and gentler thresholds.
    static let ppiByPointWidth: [Int: Double] = [
        375: 326, 390: 460, 393: 460, 402: 460,
        428: 458, 430: 460, 440: 460,
    ]

    static func millimeters(for size: CGSize) -> (w: UInt16, h: UInt16) {
        let screen = UIScreen.main.bounds.size
        let ppi = ppiByPointWidth[Int(min(screen.width, screen.height).rounded())] ?? 160
        let mmPerPoint = UIScreen.main.scale * 25.4 / ppi
        let w = UInt16(clamping: Int((size.width * mmPerPoint).rounded()))
        let h = UInt16(clamping: Int((size.height * mmPerPoint).rounded()))
        return (max(w, 1), max(h, 1))
    }
}

/// What `AppDelegate` reports on a phone: portrait, except on the touchpad.
@MainActor
enum AppOrientation {
    static var allowed: UIInterfaceOrientationMask = .portrait

    static func allow(_ mask: UIInterfaceOrientationMask) {
        allowed = mask
        guard UIDevice.current.userInterfaceIdiom == .phone,
              let scene = UIApplication.shared.connectedScenes.first as? UIWindowScene
        else { return }
        var top = scene.keyWindow?.rootViewController
        while let next = top?.presentedViewController { top = next }
        top?.setNeedsUpdateOfSupportedInterfaceOrientations()
        scene.requestGeometryUpdate(.iOS(interfaceOrientations: mask))
    }
}

#endif
#endif
