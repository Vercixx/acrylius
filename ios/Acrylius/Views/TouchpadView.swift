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

    var body: some View {
        TouchSurface(peer: peer, model: model)
            .background(.black)
            .ignoresSafeArea()
            .statusBarHidden()
            .persistentSystemOverlays(.hidden)
            .toolbar(.hidden, for: .navigationBar)
            .overlay(alignment: .topLeading) {
                Button {
                    dismiss()
                } label: {
                    Image(systemName: "xmark.circle.fill")
                        .font(.title2)
                        .foregroundStyle(.white.opacity(0.6))
                }
                .padding()
            }
            .overlay(alignment: .topTrailing) {
                if let ms = model.lastPingRTTms {
                    Text("\(Int(ms)) ms")
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
                    await model.ping(peer)
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
        view.onFrame = context.coordinator.frame
        view.onEnd = context.coordinator.end
        return view
    }

    func updateUIView(_ uiView: TouchpadUIView, context: Context) {}

    func makeCoordinator() -> Coordinator {
        Coordinator(peer: peer, model: model)
    }

    @MainActor
    final class Coordinator {
        private let peer: FfiPeer
        private let model: AppModel

        init(peer: FfiPeer, model: AppModel) {
            self.peer = peer
            self.model = model
        }

        func begin(size: CGSize) {
            let (wMm, hMm) = ScreenDensity.millimeters(for: size)
            Task { await model.touchpadBegin(peer, wMm: wMm, hMm: hMm) }
        }

        func frame(seq: UInt32, ids: [UInt8], xs: [UInt16], ys: [UInt16]) {
            Task { await model.touchpadFrame(peer, seq: seq, ids: Data(ids), xs: xs, ys: ys) }
        }

        func end() {
            Task { await model.touchpadEnd(peer) }
        }
    }
}

/// A bare, multitouch `UIView`, sampled once per display refresh rather
/// than from `touchesMoved`, so a network stall banks at most one frame.
final class TouchpadUIView: UIView {
    var onBegin: ((CGSize) -> Void)?
    var onFrame: ((UInt32, [UInt8], [UInt16], [UInt16]) -> Void)?
    var onEnd: (() -> Void)?

    /// Kernel-style slots: which touch (by identity) owns each small id.
    private var slots: [ObjectIdentifier?] = Array(repeating: nil, count: 10)
    private var points: [ObjectIdentifier: CGPoint] = [:]
    private var displayLink: CADisplayLink?
    private var seq: UInt32 = 0
    private var sentBegin = false
    /// Set once an empty frame has gone out, so idling with no fingers down
    /// does not spam the wire every refresh.
    private var sentEmptyFrame = true

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
        if window != nil {
            let link = CADisplayLink(target: self, selector: #selector(tick))
            link.add(to: .main, forMode: .common)
            displayLink = link
            UIApplication.shared.isIdleTimerDisabled = true
        } else {
            displayLink?.invalidate()
            displayLink = nil
            UIApplication.shared.isIdleTimerDisabled = false
            if sentBegin { onEnd?() }
            reset()
        }
    }

    private func reset() {
        sentBegin = false
        sentEmptyFrame = true
        seq = 0
        points.removeAll()
        slots = Array(repeating: nil, count: slots.count)
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
        for touch in touches {
            guard slot(for: touch) != nil else { continue }
            points[ObjectIdentifier(touch)] = touch.location(in: self)
        }
    }

    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        for touch in touches {
            points[ObjectIdentifier(touch)] = touch.location(in: self)
        }
    }

    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
        release(touches)
    }

    // A cancel (an incoming call, a system gesture taking over) is treated
    // the same as a lift: the next tick's empty frame clears the slot.
    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
        release(touches)
    }

    private func release(_ touches: Set<UITouch>) {
        for touch in touches {
            let id = ObjectIdentifier(touch)
            points[id] = nil
            if let owned = slots.firstIndex(of: id) { slots[owned] = nil }
        }
    }

    @objc private func tick() {
        guard bounds.width > 0, bounds.height > 0 else { return }
        if !sentBegin {
            sentBegin = true
            onBegin?(bounds.size)
        }
        if points.isEmpty {
            guard !sentEmptyFrame else { return }
            sentEmptyFrame = true
            seq += 1
            onFrame?(seq, [], [], [])
            return
        }
        sentEmptyFrame = false

        var ids: [UInt8] = []
        var xs: [UInt16] = []
        var ys: [UInt16] = []
        for (index, owner) in slots.enumerated() {
            guard let owner, let point = points[owner] else { continue }
            ids.append(UInt8(index))
            xs.append(Self.normalize(point.x, extent: bounds.width))
            ys.append(Self.normalize(point.y, extent: bounds.height))
        }
        seq += 1
        onFrame?(seq, ids, xs, ys)
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
        let ppi = ppiByPointWidth[Int(UIScreen.main.bounds.width.rounded())] ?? 160
        let mmPerPoint = UIScreen.main.scale * 25.4 / ppi
        let w = UInt16(clamping: Int((size.width * mmPerPoint).rounded()))
        let h = UInt16(clamping: Int((size.height * mmPerPoint).rounded()))
        return (max(w, 1), max(h, 1))
    }
}

#endif
#endif
