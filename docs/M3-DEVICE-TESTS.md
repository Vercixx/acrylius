# M3 — what a machine cannot check

## Re-test after the first pass

Fixed from the 2026-08-28 report. Everything here was found on a device and
none of it was caught by any gate.

- [ ] **Pairing connects immediately.** No force-quit. *(`confirm_pairing` left
      it to the peer to open a session, which is only ever true between two
      computers — a phone always dials and is never dialled, so nobody did.)*
- [ ] **It reconnects on its own** after backgrounding the app, after the
      computer sleeps and wakes, and after Wi-Fi comes back — within ~10s and
      with no force-quit. *(Auto-connect fired on a sighting, and mDNS resolves
      once and then says nothing, so anything that ended a session without a
      fresh advertisement was permanent.)*
- [ ] **A sleeping computer stops reading as connected** within ~20s.
      *(The phone opened its sockets with plain defaults while the desktop has
      bounded this since M2; the phone held an ESTABLISHED socket forever.)*
- [ ] **The timeline can be dragged** and the track actually moves.
      *(The slider lived inside a `TimelineView` that rebuilt it every second,
      cancelling the drag before it ended; and its binding read a value
      captured once per rebuild, so it answered the same number however far the
      knob went.)*
- [ ] **The clock is not a second behind** — it advances four times a second.
- [ ] **Swipe a device, tap Forget: the dialog stays** until answered, and
      answering it actually forgets. *(Present since M1. The dialog was
      attached to the row the swipe was removing, so it went with it.)*
- [ ] **Errors are a normal iOS alert**, not a floating glass capsule.
- [ ] **Bluetooth takes over when Wi-Fi goes off**, within about six seconds
      and with no force-quit. *(A dial with no viable path is not failed by
      Network.framework, it is waited on — so the Wi-Fi dial hung, and the
      route walk it was holding never reached Bluetooth. Nothing could rescue
      it: an automatic retry stands down while a dial is outstanding.)*
- [ ] **"Connecting…" while it is connecting.** A dial in flight used to read
      as "Not connected", which beside a state called Connecting says *gave up*.
      "Not connected" now means every route has been tried.
- [ ] **A file arriving says it arrived** — "Saved <name>", not "Sent the
      file". *(Present since M1. The ending was looked up in the table of
      things this phone had offered, and a file coming the other way is not in
      it, so every arrival read as a send of an unnamed file.)*
- [ ] **Seeking to 00:00 works.** Fixed on the desktop, so it needs
      `./scripts/install.sh` — a phone build alone will not carry it.
- [ ] **Wi-Fi coming back moves the session off Bluetooth**, without a
      force-quit. *(Losing Wi-Fi announces itself; regaining it announces
      nothing, because the peer never stopped being reachable — it was just on
      a radio that cannot carry a file. And the Bonjour browse was created once
      and never replaced, so a browse that failed during the outage reported
      nothing ever again.)*
- [ ] **A computer switched off leaves "On this network"** within a few
      seconds, rather than staying on offer until the app is restarted.
      *(Sightings were one-way: nothing was ever un-discovered.)*
- [ ] **Actions can be felt.** A control that lands and one that is refused
      buzz differently; a seek buzzes when the finger lifts. Nothing buzzes on
      the press itself.

Still open, and not fixed in this build — see the end of this file: the widget
extension, and BLE.


Everything in this list is here because no gate reaches it. `cargo test`,
`clippy`, `swift-test.sh`, `xcodegen-check.sh`, the M0/M1 acceptance runs and
the macOS build are all green; none of them has a screen, a camera, a radio, or
a person.

Two things shape the list. **No local gate compiles `ios/Acrylius/Views`** —
`swift-test.sh` builds `Runtime/*.swift` and nothing else, so every view below
was first type-checked by the macOS runner and has never been *run*. And the
**loopback suite proves negotiation, not bytes**: it moves no data over a real
socket, so anything about what actually travels is a phone-and-desktop question.

Ship both ends together. `pair` in the discovery TXT record is new, and a
desktop that advertises it needs a phone that reads it.

---

## Before anything

```
./scripts/install.sh                     # the daemon under test
gh run download --name acrylius-unsigned-ipa   # or ./scripts/send-latest-ipa.sh
journalctl --user -u acryliusd -f        # keep this open
```

The daemon logs at `info`. Anything below that is invisible in a report, which
has caused wrong diagnoses before — if something looks silent, check the filter
before concluding the code did not run.

---

## 1. The toolchain (Stage A)

- [ ] The app has an icon on the Home Screen, not a white square.
- [ ] The icon's corners are rounded by iOS and not black. *(A black corner
      means the PNG regained an alpha channel; `xcodegen-check.sh` refuses that,
      so this is really a check that the installed build is the one CI made.)*
- [ ] Under iOS 26: navigation bars, the tab bar and sheets look like Liquid
      Glass rather than flat iOS 17 chrome.
- [ ] Under iOS 17 or 18, if you have such a device: the app still launches and
      nothing is invisible or unreadable. **Untested by anyone so far** — the
      `#available` fallback path has only ever been compiled. *(There is no iOS
      19–25; Apple went from 18 to 26. The floor is 17 and the guard is
      `#available(iOS 26, *)`, so the fallback covers exactly 17 and 18.)*

## 2. The shape of the app (Stage B)

- [ ] Three tabs: Devices, Files, Status.
- [ ] Scrolling a long list minimises the tab bar (iOS 26 only).
- [ ] **The Bluetooth prompt still appears on a fresh install.** Delete the app,
      reinstall, and confirm Devices offers "Turn on Bluetooth" and that tapping
      it raises the system prompt. *This is the highest-value item in this
      section:* the prompt used to live behind the Bluetooth diagnostics screen,
      which is now three taps deep in Status › Debug. Getting this wrong means a
      phone that silently stops working whenever Wi-Fi does.
- [ ] Status shows no device ids or entitlement rows; those are behind Debug.
- [ ] Debug still reaches the Bluetooth transcript, and Copy still works.
- [ ] **There is no Connect button.** Turn the desktop's Wi-Fi off and watch a
      peer go from Connected → Not connected on its own, and back when it
      returns, with nobody pressing anything.
- [ ] A peer mid-handshake shows a spinner and "Connecting…", not "Not
      connected". *Easiest to see by putting the desktop to sleep and waking it.*
- [ ] An unreachable peer states a reason under Status — e.g. "Nothing has found
      it yet…". Confirm the reason **disappears** once it connects.
- [ ] An error banner appears above the tabs, is dismissible, and clears itself
      after ~30s.

## 3. Media (Stage C)

Play something on the desktop with a real player (Chromium is the one this was
built against).

- [ ] The timeline advances roughly in real time, not in two-second jumps.
- [ ] **Drag the timeline.** The track moves to where you dropped it, and the
      knob does not snap back before the new position arrives.
- [ ] On a stream with no length, or a player reporting `can_seek = false`, the
      timeline is a plain bar with no draggable knob.
- [ ] The volume row shows a percentage that tracks the slider.
- [ ] A transport button that the player ignores turns orange rather than
      pretending it worked. *Chromium reporting `CanControl` while ignoring a
      volume write is the known case.*
- [ ] Background the app for a minute with something playing, come back, and
      confirm the desktop was not being polled meanwhile *(watch the journal)*.
- [ ] Over Bluetooth only, with Wi-Fi off: media still updates, more slowly.

## 4. Lock screen and controls (Stage C)

- [ ] Add the circular accessory widget to the Lock Screen; it shows a lock,
      open lock, or computer glyph.
- [ ] The rectangular and inline accessory widgets still render.
- [ ] Add the **Wake PC** control to Control Centre, and to a Lock Screen
      button slot. Pressing it wakes a sleeping machine **from the Lock Screen,
      without unlocking the phone.**
- [ ] With no wake target on file, the control says so rather than doing
      nothing silently.

> There is deliberately **no Lock control**. A control runs in the widget
> process, which has no Local Network permission of its own, and locking needs a
> live session. Do not add one without testing that it can actually reach the
> desktop from the Lock Screen.

## 5. `acryliusctl` (Stage D)

Mostly covered by the M0/M1 acceptance runs, which pass. What they do not cover:

- [ ] `acryliusctl --help` reads as nine groups, and each group's `--help`
      lists its verbs.
- [ ] `acryliusctl --version` prints a version. *(It used to fail.)*
- [ ] `acryliusctl play status <dev> --json | jq .` parses, and the numbers
      agree with the table from the same command without `--json`.
- [ ] **The correlation fix, with two computers paired.** Play something on
      both, then run `acryliusctl play status A` repeatedly. Every answer must
      be A's. Before M3 this returned B's now-playing routinely, because the
      media plugin broadcasts state every two seconds. *This is the one item
      here that needs a second desktop.*
- [ ] With two peers, `acryliusctl device ping A` while B is going up and down:
      the ping must not report B's state.

## 6. Pairing (Stage E)

**Tap a computer, compare six digits, press a button on each end.** Nothing is
typed and nothing is scanned. The code, the QR and the scanner are all gone.

Most of this is automated by `./scripts/m3-acceptance.sh`; run that first, and
run it with `ACRYLIUS_M3_PHONE=1` to print the phone half as a checklist.

- [ ] On the phone: Pair shows an **On this network** section listing the
      desktop, with its name and address. There is **no** code field and **no**
      scan button anywhere on the screen.
- [ ] Tapping the row pairs outright rather than filling in a text field.
- [ ] Six digits appear on the phone **and** as a desktop notification, and
      they match.
- [ ] The notification carries **They match** and **They don't** as buttons.
- [ ] Pressing **They match** on both ends pairs, and a session comes up without
      force-quitting the app.
- [ ] Pressing **They don't** on either end pairs nothing, and a second attempt
      from the phone is refused for a few minutes afterwards. *This cooldown is
      load-bearing: see PROTOCOL.md § 8.*
- [ ] While the desktop is showing digits, its row on a **second** phone is
      greyed out and marked **busy**. *This is `pair=1`, which now means busy
      rather than ready.*
- [ ] With `[share] enabled = false` in the desktop config, the pairing
      notification still appears. *It used to be built only alongside file
      sharing, so turning sharing off silently cost every desktop prompt.*
- [ ] Kill the notification daemon, then pair: the desktop degrades to a
      notification-free flow that `acryliusctl pair` can still answer, rather
      than losing the pairing.
- [ ] Over SSH with no desktop at all, `acryliusctl pair` waits, prints the
      digits when a phone asks, and `pair approve` answers it.
- [ ] A device name with punctuation or non-Latin characters — rename the
      desktop to something like `Vercixx's PC` or `кухня` — renders correctly in
      the notification summary and in the phone's list.

## 7. Nothing broke

- [ ] Send a file phone → desktop, and desktop → phone. Accept from the
      notification, and accept from `acryliusctl file accept`.
- [ ] Lock and unlock the desktop from the phone; confirm the phone reports the
      truth both times.
- [ ] Clipboard both directions.
- [ ] Run a configured command.
- [ ] A transfer over Bluetooth is refused with a clear reason, not a hang.

---

## Deliberately not built

Two things the M3 plan called for do not exist, and are not pending work — the
design that replaced them does not need either:

- **`PairRequest` (protocol kind byte 4).** It existed only because an unpaired
  phone had no pre-shared key and therefore nothing it could say. Pairing has no
  key at all now, so the pairing handshake — kind byte 1 — *is* the request.
- **The Qt6 pairing window.** A notification carries six digits and two buttons,
  which is the whole ceremony. No new workspace member, no cxx-qt, no CMake.

## Two reports that are not what they look like

**The widget extension is in the IPA.** I unpacked the artifact this build came
from: `Payload/Acrylius.app/PlugIns/AcryliusWidgets.appex` is present, signed,
`CFBundleIdentifier org.acrylius.app.widgets`, `MinimumOSVersion 17.0`, with
`NSExtensionPointIdentifier = com.apple.widgetkit-extension`. CI also refuses to
package a build without it. So SideStore not offering the extensions prompt is
happening on the device side, not in the build — the usual cause is the free
Apple ID's App ID budget, which is ten per week and needs a *second* one for the
extension. Worth checking whether SideStore has a remembered "remove extensions"
choice, and what its log says while installing.

**"On this network" is empty because there is nothing to put in it.** The
section lists devices this phone is *not paired with*, and you have one
computer, already paired. It will appear when a second machine is on the
network, or after forgetting the one you have. For the **busy** mark the desktop
must also be running an M3 daemon (`./scripts/install.sh` — `pair=1` is produced
by nothing older) and be mid-pairing with somebody else at that moment.
