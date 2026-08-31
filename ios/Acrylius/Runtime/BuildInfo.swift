import Foundation

/// Which build this is, stamped by the CI job that built the app. In `Runtime/`
/// so `swift-test.sh` can compile the parsing on Linux.
public struct BuildInfo: Equatable, Sendable {
    /// `nil` for a build made outside CI; a wrong commit is worse than none.
    public let commit: String?
    public let builtAt: Date?
    public let version: String?

    /// Info.plist keys expanded by Xcode; an unset setting yields an empty
    /// string, not a missing key, so emptiness is what "not from CI" looks like.
    static let commitKey = "ACRBuildCommit"
    static let dateKey = "ACRBuildDate"

    public init(commit: String?, builtAt: Date?, version: String?) {
        self.commit = commit
        self.builtAt = builtAt
        self.version = version
    }

    public static func from(_ info: [String: Any]?) -> BuildInfo {
        BuildInfo(
            commit: trimmed(info?[commitKey]).map { String($0.prefix(12)) },
            builtAt: trimmed(info?[dateKey]).flatMap(parse(iso8601:)),
            version: trimmed(info?["CFBundleShortVersionString"])
        )
    }

    /// Xcode leaves an unset build setting as an empty string, which would read
    /// as a real commit.
    private static func trimmed(_ value: Any?) -> String? {
        guard let s = value as? String else { return nil }
        let t = s.trimmingCharacters(in: .whitespacesAndNewlines)
        return t.isEmpty ? nil : t
    }

    private static func parse(iso8601 s: String) -> Date? {
        let f = ISO8601DateFormatter()
        f.formatOptions = [.withInternetDateTime]
        if let d = f.date(from: s) { return d }
        // Some `date` invocations emit fractional seconds; accept those too.
        f.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return f.date(from: s)
    }

    /// "Development build" rather than blank: a blank row invites a reinstall.
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

    /// Reads as a development build off-device (the Linux test build), correctly.
    public static let current = BuildInfo.from(Bundle.main.infoDictionary)
}
