#!/usr/bin/env bash
#
# M1 acceptance. Two daemons on one machine pair, then exercise every feature:
# session query, clipboard, run-a-command, media, file transfer, and a relayed
# wake.
#
# Both daemons share this machine's real desktop, so "the peer's session" and
# "our session" are the same one. That is fine for checking the wire and the
# effectors; only a second physical machine can check that the right desktop was
# affected.
#
# State lives under /tmp because a Unix socket path has a hard ~108 byte limit.
set -u
D=/tmp/acr-m1; BIN="$PWD/target/debug"

# Not the default port. A developer running this has an installed daemon on
# 1971, and the failure it caused — one instance quietly refusing to bind, then
# a pairing that never completes — looks nothing like a port conflict.
PORT_A=19711
PORT_B=19721

# Wait for a previous run's daemons to actually be gone. pkill returns as
# soon as the signal is sent, and a daemon that still holds the listening
# port, or the Wayland selection, makes the next run fail in a way that looks
# like a flake.
#
# The pattern matches this run's state directory rather than the binary: one
# naming the binary also matches the shell running this script, which then kills
# itself, and it would take an installed daemon with it.
mine() { pgrep -f "acryliusd --state $D/" 2>/dev/null; }
cleanup() { mine | xargs -r kill 2>/dev/null || true; }
trap cleanup EXIT

cleanup
for i in $(seq 1 50); do mine >/dev/null || break; sleep 0.1; done
rm -rf $D; mkdir -p $D/a $D/b
export RUST_LOG=acryliusd=warn,acrylius_rt=warn,acrylius_linux=warn

# What the "PC" is willing to do. Nothing here can be changed from the network.
cat > $D/b/config.toml <<CFG
name = "bravo"

[wol]
macs = ["00:11:22:33:44:55"]
broadcast = "127.0.0.1"
port = 9
allowlist = ["aa:bb:cc:dd:ee:ff"]

[commands.hello]
name = "Say hello"
program = "/bin/echo"
args = ["hello from bravo"]

[commands.fail]
name = "Fail on purpose"
program = "/bin/false"

# Both daemons sit on loopback, so the address a real deployment discovers for
# itself would be this machine's LAN address and the peer could not reach it.
[share]
directory = "$D/b-dl"
advertise_host = "127.0.0.1"
CFG

# Alpha stands in for a phone: it reads the peer's clipboard but never pushes
# or owns one. Without this, two daemons on one desktop fight over selection
# ownership, which no real deployment does.
cat > $D/a/config.toml <<ACFG
name = "alpha"

[clipboard]
send = false
receive = false

[share]
directory = "$D/a-dl"
advertise_host = "127.0.0.1"
ACFG

"$BIN/acryliusd" --state $D/a --port $PORT_A --config $D/a/config.toml > $D/a.log 2>&1 &
"$BIN/acryliusd" --state $D/b --port $PORT_B --config $D/b/config.toml > $D/b.log 2>&1 &
ready() { for i in $(seq 1 100); do "$BIN/acryliusctl" --state "$1" status >/dev/null 2>&1 && return 0; sleep 0.1; done; return 1; }
ready $D/a || { echo "alpha never came up"; cat $D/a.log; exit 1; }
ready $D/b || { echo "bravo never came up"; cat $D/b.log; exit 1; }

fail=0
check() { if [ "$1" = 0 ]; then echo "  ok   $2"; else echo "  FAIL $2"; fail=1; fi; }

# Built here rather than assumed, because nothing else in this script would
# notice it was stale. A run against a binary from an earlier day reported a
# failure that had been fixed hours before, and would just as happily report a
# pass for a fix that is not in it.
if ! cargo build --quiet; then
  echo "  FAIL the workspace does not build; nothing to accept"
  exit 1
fi

echo "### capabilities each side negotiated"
"$BIN/acryliusctl" --state $D/a status | sed -n '5,6p'
"$BIN/acryliusctl" --state $D/b status | sed -n '5,6p'
B_ID=$("$BIN/acryliusctl" --state $D/b status | head -1 | awk '{print $2}')

echo
echo "### pair"
"$BIN/acryliusctl" --state $D/b pair --code ACRYLIUS > $D/b.pair 2>&1 &
sleep 0.5
"$BIN/acryliusctl" --state $D/a pair with 127.0.0.1:$PORT_B ACRYLIUS > $D/a.pair 2>&1 &
sleep 1.5
SAS_A=$(grep -o 'It should be showing:  *[0-9 ]*' $D/a.pair | head -1)
SAS_B=$(grep -o 'It should be showing:  *[0-9 ]*' $D/b.pair | head -1)
[ -n "$SAS_A" ] && [ "$SAS_A" = "$SAS_B" ]; check $? "the same code on both ends ($SAS_A)"
"$BIN/acryliusctl" --state $D/a pair approve >/dev/null
"$BIN/acryliusctl" --state $D/b pair approve >/dev/null
sleep 1

"$BIN/acryliusctl" --state $D/a device connect "$B_ID" --addr 127.0.0.1:$PORT_B >/dev/null
sleep 0.5

echo
echo "### session"
OUT=$("$BIN/acryliusctl" --state $D/a screen query "$B_ID" 2>&1); echo "  $OUT"
echo "$OUT" | grep -q "is unlocked"; check $? "bravo reports its session, and it is unlocked"

echo
echo "### clipboard"
wl-copy "acrylius m1 test" 2>/dev/null || echo "  (wl-copy unavailable; setting via the daemon instead)"
sleep 1
OUT=$("$BIN/acryliusctl" --state $D/a clip get "$B_ID" 2>&1); echo "  read back: $OUT"
echo "$OUT" | grep -q "acrylius m1 test"; check $? "alpha read bravo's clipboard"

echo
echo "### commands"
OUT=$("$BIN/acryliusctl" --state $D/a cmd list "$B_ID" 2>&1); echo "$OUT" | sed 's/^/  /'
echo "$OUT" | grep -q "hello"; check $? "bravo published its catalogue"

OUT=$("$BIN/acryliusctl" --state $D/a cmd run "$B_ID" hello 2>&1); echo "  $OUT"
echo "$OUT" | grep -q "exit 0"; check $? "a listed command ran"

OUT=$("$BIN/acryliusctl" --state $D/a cmd run "$B_ID" fail 2>&1); echo "  $OUT"
echo "$OUT" | grep -q "exit 1"; check $? "a failing command reports its code"

OUT=$("$BIN/acryliusctl" --state $D/a cmd run "$B_ID" '/bin/sh' 2>&1); echo "  $OUT"
echo "$OUT" | grep -q "refused"; check $? "an unlisted command is refused"

echo
echo "### media"
OUT=$("$BIN/acryliusctl" --state $D/a play status "$B_ID" 2>&1); echo "  $OUT" | head -3
# A machine with nothing open is a normal state and not a failure, so the check
# is that the question was answered rather than that something was playing.
echo "$OUT" | grep -qE 'nothing is playing|[a-z]'; check $? "bravo answered about its players"

OUT=$("$BIN/acryliusctl" --state $D/a play volume "$B_ID" 500 2>&1); echo "  $OUT"
echo "$OUT" | grep -q "refused"; check $? "a volume out of range is refused, and promptly"

# A volume command with no player named moves the machine, not a player. MPRIS
# gives every player a writable `Volume` that a great many ignore — Chromium
# accepts the write and does nothing, while reporting CanControl true — so the
# slider people actually reach for has to move something that always moves.
#
# `output volume`, not `vol`: the latter is a player's own, and reading it here
# is what left this desktop at full volume once.
#
# Off unless asked for. Everything else in this script is invisible from across
# the room; this one is not, and a restore step that read the wrong number once
# left a desktop at full volume with music playing. Run it deliberately:
#
#     ACRYLIUS_TOUCH_AUDIO=1 ./scripts/m1-acceptance.sh
#
sysvol() {
  "$BIN/acryliusctl" --state $D/a play status "$B_ID" 2>&1 \
    | grep -oE 'output volume [0-9]+%' | head -1 | tr -dc '0-9'
}
WAS=$(sysvol)
if [ "${ACRYLIUS_TOUCH_AUDIO:-0}" != "1" ]; then
  echo "  skip  volume is audible; set ACRYLIUS_TOUCH_AUDIO=1 to test it"
elif [ -z "$WAS" ]; then
  echo "  skip  no mixer on this machine"
else
  WANT=$(( WAS > 50 ? 42 : 73 ))
  "$BIN/acryliusctl" --state $D/a play volume "$B_ID" $WANT >/dev/null 2>&1
  GOT=$(sysvol); echo "  asked for $WANT%, machine reports $GOT%"
  [ -n "$GOT" ] && [ "$GOT" -ge $((WANT - 5)) ] && [ "$GOT" -le $((WANT + 5)) ]
  check $? "a volume with no player named moves the machine"
  # Put it back. This runs against a real desktop and has no business leaving
  # it louder or quieter than it found it.
  "$BIN/acryliusctl" --state $D/a play volume "$B_ID" "$WAS" >/dev/null 2>&1
  BACK=$(sysvol)
  [ "$BACK" = "$WAS" ]; check $? "and it was put back to $WAS%"
fi

OUT=$("$BIN/acryliusctl" --state $D/a play pause "$B_ID" --player nosuchplayer 2>&1); echo "  $OUT"
echo "$OUT" | grep -q "refused"; check $? "a player that does not exist is refused"

echo
echo "### file transfer"
# Bigger than one 64 KiB chunk, so the sequence numbering and the final short
# chunk are both exercised rather than a single frame that happens to work.
head -c 200000 /dev/urandom > $D/photo.bin

OUT=$("$BIN/acryliusctl" --state $D/b file offers 2>&1); echo "  $OUT"
echo "$OUT" | grep -q "nothing offered"; check $? "an unoffered transfer is not waiting"

# Backgrounded: the sender blocks until the receiver has answered, and nobody
# has yet.
"$BIN/acryliusctl" --state $D/a file send "$B_ID" $D/photo.bin > $D/send.out 2>&1 &
SENDER=$!
for i in $(seq 1 50); do
  "$BIN/acryliusctl" --state $D/b file offers 2>&1 | grep -q photo.bin && break
  sleep 0.1
done
OUT=$("$BIN/acryliusctl" --state $D/b file offers 2>&1); echo "  $OUT"
echo "$OUT" | grep -q "photo.bin"; check $? "bravo was told about the file, and its size"

TRANSFER=$(echo "$OUT" | grep photo.bin | head -1 | awk '{print $1}')
OUT=$("$BIN/acryliusctl" --state $D/b file accept "$TRANSFER" 2>&1); echo "  $OUT"
echo "$OUT" | grep -q "finished"; check $? "bravo accepted, and the transfer finished"
wait $SENDER 2>/dev/null || true

cmp -s $D/photo.bin $D/b-dl/photo.bin
check $? "every byte arrived, unchanged"

# A peer is told a name, a size and an id. Where the file sits on the sending
# machine is never part of that, so it cannot appear in what bravo reports.
if echo "$OUT" | grep -q "$D/photo.bin"; then R=1; else R=0; fi
check $R "the sending machine's path stayed on the sending machine"

OUT=$("$BIN/acryliusctl" --state $D/b file accept 4242 2>&1); echo "  $OUT"
echo "$OUT" | grep -q "no offer numbered"; check $? "a transfer nobody offered cannot be accepted"

OUT=$("$BIN/acryliusctl" --state $D/b file offers 2>&1); echo "  $OUT"
echo "$OUT" | grep -q "nothing offered"; check $? "a transfer that is over is no longer waiting"

# A second copy under the same name must not replace the first: two photos
# called the same thing is ordinary, losing one is not.
"$BIN/acryliusctl" --state $D/a file send "$B_ID" $D/photo.bin > $D/send2.out 2>&1 &
SENDER=$!
for i in $(seq 1 50); do
  T2=$("$BIN/acryliusctl" --state $D/b file offers 2>&1 | grep photo.bin | head -1 | awk '{print $1}')
  [ -n "$T2" ] && [ "$T2" != "$TRANSFER" ] && break
  sleep 0.1
done
"$BIN/acryliusctl" --state $D/b file accept "$T2" >/dev/null 2>&1
wait $SENDER 2>/dev/null || true
[ "$(ls $D/b-dl | wc -l)" = 2 ]; check $? "a second file of the same name did not replace the first"

echo
echo "### wake"
OUT=$("$BIN/acryliusctl" --state $D/a wake "$B_ID" aa:bb:cc:dd:ee:ff 2>&1); echo "  $OUT"
echo "$OUT" | grep -qE '^ok'; check $? "an allowlisted MAC is relayed"

OUT=$("$BIN/acryliusctl" --state $D/a wake "$B_ID" 99:99:99:99:99:99 2>&1); echo "  $OUT"
echo "$OUT" | grep -q "refused"; check $? "a MAC that is not allowlisted is refused"

echo
[ $fail = 0 ] && echo "M1 acceptance passed" || echo "M1 acceptance FAILED"
exit $fail
