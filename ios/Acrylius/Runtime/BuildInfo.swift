import Foundation

/// Which build this is, and when it was made.
///
/// Here because "am I even testing the build I think I am" costs a reinstall to
/// answer otherwise, and a reinstall proves nothing either — SideStore will
/// happily reinstall the same IPA. Every field comes from the CI job that built
/// the app, so a screen can name the commit rather than a person guessing from
/// whether a fix appears to work.
///
/// In `Runtime/` rather than `Views/` deliberately: `swift-test.sh` compiles
/// this directory on Linux and the views only ever reach a macOS runner, so the
/// parsing below is the half that can be tested at all.
public struct BuildInfo: Equatable, Sendable {
    /// The commit the app was built from, abbreviated for reading aloud.
    ///
    /// `nil` for a build made outside CI, which is the honest answer: nothing
    /// local knows what it was built from, and a wrong commit is worse than no
    /// commit.
    public let commit: String?
    public let builtAt: Date?
    /// `CFBundleShortVersionString`, for the version people talk about.
    public let version: String?

    /// Info.plist keys, expanded by Xcode from build settings the CI job passes.
    /// An expansion with nothing behind it yields an empty string, not a missing
    /// key, so emptiness is what "not from CI" looks like.
    static let commitKey = "ACRBuildCommit"
    static let dateKey = "ACRBuildDate"

    public init(commit: String?, builtAt: Date?, version: String?) {
        self.commit = commit
        self.builtAt = builtAt
        self.version = version
    }

    /// Read from a bundle's Info.plist.
    public static func from(_ info: [String: Any]?) -> BuildInfo {
        BuildInfo(
            commit: trimmed(info?[commitKey]).map { String($0.prefix(12)) },
            builtAt: trimmed(info?[dateKey]).flatMap(parse(iso8601:)),
            version: trimmed(info?["CFBundleShortVersionString"])
        )
    }

    /// A non-empty string, or nothing. Xcode leaves an unset build setting as an
    /// empty string in the plist, and an empty commit reads as a real one.
    private static func trimmed(_ value: Any?) -> String? {
        guard let s = value as? String else { return nil }
        let t = s.trimmingCharacters(in: .whitespacesAndNewlines)
        return t.isEmpty ? nil : t
    }

    private static func parse(iso8601 s: String) -> Date? {
        let f = ISO8601DateFormatter()
        f.formatOptions = [.withInternetDateTime]
        if let d = f.date(from: s) { return d }
        // Some `date` invocations emit fractional seconds; accept those too
        // rather than losing the timestamp to a format detail.
        f.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return f.date(from: s)
    }

    /// What this build is, in one line.
    ///
    /// Deliberately says "Development build" rather than showing nothing: a
    /// blank row invites the reinstall this exists to prevent.
    public var summary: String {
        guard let commit else { return "Development build" }
        guard let builtAt else { return commit }
        return "\(commit) · \(BuildInfo.when(builtAt))"
    }

    /// Short, local, and with a year only when it is not this one.
    static func when(_ date: Date, now: Date = Date(), calendar: Calendar = .current) -> String {
        let f = DateFormatter()
        f.locale = .current
        let sameYear = calendar.component(.year, from: date) == calendar.component(.year, from: now)
        f.setLocalizedDateFormatFromTemplate(sameYear ? "dMMM HH:mm" : "dMMMyyyy HH:mm")
        return f.string(from: date)
    }

    /// This app's own build.
    ///
    /// Reads as a development build off-device, where there is no Info.plist to
    /// find — which is what the Linux test build sees, and is correct there.
    public static let current = BuildInfo.from(Bundle.main.infoDictionary)
}
