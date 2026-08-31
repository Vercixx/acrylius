//
//  What this phone can actually carry out. A plugin whose effects are missing
//  still loads and can still send capabilities out; it just can't serve them
//  in. A phone has no desktop session to lock and runs nothing on request.
//

#if canImport(UIKit)

import Foundation
import UIKit

public final class IosEffector: Effector, @unchecked Sendable {
    public init() {}

    /// What to hand `AcryliusCore` at construction.
    ///
    /// `.wol` is deliberately excluded: serving it means relaying a wake for a
    /// third machine, which the phone's empty allowlist always refuses anyway.
    /// Nothing here works while the app isn't open.
    public static let kinds: [FfiEffectKind] = [.clipboard, .share]

    public func run(_ effect: FfiEffect) async -> FfiEffectResult {
        switch effect {
        case let .clipboardWrite(_, data):
            guard let text = String(data: data, encoding: .utf8) else {
                return .failed(detail: "not UTF-8 text")
            }
            await MainActor.run { UIPasteboard.general.string = text }
            return .ok(data: Data())

        case .clipboardRead:
            // Reading raises the system "Allow Paste?" alert, so this never
            // runs on its own — only when a computer explicitly asks for it.
            let text = await MainActor.run { UIPasteboard.general.string }
            guard let text else { return .failed(detail: "the pasteboard holds no text") }
            return .ok(data: Data(text.utf8))

        case let .sendMagicPacket(macs, dests, port):
            // Unicast, not a fallback: a NIC matches a magic packet by payload
            // regardless of destination, and iOS gates broadcast behind an
            // entitlement a free account can't get.
            var sent = false
            for mac in macs {
                guard let packet = try? magicPacket(mac: mac) else { continue }
                if await MagicPacketSender.send(packet, to: dests, port: port) {
                    sent = true
                }
            }
            return sent ? .ok(data: Data()) : .failed(detail: "nothing could be sent")

        default:
            // Session and command effects. Answering `unsupported` rather than
            // failing says this is a property of the device, not a bad moment.
            return .unsupported
        }
    }
}

#endif
