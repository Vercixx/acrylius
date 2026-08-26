//
//  The phrases Siri and the Shortcuts app know about.
//
//  Split from the intents themselves because those are compiled into the widget
//  extension as well, and two targets in one app may not each declare an
//  `AppShortcutsProvider`. The intents are shared; the shortcuts are the app's.
//

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
