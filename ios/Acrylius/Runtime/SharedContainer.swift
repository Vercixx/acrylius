//
//  The one directory the app and its widget can both see.
//
//  A widget is a separate process with a separate container. Nothing the app
//  writes to its own container is visible there, so anything the widget renders
//  has to live in an App Group.
//
//  Which a free Apple account may not be able to register. That is not settled
//  here and cannot be settled from Linux, so this resolves the group at runtime
//  and falls back to the app's own container when there is none. The app then
//  works exactly as before and the widget shows that it has nothing, instead of
//  the whole thing failing to build or launching to a blank rectangle with no
//  explanation. `isShared` is what tells them apart, and the app surfaces it.
//

import Foundation

public enum SharedContainer {
    /// Must match `com.apple.security.application-groups` in both entitlements
    /// files. A mismatch is invisible at build time and shows up as a widget
    /// that never has any data.
    public static let group = "group.org.acrylius"

    /// True when the App Group resolved. False means every process is reading
    /// and writing its own container and the widget will find nothing.
    public static var isShared: Bool { groupURL != nil }

    private static var groupURL: URL? {
        #if canImport(Darwin)
        FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: group)
        #else
        nil
        #endif
    }

    /// Where shared state goes. The App Group container when there is one, this
    /// process's own Application Support when there is not.
    public static var base: URL? {
        if let groupURL {
            // An App Group container has no Application Support of its own, so
            // one is made rather than scattering files at its root.
            let dir = groupURL.appendingPathComponent("Acrylius", isDirectory: true)
            try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
            return dir
        }
        return try? FileManager.default.url(
            for: .applicationSupportDirectory, in: .userDomainMask,
            appropriateFor: nil, create: true
        )
    }

    /// A subdirectory of the shared base, created if it is not there.
    public static func directory(_ name: String) -> URL? {
        guard let base else { return nil }
        let dir = base.appendingPathComponent(name, isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }
}
