# M3 — what a machine cannot check

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

The half that is done: a desktop advertises an open window, the phone lists what
is nearby and marks which machines are waiting, and a QR replaces typing.

- [ ] `acryliusctl pair` draws a QR in the terminal above the code.
- [ ] The QR is scannable from a phone camera at arm's length. *If it is not,
      the error-correction level is `L` and can be raised.*
- [ ] Over SSH, the QR still renders (it is text, not an image).
- [ ] On the phone: Pair shows an **On this network** section listing the
      desktop, with the desktop's name and address.
- [ ] While `acryliusctl pair` is running, that row is marked **waiting**;
      within a few seconds of the window closing, the mark goes away. *This is
      `pair=1`, which has been specified since M0 and produced by nothing until
      now — both ends have been reading a flag that was always false.*
- [ ] Tapping a nearby row fills in the address.
- [ ] **Scan the QR.** Pairing completes with nothing typed, and the six-digit
      SAS on the phone matches the terminal.
- [ ] A QR that is not ours — a Wi-Fi code, a URL — says "That is not an
      Acrylius pairing code" rather than failing silently or crashing.
- [ ] First scan raises the camera permission prompt with the wording from
      `Info.plist`. **The app has never asked for the camera before**; a missing
      usage string is a crash, so this is worth doing on a clean install.
- [ ] Deny camera access, then open Scan again: it explains rather than showing
      a black rectangle.
- [ ] A device name with punctuation or non-Latin characters — rename the
      desktop to something like `Vercixx's PC` or `кухня` — survives the QR
      round trip. *Unit-tested, but never through a real camera.*
- [ ] Typing the code by hand still works, and `pair approve` still answers a
      non-terminal `pair`.

## 7. Nothing broke

- [ ] Send a file phone → desktop, and desktop → phone. Accept from the
      notification, and accept from `acryliusctl file accept`.
- [ ] Lock and unlock the desktop from the phone; confirm the phone reports the
      truth both times.
- [ ] Clipboard both directions.
- [ ] Run a configured command.
- [ ] A transfer over Bluetooth is refused with a clear reason, not a hang.

---

## Known not done

These are Stage E items that are **not in this build**, so do not go looking:

- **`PairRequest` (protocol kind byte 4).** Tapping a nearby computer fills in
  its address; it does not open a pairing window on that computer. The window
  still has to be opened at the keyboard with `acryliusctl pair`. This is the
  piece that would make the phone-initiated flow complete, and it is the only
  remaining wire change in M3.
- **The Qt6 pairing window.** Pairing on Linux is still the terminal. The
  notification path for file offers is unchanged and still works.
- **`ServiceRemoved` handling.** mDNS never withdraws a service here, so a
  computer switched off stays in the phone's nearby list until the app filters
  it by age (5 minutes). A stale row costs a failed dial with a clear message,
  not a wrong pairing.
