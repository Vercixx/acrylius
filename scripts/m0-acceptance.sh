#!/usr/bin/env bash
#
# M0 acceptance. Two daemons on one machine pair over real TCP, open a session,
# ping, and survive a restart — with no Apple hardware and no second device.
#
# Run it from the repo root after `cargo build`.
#
# The state directories live under /tmp rather than somewhere deeper because a
# Unix socket path has a hard ~108-byte limit (SUN_LEN); a deeply nested state
# directory genuinely cannot host one. Real installs use $XDG_RUNTIME_DIR.
set -u
D=/tmp/acr; BIN="$PWD/target/debug"
# Wait for a previous run's daemons to actually be gone. pkill returns as
# soon as the signal is sent, and a daemon that still holds the listening
# port, or the Wayland selection, makes the next run fail in a way that looks
# like a flake.
pkill -f 'target/debug/acryliusd' 2>/dev/null
for i in $(seq 1 50); do pgrep -f 'target/debug/acryliusd' >/dev/null || break; sleep 0.1; done
rm -rf $D; mkdir -p $D/a $D/b
export RUST_LOG=acryliusd=info,acrylius_rt=warn

"$BIN/acryliusd" --state $D/a --port 1971 --name alpha > $D/a.log 2>&1 &
"$BIN/acryliusd" --state $D/b --port 1972 --name bravo > $D/b.log 2>&1 &
ready() { for i in $(seq 1 100); do "$BIN/acryliusctl" --state "$1" status >/dev/null 2>&1 && return 0; sleep 0.1; done; return 1; }
ready $D/a || { echo "alpha never came up"; cat $D/a.log; exit 1; }
ready $D/b || { echo "bravo never came up"; cat $D/b.log; exit 1; }

echo "### 1. both daemons up"
"$BIN/acryliusctl" --state $D/a status
"$BIN/acryliusctl" --state $D/b status

A_ID=$("$BIN/acryliusctl" --state $D/a status | head -1 | awk '{print $2}')
B_ID=$("$BIN/acryliusctl" --state $D/b status | head -1 | awk '{print $2}')

echo
echo "### 2. bravo opens a pairing window; alpha dials it"
"$BIN/acryliusctl" --state $D/b pair --code ABCD1234 > $D/b.pair 2>&1 &
sleep 0.5
"$BIN/acryliusctl" --state $D/a pair-with 127.0.0.1:1972 ABCD1234 > $D/a.pair 2>&1 &
sleep 1.5

echo "--- what bravo shows ---"; cat $D/b.pair
echo "--- what alpha shows ---"; cat $D/a.pair

SAS_A=$(grep -o 'both screens: [0-9 ]*' $D/a.pair | head -1)
SAS_B=$(grep -o 'both screens: [0-9 ]*' $D/b.pair | head -1)
echo
if [ -n "$SAS_A" ] && [ "$SAS_A" = "$SAS_B" ]; then
  echo "### 3. PASS: both ends show the same code -> $SAS_A"
else
  echo "### 3. FAIL: alpha='$SAS_A' bravo='$SAS_B'"; exit 1
fi

echo
echo "### 4. approve at both ends"
"$BIN/acryliusctl" --state $D/a approve
"$BIN/acryliusctl" --state $D/b approve
sleep 1
cat $D/a.pair | tail -2; cat $D/b.pair | tail -2

echo
echo "### 5. paired devices"
"$BIN/acryliusctl" --state $D/a devices
"$BIN/acryliusctl" --state $D/b devices

echo
echo "### 6. alpha opens a session to bravo and pings"
"$BIN/acryliusctl" --state $D/a connect "$B_ID" --addr 127.0.0.1:1972
"$BIN/acryliusctl" --state $D/a ping "$B_ID"
RC=$?

echo
echo "### 7. restart alpha: the pairing must survive"
pkill -f "acryliusd --state $D/a" 2>/dev/null
sleep 1
"$BIN/acryliusd" --state $D/a --port 1971 --name alpha >> $D/a.log 2>&1 &
ready $D/a || { echo "alpha did not restart"; tail -5 $D/a.log; exit 1; }
"$BIN/acryliusctl" --state $D/a devices

pkill -f 'target/debug/acryliusd' 2>/dev/null
exit $RC
