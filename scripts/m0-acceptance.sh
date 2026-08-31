#!/usr/bin/env bash
#
# M0 acceptance: two daemons on one machine pair over TCP, ping, and survive a
# restart. Run from the repo root after `cargo build`.
# State lives under /tmp: a Unix socket path has a hard ~108-byte limit.
set -u
D=/tmp/acr; BIN="$PWD/target/debug"

# Not the default port: an installed daemon holds 1971.
PORT_A=19710
PORT_B=19720

# pkill returns before the port/Wayland selection is released, so poll until the process is gone.
# Matched by state dir, not binary name: a name pattern would also match this shell's own pgrep/pkill.
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

# Build here: nothing else in this script would notice a stale binary.
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
echo "### 2. bravo waits to be asked; alpha dials it"
"$BIN/acryliusctl" --state $D/b pair > $D/b.pair 2>&1 &
sleep 0.5
"$BIN/acryliusctl" --state $D/a pair with 127.0.0.1:$PORT_B > $D/a.pair 2>&1 &
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
# No --addr on purpose: pairing must have recorded the address it proved.
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
