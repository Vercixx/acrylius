#!/usr/bin/env bash
#
# M3 acceptance: pairing by tapping, confirmed by six digits.
#
# The admission policy is the whole of this milestone's security, so most of
# what follows is about what a device *refuses*. Pairing runs plain `XX` with no
# pre-shared key — anybody who can reach a daemon can complete a handshake with
# it — and what keeps that from being a way in is the six digits a person
# compares plus the rules in PROTOCOL.md § 8. Those rules are testable from a
# shell, and every one of them is checked here.
#
# Self-skipping like the M2 run: it reports what this machine has rather than
# failing on what it does not. The half that needs a phone is a checklist,
# because no script can tap a row on somebody's iPhone.
#
# State lives under /tmp because a Unix socket path has a hard ~108 byte limit.
set -u
D=/tmp/acr-m3; BIN="$PWD/target/debug"

# Not the default port: a developer running this has an installed daemon on
# 1971, and the failure a conflict causes — one instance quietly refusing to
# bind, then a pairing that never completes — looks nothing like a port clash.
PORT_A=19713
PORT_B=19723
PORT_C=19733

# Matches this run's state directory, never the binary name: a pattern naming
# the binary also matches the shell running this script, which then kills
# itself and takes the installed daemon with it.
mine() { pgrep -f "acryliusd --state $D/" 2>/dev/null; }
cleanup() { mine | xargs -r kill 2>/dev/null || true; }
trap cleanup EXIT

fail=0
check() { if [ "$1" = 0 ]; then echo "  ok   $2"; else echo "  FAIL $2"; fail=1; fi; }
skip() { echo "  skip $1"; }

# Built here rather than assumed, because nothing else in this script would
# notice it was stale. A run against a binary from an earlier day reported a
# failure that had been fixed hours before, and would just as happily report a
# pass for a fix that is not in it.
if ! cargo build --quiet; then
  echo "  FAIL the workspace does not build; nothing to accept"
  exit 1
fi

cleanup
for i in $(seq 1 50); do mine >/dev/null || break; sleep 0.1; done
rm -rf $D; mkdir -p $D/a $D/b $D/c
export RUST_LOG=acryliusd=warn,acrylius_rt=warn,acrylius_linux=warn

for who in a b c; do
  cat > $D/$who/config.toml <<CFG
name = "$who"

[clipboard]
send = false
receive = false

[share]
directory = "$D/$who-dl"
advertise_host = "127.0.0.1"
CFG
done

"$BIN/acryliusd" --state $D/a --port $PORT_A --config $D/a/config.toml > $D/a.log 2>&1 &
"$BIN/acryliusd" --state $D/b --port $PORT_B --config $D/b/config.toml > $D/b.log 2>&1 &
"$BIN/acryliusd" --state $D/c --port $PORT_C --config $D/c/config.toml > $D/c.log 2>&1 &
ready() { for i in $(seq 1 100); do "$BIN/acryliusctl" --state "$1" status >/dev/null 2>&1 && return 0; sleep 0.1; done; return 1; }
for who in a b c; do
  ready $D/$who || { echo "$who never came up"; cat $D/$who.log; exit 1; }
done

# One pairing attempt from `$1` aimed at `$2`, left running in the background
# with its output in a file. Nothing is armed first and nothing is typed: that
# is the entire point of the milestone.
ask() {
  "$BIN/acryliusctl" --state $D/$1 pair with 127.0.0.1:$2 > $D/$1.pair 2>&1 &
}
# The six digits a side is showing, or empty.
digits() { grep -o 'It should be showing:  *[0-9 ]*' $D/$1.pair 2>/dev/null | head -1; }

# Wait up to ~5s for a side to show digits. Polled rather than slept: a fixed
# wait is either too short on a loaded machine — which is how this script first
# reported a pairing failure that was really a slow handshake — or wasted time
# on an idle one.
wait_digits() {
  for _ in $(seq 1 50); do
    [ -n "$(digits $1)" ] && return 0
    sleep 0.1
  done
  return 1
}

# And the other direction: give a refusal long enough to have arrived, so that
# "no digits" means refused rather than not yet answered.
settle() { sleep 1.5; }

echo "### nothing has to be opened first"
"$BIN/acryliusctl" --state $D/b pair > $D/b.pair 2>&1 &
sleep 0.5
ask a $PORT_B
wait_digits a; wait_digits b
SAS_A=$(digits a); SAS_B=$(digits b)
[ -n "$SAS_A" ]; check $? "a tap alone put digits on the asking end"
[ -n "$SAS_B" ]; check $? "and on the answering end, which armed nothing"
[ -n "$SAS_A" ] && [ "$SAS_A" = "$SAS_B" ]; check $? "the same digits on both ends ($SAS_A)"

echo
echo "### a machine mid-pairing says so, and refuses the next one"
PAIRING=$("$BIN/acryliusctl" --state $D/b status --json 2>/dev/null | jq -r '.pairing // empty' 2>/dev/null)
if [ -z "$PAIRING" ]; then
  skip "status --json carries no pairing flag; checked on the wire instead"
else
  [ "$PAIRING" = "true" ]; check $? "b advertises that it is busy"
fi
ask c $PORT_B
settle
[ -z "$(digits c)" ]; check $? "c was refused while b was already comparing digits"
NOW_B=$(digits b)
[ "$NOW_B" = "$SAS_B" ]; check $? "and b is still comparing the digits it had"

echo
echo "### only a person at the machine writes a peer"
[ "$("$BIN/acryliusctl" --state $D/b device list | grep -c .)" -le 1 ]; check $? "a completed handshake alone paired nobody"
"$BIN/acryliusctl" --state $D/a pair approve >/dev/null 2>&1
"$BIN/acryliusctl" --state $D/b pair approve >/dev/null 2>&1
sleep 1
"$BIN/acryliusctl" --state $D/a device list | grep -q .; check $? "approving on both ends paired them"

echo
echo "### refusing the digits"
# A fresh pair, so the cooldown from anything above cannot be what is measured.
rm -f $D/c.pair $D/a.pair
"$BIN/acryliusctl" --state $D/c pair > $D/c.pair 2>&1 &
sleep 0.5
"$BIN/acryliusctl" --state $D/a pair with 127.0.0.1:$PORT_C > $D/a.pair 2>&1 &
wait_digits c
[ -n "$(digits c)" ]; check $? "c and a are comparing digits"
"$BIN/acryliusctl" --state $D/c pair deny >/dev/null 2>&1
sleep 0.5
[ "$("$BIN/acryliusctl" --state $D/c device list | grep -c .)" -le 1 ]; check $? "saying they differ stored nothing"

# The long cooldown. A mismatch is the one sign of a relayed handshake, so the
# next attempt has to cost more than one that merely lapsed — otherwise the
# one-in-a-million bound on the digits can simply be retried.
rm -f $D/a.pair
"$BIN/acryliusctl" --state $D/a pair with 127.0.0.1:$PORT_C > $D/a.pair 2>&1 &
settle
[ -z "$(digits a)" ]; check $? "and the next attempt is refused, not merely slower"

echo
echo "### finding something to pair with"
# The other half of `pair with`, which takes an address and until now had
# nothing anywhere that would tell you one.
#
# Polled, and skipped rather than failed when nothing at all turns up: this
# needs mDNS to actually work on the machine running the script, which is not
# something the script can arrange. A list that has *something* in it and not
# the machine we want is a real failure and is still reported as one.
# Matched on the port, not on `127.0.0.1:port`. Discovery advertises the
# address the machine is actually reachable at, which on a machine with a real
# network card is its LAN address — the loopback address is what this script
# *dials*, not what mDNS hands back.
for _ in $(seq 1 60); do
  "$BIN/acryliusctl" --state $D/a device nearby | grep -q ":$PORT_C " && break
  sleep 0.25
done
NEARBY=$("$BIN/acryliusctl" --state $D/a device nearby)
if ! echo "$NEARBY" | grep -q "fingerprint"; then
  skip "mDNS found nothing on this machine; the nearby list cannot be checked"
else
  echo "$NEARBY" | grep -q ":$PORT_C "
  check $? "c is listed as nearby, with the address pair with takes"
  echo "$NEARBY" | grep -q ":$PORT_B "; RC=$?
  [ $RC -ne 0 ]; check $? "and b is not, because a is already paired with it"
fi

echo
echo "### the CLI surface"
"$BIN/acryliusctl" pair --help 2>&1 | grep -q -- '--code'; RC=$?
[ $RC -ne 0 ]; check $? "no --code flag survives on \`pair\`"
"$BIN/acryliusctl" pair with --help 2>&1 | grep -qi '<CODE>'; RC=$?
[ $RC -ne 0 ]; check $? "and \`pair with\` takes an address and nothing else"

if command -v jq >/dev/null 2>&1; then
  "$BIN/acryliusctl" --state $D/a status --json | jq -e . >/dev/null 2>&1
  check $? "status --json parses under jq"
  "$BIN/acryliusctl" --state $D/a device list --json | jq -e . >/dev/null 2>&1
  check $? "device list --json parses under jq"
else
  skip "jq is not installed; --json shapes not checked"
fi

echo
if [ "${ACRYLIUS_M3_PHONE:-0}" = 1 ]; then
  cat <<'MANUAL'
### by hand, with a phone

  Nothing below can be scripted. Run the daemon normally (./scripts/install.sh)
  and work through it with the phone in your hand.

  [ ] The pairing sheet lists this computer, with no code field and no scan
      button anywhere on it.
  [ ] Tapping the row pairs outright — it does not fill in a text field.
  [ ] Six digits appear on the phone AND as a desktop notification, and they
      match.
  [ ] The notification carries "They match" and "They don't" as buttons.
  [ ] Pressing "They match" on both ends pairs, and a session comes up without
      force-quitting the app.
  [ ] Pressing "They don't" on either end pairs nothing, and a second attempt
      from the phone is refused for a while afterwards.
  [ ] While the desktop is showing digits, the row for it in a *second* phone
      is greyed out and marked busy.
  [ ] With `[share] enabled = false`, the pairing notification still appears.
      (It used to be built only alongside file sharing.)
  [ ] Killing the notification daemon and pairing again degrades to a
      notification-free flow that `acryliusctl pair` can still answer.
MANUAL
else
  skip "the phone half. Run with ACRYLIUS_M3_PHONE=1 to print the checklist"
fi

echo
if [ $fail = 0 ]; then echo "M3 acceptance passed"; else echo "M3 acceptance FAILED"; fi
exit $fail
