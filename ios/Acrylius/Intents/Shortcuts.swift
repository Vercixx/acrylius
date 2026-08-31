//  Siri/Shortcuts phrases. Split from the intents: those also compile into the
//  widget extension, and only one target may declare an AppShortcutsProvider.

#if canImport(AppIntents)

import AppIntents

struct AcryliusShortcuts: AppShortcutsProvider {
    static var appShortcuts: [AppShortcut] {
        AppShortcut(intent: WakePCIntent(), phrases: [
            "Wake my PC with \(.applicationName)",
            "Wake up my computer with \(.applicationName)",
        ], shortTitle: "Wake PC", systemImageName: "power")

        AppShortcut(intent: LockPCIntent(), phrases: [
            "Lock my PC with \(.applicationName)",
        ], shortTitle: "Lock PC", systemImageName: "lock")

        AppShortcut(intent: UnlockPCIntent(), phrases: [
            "Unlock my PC with \(.applicationName)",
        ], shortTitle: "Unlock PC", systemImageName: "lock.open")
    }
}

#endif
