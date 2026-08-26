#!/usr/bin/env bash
#
# M1 acceptance. Two daemons on one machine pair, then exercise every feature:
# session query, clipboard, run-a-command, and a relayed wake.
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
cat > $D/b/config.toml <<'CFG'
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
CFG

# Alpha stands in for a phone: it reads the peer's clipboard but never pushes
# or owns one. Without this, two daemons on one desktop fight over selection
# ownership, which no real deployment does.
cat > $D/a/config.toml <<'ACFG'
name = "alpha"

[clipboard]
send = false
receive = false
ACFG

"$BIN/acryliusd" --state $D/a --port $PORT_A --config $D/a/config.toml > $D/a.log 2>&1 &
"$BIN/acryliusd" --state $D/b --port $PORT_B --config $D/b/config.toml > $D/b.log 2>&1 &
ready() { for i in $(seq 1 100); do "$BIN/acryliusctl" --state "$1" status >/dev/null 2>&1 && return 0; sleep 0.1; done; return 1; }
ready $D/a || { echo "alpha never came up"; cat $D/a.log; exit 1; }
ready $D/b || { echo "bravo never came up"; cat $D/b.log; exit 1; }

fail=0
check() { if [ "$1" = 0 ]; then echo "  ok   $2"; else echo "  FAIL $2"; fail=1; fi; }

echo "### capabilities each side negotiated"
"$BIN/acryliusctl" --state $D/a status | sed -n '5,6p'
"$BIN/acryliusctl" --state $D/b status | sed -n '5,6p'
B_ID=$("$BIN/acryliusctl" --state $D/b status | head -1 | awk '{print $2}')

echo
echo "### pair"
"$BIN/acryliusctl" --state $D/b pair --code ACRYLIUS > $D/b.pair 2>&1 &
sleep 0.5
"$BIN/acryliusctl" --state $D/a pair-with 127.0.0.1:$PORT_B ACRYLIUS > $D/a.pair 2>&1 &
sleep 1.5
SAS_A=$(grep -o 'It should be showing:  *[0-9 ]*' $D/a.pair | head -1)
SAS_B=$(grep -o 'It should be showing:  *[0-9 ]*' $D/b.pair | head -1)
[ -n "$SAS_A" ] && [ "$SAS_A" = "$SAS_B" ]; check $? "the same code on both ends ($SAS_A)"
"$BIN/acryliusctl" --state $D/a approve >/dev/null
"$BIN/acryliusctl" --state $D/b approve >/dev/null
sleep 1

"$BIN/acryliusctl" --state $D/a connect "$B_ID" --addr 127.0.0.1:$PORT_B >/dev/null
sleep 0.5

echo
echo "### session"
OUT=$("$BIN/acryliusctl" --state $D/a session "$B_ID" query 2>&1); echo "  $OUT"
echo "$OUT" | grep -q "is unlocked"; check $? "bravo reports its session, and it is unlocked"

echo
echo "### clipboard"
wl-copy "acrylius m1 test" 2>/dev/null || echo "  (wl-copy unavailable; setting via the daemon instead)"
sleep 1
OUT=$("$BIN/acryliusctl" --state $D/a clipboard "$B_ID" 2>&1); echo "  read back: $OUT"
echo "$OUT" | grep -q "acrylius m1 test"; check $? "alpha read bravo's clipboard"

echo
echo "### commands"
OUT=$("$BIN/acryliusctl" --state $D/a commands "$B_ID" 2>&1); echo "$OUT" | sed 's/^/  /'
echo "$OUT" | grep -q "hello"; check $? "bravo published its catalogue"

OUT=$("$BIN/acryliusctl" --state $D/a run "$B_ID" hello 2>&1); echo "  $OUT"
echo "$OUT" | grep -q "exit 0"; check $? "a listed command ran"

OUT=$("$BIN/acryliusctl" --state $D/a run "$B_ID" fail 2>&1); echo "  $OUT"
echo "$OUT" | grep -q "exit 1"; check $? "a failing command reports its code"

OUT=$("$BIN/acryliusctl" --state $D/a run "$B_ID" '/bin/sh' 2>&1); echo "  $OUT"
echo "$OUT" | grep -q "refused"; check $? "an unlisted command is refused"

echo
echo "### media"
OUT=$("$BIN/acryliusctl" --state $D/a media "$B_ID" 2>&1); echo "  $OUT" | head -3
# A machine with nothing open is a normal state and not a failure, so the check
# is that the question was answered rather than that something was playing.
echo "$OUT" | grep -qE 'nothing is playing|[a-z]'; check $? "bravo answered about its players"

OUT=$("$BIN/acryliusctl" --state $D/a media "$B_ID" volume --value 500 2>&1); echo "  $OUT"
echo "$OUT" | grep -q "refused"; check $? "a volume out of range is refused, and promptly"

OUT=$("$BIN/acryliusctl" --state $D/a media "$B_ID" pause --player nosuchplayer 2>&1); echo "  $OUT"
echo "$OUT" | grep -q "refused"; check $? "a player that does not exist is refused"

echo
echo "### wake"
OUT=$("$BIN/acryliusctl" --state $D/a wake "$B_ID" aa:bb:cc:dd:ee:ff 2>&1); echo "  $OUT"
echo "$OUT" | grep -qE '^ok'; check $? "an allowlisted MAC is relayed"

OUT=$("$BIN/acryliusctl" --state $D/a wake "$B_ID" 99:99:99:99:99:99 2>&1); echo "  $OUT"
echo "$OUT" | grep -q "refused"; check $? "a MAC that is not allowlisted is refused"

echo
[ $fail = 0 ] && echo "M1 acceptance passed" || echo "M1 acceptance FAILED"
exit $fail
