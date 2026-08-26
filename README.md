# acrylius

Link an iPhone and a Linux computer over a local network. Lock and unlock the
desktop, wake it, share the clipboard, and run commands it has offered.

Everything on the wire is defined once, in Rust, and compiled into both ends.
The daemon uses that crate directly; the iOS app uses the same crate through
UniFFI. There is no second implementation to keep in step, which is the whole
reason this project exists: its predecessor ended up with five.

## What works today

```
$ acryliusctl status
desktop  UnOfEh0ZNHHuzLPviAR2fA
  fingerprint  JZrTp_5-8vWbrAf5NRFJKiS2znkhlXjLk9OGoLFtb5A
  port         1971
  accepts      org.acrylius.clipboard/1, org.acrylius.command/1,
               org.acrylius.ping/1, org.acrylius.session/1, org.acrylius.wol/1
```

| | |
|---|---|
| **Protocol** | [`docs/PROTOCOL.md`](docs/PROTOCOL.md), normative, with vectors the tests assert |
| **Pairing** | `acryliusctl pair` on the computer, then scan or type the code. Both ends show six digits to compare, and each asks you to confirm them. |
| **Session** | `acryliusctl session <device> lock` / `unlock` / `query` |
| **Clipboard** | `acryliusctl clipboard <device>` to read, `--push` to send |
| **Commands** | `acryliusctl commands <device>`, then `run <device> <id>` |
| **Wake** | The phone sends the packet. A sleeping machine runs no daemon. |

## Installing it

```bash
./scripts/install.sh
```

Builds, puts `acryliusd` and `acryliusctl` in `~/.local/bin`, writes a commented
config if there is none, and starts a `systemd --user` service tied to your
graphical session. It needs no root and never asks for any: locking your own
session works because logind passes the session owner's uid to polkit as
`good_user`, so nothing here requires privilege — which is what lets the unit be
locked down hard.

Run it again after pulling. It notices an existing installation, replaces the
binaries, adds settings a newer version introduced, and restarts the service.
Your config is not overwritten: a value you set is never reset and your comments
survive, because the only thing an update does to that file is add what is
missing.

```
$ ./scripts/install.sh
Updating an existing installation
  acryliusd 0.1.0
...
Config
  added to /home/you/.config/acrylius/config.toml:
    [session]  lock_command, unlock_command
```

To run it in the foreground instead — for a second instance, or to watch it —
`cargo build --release && ./target/release/acryliusd`.

Configuration lives at `~/.config/acrylius/config.toml` and is where every
decision about what this machine offers is made. Nothing in it can be changed
from the network. `acryliusd config check` parses it without starting anything.

```toml
name = "desktop"

[wol]
macs = ["00:11:22:33:44:55"]
broadcast = "192.168.1.255"

[commands.screenshot]
name = "Take a screenshot"
program = "/usr/bin/grim"       # absolute, always
args = ["/tmp/shot.png"]
```

A command is chosen by an id the computer published. The wire never carries a
command string, so there is nothing to quote and nothing to escape.

## Checking it

Most of this is verifiable on Linux, with no Apple hardware, which is
deliberate: there is no Mac here and every Swift change would otherwise cost a
fifteen-minute round trip through CI.

```bash
cargo test --workspace          # protocol, plugins, effectors
./scripts/swift-test.sh         # the iOS runtime, on Linux
./scripts/m0-acceptance.sh      # two daemons pair over real TCP, ping, restart
./scripts/m1-acceptance.sh      # every feature, against this desktop
./scripts/xcodegen-check.sh     # validate ios/project.yml without Xcode
```

To prove an unlock really happened, read the state back rather than trusting an
exit code:

```bash
loginctl lock-session "$XDG_SESSION_ID" && sleep 2
hyprctl locked                          # true
acryliusctl session <device> unlock
hyprctl locked                          # false, and this is the proof
```

`LockedHint` is not enough on its own. Some lockers never maintain it: Noctalia
on Hyprland locks the screen while the hint still reads `no`. A hint of `yes` is
trusted; a `no` on an active Wayland session is checked against the compositor
first, and a compositor that cannot answer leaves the hint standing.

## The iPhone app

There is no Mac in this project, so the app is built on a GitHub Actions macOS
runner and signed on the phone by SideStore with a free Apple ID. Run the
`ios-ipa` workflow and install the artifact.

A free account brings limits that shape the design rather than merely annoying
it. There is no push, so a computer can reach the phone only while the app is
open, and every feature here is therefore phone-initiated. There is no multicast
entitlement, so the phone never broadcasts: it aims a wake packet at the last
known address, which works because a network interface matches the packet's
payload and ignores where it was sent. And App IDs are limited per week, so the
app ships no extensions at all; the Shortcuts actions live in the app target,
where they need no entitlement.

## Security

Identity is a static X25519 key. Pairing runs `Noise_XXpsk0` with the pairing
code mixed in as a pre-shared key, so a wrong code does not fail a check, it
fails to decrypt. Sessions run `Noise_IKpsk2` in one round trip. The session's
own cipher counter handles replay, which is why there are no nonces, no
timestamps on messages, and no per-message signatures anywhere.

Opening a pairing window has no network route. It is reachable only over a
`0600` Unix socket whose peer credentials are checked, so "you must be at the
machine" is a property of the transport rather than a rule a handler could
forget.

The daemon runs as your user and needs no privilege. logind passes a session's
owner uid to polkit as `good_user`, which short-circuits the check when the
caller's uid matches, so locking your own session needs no sudo, no polkit rule
and no setuid binary.

## Layout

```
crates/acrylius-proto    the wire format, no_std, no crypto, no IO
crates/acrylius-core     the sans-IO state machine and the plugins
crates/acrylius-rt       tokio host runtime, for Rust hosts only
crates/acrylius-linux    logind, the Wayland clipboard, the command runner
crates/acrylius-ffi      the UniFFI facade, the only crate iOS sees
crates/acryliusd         the daemon and acryliusctl
ios/                     the app; project.yml generates the Xcode project
```

The core owns no sockets, no clock and no filesystem. A host feeds it events and
carries out the actions it returns, which is what lets one artifact drive a
tokio daemon on Linux and Network.framework on iOS, and what lets the whole
protocol be tested without either.
