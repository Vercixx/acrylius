#!/usr/bin/env bash
#
# M0 acceptance. Two daemons on one machine pair over real TCP, open a session,
# ping, and survive a restart, with no Apple hardware and no second device.
#
# Run it from the repo root after `cargo build`.
#
# The state directories live under /tmp rather than somewhere deeper because a
# Unix socket path has a hard ~108-byte limit (SUN_LEN); a deeply nested state
# directory genuinely cannot host one. Real installs use $XDG_RUNTIME_DIR.
set -u
D=/tmp/acr; BIN="$PWD/target/debug"

# Not the default port. A developer running this has an installed daemon on
# 1971, and the failure it caused — one instance quietly refusing to bind, then
# a pairing that never completes — looks nothing like a port conflict.
PORT_A=19710
PORT_B=19720

# Wait for a previous run's daemons to actually be gone. pkill returns as
# soon as the signal is sent, and a daemon that still holds the listening
# port, or the Wayland selection, makes the next run fail in a way that looks
# like a flake.
#
# The pattern matches this run's state directory rather than the binary: a
# pattern naming the binary also matches the shell that runs this script, which
# then kills itself, and it would take an installed daemon with it.
mine() { pgrep -f "acryliusd --state $D/" 2>/dev/null; }
cleanup() { mine | xargs -r kill 2>/dev/null || true; }
trap cleanup EXIT

cleanup
for i in $(seq 1 50); do mine >/dev/null || break; sleep 0.1; done
rm -rf $D; mkdir -p $D/a $D/b
export RUST_LOG=acryliusd=info,acrylius_rt=warn

"$BIN/acryliusd" --state $D/a --port $PORT_A --name alpha > $D/a.log 2>&1 &
"$BIN/acryliusd" --state $D/b --port $PORT_B --name bravo > $D/b.log 2>&1 &
ready() { for i in $(seq 1 100); do "$BIN/acryliusctl" --state "$1" status >/dev/null 2>&1 && return 0; sleep 0.1; done; return 1; }
ready $D/a || { echo "alpha never came up"; cat $D/a.log; exit 1; }
ready $D/b || { echo "bravo never came up"; cat $D/b.log; exit 1; }

# Built here rather than assumed, because nothing else in this script would
# notice it was stale. A run against a binary from an earlier day reported a
# failure that had been fixed hours before, and would just as happily report a
# pass for a fix that is not in it.
if ! cargo build --quiet; then
  echo "  FAIL the workspace does not build; nothing to accept"
  exit 1
fi

echo "### 1. both daemons up"
"$BIN/acryliusctl" --state $D/a status
"$BIN/acryliusctl" --state $D/b status

A_ID=$("$BIN/acryliusctl" --state $D/a status | head -1 | awk '{print $2}')
B_ID=$("$BIN/acryliusctl" --state $D/b status | head -1 | awk '{print $2}')

echo
echo "### 2. bravo opens a pairing window; alpha dials it"
"$BIN/acryliusctl" --state $D/b pair --code ABCD1234 > $D/b.pair 2>&1 &
sleep 0.5
"$BIN/acryliusctl" --state $D/a pair with 127.0.0.1:$PORT_B ABCD1234 > $D/a.pair 2>&1 &
sleep 1.5

echo "--- what bravo shows ---"; cat $D/b.pair
echo "--- what alpha shows ---"; cat $D/a.pair

SAS_A=$(grep -o 'It should be showing:  *[0-9 ]*' $D/a.pair | head -1)
SAS_B=$(grep -o 'It should be showing:  *[0-9 ]*' $D/b.pair | head -1)
echo
if [ -n "$SAS_A" ] && [ "$SAS_A" = "$SAS_B" ]; then
  echo "### 3. PASS: both ends show the same code -> $SAS_A"
else
  echo "### 3. FAIL: alpha='$SAS_A' bravo='$SAS_B'"; exit 1
fi

echo
echo "### 4. pair approve at both ends"
"$BIN/acryliusctl" --state $D/a pair approve
"$BIN/acryliusctl" --state $D/b pair approve
sleep 1
cat $D/a.pair | tail -2; cat $D/b.pair | tail -2

echo
echo "### 5. paired devices"
"$BIN/acryliusctl" --state $D/a device list
"$BIN/acryliusctl" --state $D/b device list

echo
echo "### 6. alpha opens a session to bravo and pings"
# Deliberately no --addr. Passing one used to hide the fact that nothing
# recorded the address a successful pairing had just proved, so a freshly
# paired device reported itself unreachable.
"$BIN/acryliusctl" --state $D/a device connect "$B_ID"
"$BIN/acryliusctl" --state $D/a device ping "$B_ID"
RC=$?

echo
echo "### 7. restart alpha: the pairing must survive"
pkill -f "acryliusd --state $D/a" 2>/dev/null
sleep 1
"$BIN/acryliusd" --state $D/a --port $PORT_A --name alpha >> $D/a.log 2>&1 &
ready $D/a || { echo "alpha did not restart"; tail -5 $D/a.log; exit 1; }
"$BIN/acryliusctl" --state $D/a device list

exit $RC
