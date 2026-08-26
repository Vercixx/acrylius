//
//  The one directory the app and its widget can both see.
//
//  A widget is a separate process with a separate container. Nothing the app
//  writes to its own container is visible there, so anything the widget renders
//  has to live in an App Group.
//
//  The identifier compiled into an app is not necessarily the one it runs
//  under. Sideloading tools re-sign with their own team and rewrite bundle
//  identifiers to keep them unique — SideStore appends the team ID — and App
//  Groups are rewritten to match, because a group can only be registered under
//  the team that owns it. A hardcoded `group.org.acrylius` then names a
//  container that does not exist, and does so in silence.
//
//  So the group is discovered, not assumed: read out of the profile the bundle
//  was signed with. The literal below is only the fallback for a build signed
//  the ordinary way.
//
//  When nothing is granted at all, this falls back to the process's own
//  container. The app then works exactly as before and the widget says it has
//  nothing, instead of writes vanishing with no error anywhere. `isShared` is
//  what tells those apart, and "This device" surfaces it.
//

import Foundation

public enum SharedContainer {
    /// The group as built. Must match `com.apple.security.application-groups`
    /// in both entitlements files — but see above: what a bundle is signed with
    /// may not be this, so nothing reads it directly.
    public static let configuredGroup = "group.org.acrylius"

    /// The group this build actually holds, or nil.
    ///
    /// Resolved once. Reading a provisioning profile means parsing a signed
    /// blob off disk, and the widget's timeline provider is not somewhere to do
    /// that repeatedly.
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

    /// What to show someone whose widget is empty.
    ///
    /// Three outcomes that look identical from the outside and need different
    /// answers: the entitlement was rewritten and found, rewritten and lost, or
    /// never there because nothing signed this build.
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

    /// Everything worth knowing when the widget stops working.
    ///
    /// A sideloading tool decides at install time whether the app and its
    /// extension get one App ID or two, and rewrites identifiers either way.
    /// Both arrangements can work and they fail differently, so what matters is
    /// being able to see which one happened rather than reasoning about what
    /// the installer probably did.
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

/// What this bundle was actually signed with.
///
/// iOS offers no public API for reading your own entitlements —
/// `SecTaskCopyValueForEntitlement` is macOS only — but every signed bundle
/// carries the profile it was signed with, and the profile is a CMS blob with
/// an XML plist inside it.
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

    /// Whose team this build was signed under.
    ///
    /// The prefix a sideloading tool appends to bundle identifiers and app
    /// groups, so seeing it is how a rewritten identifier stops being a mystery.
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

    /// Cut the XML plist out of the CMS envelope by its markers rather than by
    /// parsing PKCS#7. Nothing here verifies the signature — iOS already did,
    /// or this would not be running — it only reads.
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
