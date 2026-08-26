//
//  What this phone can actually carry out.
//
//  A plugin whose effects are missing still loads and can still send — being
//  unable to serve a capability says nothing about being able to use one — so
//  the list here is short and that is fine. A phone has no desktop session to
//  lock and runs nothing on request, but it can ask a computer to do both.
//

#if canImport(UIKit)

import Foundation
import UIKit

public final class IosEffector: Effector, @unchecked Sendable {
    public init() {}

    /// What to hand `AcryliusCore` at construction.
    ///
    /// Not `.wol`, even though the magic packet case below is implemented and
    /// correct. Serving that capability means relaying a wake for a *third*
    /// machine on someone else's say-so, and the phone registers the plugin
    /// with an empty allowlist, so every such request is refused before it gets
    /// here. Declaring it made "This device" report Wake as "Send and receive",
    /// which read as a promise the phone had no intention of keeping.
    ///
    /// Waking a paired computer is unaffected: that is the phone sending a
    /// datagram of its own accord, and needs no capability from anyone.
    public static let kinds: [FfiEffectKind] = [.clipboard]

    public func run(_ effect: FfiEffect) async -> FfiEffectResult {
        switch effect {
        case let .clipboardWrite(_, data):
            guard let text = String(data: data, encoding: .utf8) else {
                return .failed(detail: "not UTF-8 text")
            }
            await MainActor.run { UIPasteboard.general.string = text }
            return .ok(data: Data())

        case .clipboardRead:
            // Reading raises the system "Allow Paste?" alert for anything
            // another app put there, which is why nothing here reads the
            // pasteboard on its own. This runs only when a computer explicitly
            // asks, so the prompt lines up with something the user just did.
            let text = await MainActor.run { UIPasteboard.general.string }
            guard let text else { return .failed(detail: "the pasteboard holds no text") }
            return .ok(data: Data(text.utf8))

        case let .sendMagicPacket(macs, dests, port):
            // Unicast first, and not as a fallback: a network interface matches
            // a magic packet by its payload and ignores the destination
            // address, so a datagram aimed at the machine's last known address
            // wakes it exactly as well as a broadcast — and iOS gates broadcast
            // behind an entitlement a free developer account cannot get.
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
