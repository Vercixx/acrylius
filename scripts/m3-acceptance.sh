#!/usr/bin/env bash
#
# M3 acceptance: tap-to-pair with six-digit confirmation; mostly checks the admission rules in PROTOCOL.md § 8.
# Self-skipping (the phone half is a checklist). State under /tmp: Unix socket paths cap at ~108 bytes.
set -u
D=/tmp/acr-m3; BIN="$PWD/target/debug"

# Not the default port: an installed daemon holds 1971.
PORT_A=19713
PORT_B=19723
PORT_C=19733

# Matches the state directory, not the binary name: a binary pattern also
# matches this shell itself.
mine() { pgrep -f "acryliusd --state $D/" 2>/dev/null; }
cleanup() { mine | xargs -r kill 2>/dev/null || true; }
trap cleanup EXIT

fail=0
check() { if [ "$1" = 0 ]; then echo "  ok   $2"; else echo "  FAIL $2"; fail=1; fi; }
skip() { echo "  skip $1"; }

# Build here: nothing else in this script would notice a stale binary.
if ! cargo build --quiet; then
  echo "  FAIL the workspace does not build; nothing to accept"
  exit 1
fi

cleanup
for i in $(seq 1 50); do mine >/dev/null || break; sleep 0.1; done
rm -rf $D; mkdir -p $D/a $D/b $D/c
# acryliusd at info: the log is where a pairing shows up with no notification
# daemon, and that is checked below.
export RUST_LOG=acryliusd=info,acrylius_rt=warn,acrylius_linux=warn
# Colour escape codes in the log files would break the greps below.
export NO_COLOR=1

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

# One background pairing attempt from $1 aimed at port $2.
ask() {
  "$BIN/acryliusctl" --state $D/$1 pair with 127.0.0.1:$2 > $D/$1.pair 2>&1 &
}
# The six digits a side is showing, or empty.
digits() { grep -o 'It should be showing:  *[0-9 ]*' $D/$1.pair 2>/dev/null | head -1; }

# Wait up to ~5s for a side to show digits; polled, since a fixed wait is too
# short on a loaded machine.
wait_digits() {
  for _ in $(seq 1 50); do
    [ -n "$(digits $1)" ] && return 0
    sleep 0.1
  done
  return 1
}

# Give a refusal long enough to arrive, so "no digits" means refused.
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
echo "### answering a pairing nobody was watching for"
# The case a desktop with no notification daemon lands in: a pairing that
# completed before anyone subscribed must still be visible.
grep -q "a device asked to pair" $D/b.log
check $? "the daemon says so in its log, digits included, with no UI at all"
grep -q "sas=" $D/b.log
check $? "and the digits are in there, not just the fact of it"
rm -f $D/b2.pair
timeout 5 "$BIN/acryliusctl" --state $D/b pair > $D/b2.pair 2>&1
grep -q "It should be showing" $D/b2.pair
check $? "a pair started after the handshake still shows what is waiting"

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
# A fresh pair, so a cooldown from anything above cannot be what is measured.
rm -f $D/c.pair $D/a.pair
"$BIN/acryliusctl" --state $D/c pair > $D/c.pair 2>&1 &
sleep 0.5
"$BIN/acryliusctl" --state $D/a pair with 127.0.0.1:$PORT_C > $D/a.pair 2>&1 &
wait_digits c
[ -n "$(digits c)" ]; check $? "c and a are comparing digits"
"$BIN/acryliusctl" --state $D/c pair deny >/dev/null 2>&1
sleep 0.5
[ "$("$BIN/acryliusctl" --state $D/c device list | grep -c .)" -le 1 ]; check $? "saying they differ stored nothing"

# A mismatch is the one sign of a relayed handshake: the next attempt must
# cost more than one that merely lapsed, or the digit bound can be retried.
rm -f $D/a.pair
"$BIN/acryliusctl" --state $D/a pair with 127.0.0.1:$PORT_C > $D/a.pair 2>&1 &
settle
[ -z "$(digits a)" ]; check $? "and the next attempt is refused, not merely slower"

echo
echo "### finding something to pair with"
# Skipped, not failed, when mDNS finds nothing: that isn't something the script controls.
# Matched on port alone: discovery advertises the LAN address, not the loopback this script dials.
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
