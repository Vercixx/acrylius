//
//  The one directory the app and its widget can both see, since a widget's
//  own container is separate. A sideloader can rewrite bundle/group
//  identifiers at re-signing, so the group is discovered from the signed
//  profile rather than assumed; falls back to the app's own container if
//  none is granted.
//

import Foundation

public enum SharedContainer {
    /// The group as built; must match `com.apple.security.application-groups`
    /// in both entitlement files. What a build is actually signed with may differ — see above.
    public static let configuredGroup = "group.org.acrylius"

    /// The group this build actually holds, or nil. Resolved once — parsing
    /// the provisioning profile isn't cheap enough to do repeatedly from the widget's timeline provider.
    public static let group: String? = {
        #if canImport(Darwin)
        for candidate in Entitlements.appGroups() + [configuredGroup]
        where FileManager.default
            .containerURL(forSecurityApplicationGroupIdentifier: candidate) != nil {
            return candidate
        }
        return nil
        #else
        return nil
        #endif
    }()

    /// True when a shared container exists. False means every process is
    /// reading and writing its own and the widget will find nothing.
    public static var isShared: Bool { group != nil }

    private static var groupURL: URL? {
        #if canImport(Darwin)
        group.flatMap {
            FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: $0)
        }
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

    /// What to show someone whose widget is empty: distinguishes a rewritten
    /// group that was found, one that was lost, and an unsigned build.
    public static func diagnosis() -> String {
        #if canImport(Darwin)
        if let group {
            return group == configuredGroup ? group : "\(group) (rewritten at signing)"
        }
        if !Entitlements.hasProfile() {
            return "unsigned build"
        }
        let keys = Entitlements.keys()
        if keys.isEmpty {
            return "profile unreadable"
        }
        return "no app group granted"
        #else
        return "not applicable"
        #endif
    }

    /// Everything worth knowing when the widget stops working. A sideloader
    /// can give the app/extension one App ID or two, and each fails differently.
    public static func report() -> [(String, String)] {
        #if canImport(Darwin)
        var rows = [
            ("Bundle", Bundle.main.bundleIdentifier ?? "unknown"),
            ("App group", diagnosis()),
        ]
        let granted = Entitlements.appGroups()
        if granted.count > 1 {
            // More than one means the installer added its own alongside ours,
            // and which is picked matters.
            rows.append(("Granted", granted.joined(separator: ", ")))
        }
        if let team = Entitlements.teamIdentifier() {
            rows.append(("Team", team))
        }
        return rows
        #else
        return [("App group", "not applicable")]
        #endif
    }
}

#if canImport(Darwin)

/// What this bundle was actually signed with. iOS has no public API for
/// reading entitlements directly, so this parses the signed provisioning
/// profile's CMS blob instead.
enum Entitlements {
    static func appGroups(in bundle: Bundle = .main) -> [String] {
        read(in: bundle)?["com.apple.security.application-groups"] as? [String] ?? []
    }

    /// Distinguishes "no groups in the profile" from "no profile read at all",
    /// which is the one question an empty group list cannot answer on its own.
    static func keys(in bundle: Bundle = .main) -> [String] {
        read(in: bundle).map { $0.keys.sorted() } ?? []
    }

    static func hasProfile(in bundle: Bundle = .main) -> Bool {
        bundle.url(forResource: "embedded", withExtension: "mobileprovision") != nil
    }

    /// Whose team this build was signed under — the prefix a sideloader
    /// appends to bundle identifiers and app groups.
    static func teamIdentifier(in bundle: Bundle = .main) -> String? {
        read(in: bundle)?["com.apple.developer.team-identifier"] as? String
    }

    private static func read(in bundle: Bundle) -> [String: Any]? {
        guard let url = bundle.url(forResource: "embedded", withExtension: "mobileprovision"),
              let data = try? Data(contentsOf: url),
              let plist = carvePlist(from: data),
              let profile = try? PropertyListSerialization.propertyList(
                  from: plist, options: [], format: nil) as? [String: Any]
        else { return nil }
        return profile["Entitlements"] as? [String: Any]
    }

    /// Cut the XML plist out of the CMS envelope by its markers rather than
    /// parsing PKCS#7. Doesn't verify the signature — iOS already did.
    private static func carvePlist(from data: Data) -> Data? {
        guard let start = data.range(of: Data("<?xml".utf8)),
              let end = data.range(of: Data("</plist>".utf8),
                                   options: .backwards,
                                   in: start.lowerBound..<data.endIndex)
        else { return nil }
        return data.subdata(in: start.lowerBound..<end.upperBound)
    }
}

#endif
