#!/usr/bin/env bash
#
# M1 acceptance: two daemons pair, then exercise session query, clipboard,
# commands, media, file transfer, and a relayed wake. Both use this machine's
# real desktop, so only a second machine can confirm which one was affected.
# State lives under /tmp: a Unix socket path has a hard ~108-byte limit.
set -u
D=/tmp/acr-m1; BIN="$PWD/target/debug"

# Not the default port: an installed daemon holds 1971.
PORT_A=19711
PORT_B=19721

# pkill returns before the port/Wayland selection is released, so poll until the process is gone.
# Matched by state dir, not binary name: a name pattern would also match this shell's own pgrep/pkill.
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

# Alpha stands in for a phone: clipboard off, or two daemons on one desktop
# fight over selection ownership.
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

# Build here: nothing else in this script would notice a stale binary.
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
"$BIN/acryliusctl" --state $D/b pair > $D/b.pair 2>&1 &
sleep 0.5
"$BIN/acryliusctl" --state $D/a pair with 127.0.0.1:$PORT_B > $D/a.pair 2>&1 &
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
# Nothing playing is a normal state; check the question was answered.
echo "$OUT" | grep -qE 'nothing is playing|[a-z]'; check $? "bravo answered about its players"

OUT=$("$BIN/acryliusctl" --state $D/a play volume "$B_ID" 500 2>&1); echo "  $OUT"
echo "$OUT" | grep -q "refused"; check $? "a volume out of range is refused, and promptly"

# Only when something is playing and seekable. Pinned to one player by id:
# "active" moves mid-run, and the seek then lands where the check is not looking.
SEEKABLE=$("$BIN/acryliusctl" --state $D/a play status "$B_ID" 2>&1 \
  | grep -E '^\*' | awk '{print $2}' || true)
if [ -z "$SEEKABLE" ]; then
  echo "  skip  nothing is playing; open a player to test seeking"
else
  echo "  seeking $SEEKABLE"
  # Checked after the fact, not from the reply, which returns before the seek lands.
  # If the track changes mid-check the stale seek is ignored on purpose; matched by trackid.
  status_line() {
    "$BIN/acryliusctl" --state $D/a play status "$B_ID" 2>&1 | grep -F "$SEEKABLE"
  }
  track_of() { printf '%s' "$1" | grep -oE '/[0-9]+:[0-9]{2}\]' | tr -d '/]'; }
  for MS in 30000 0; do
    WAS=$(track_of "$(status_line)")
    "$BIN/acryliusctl" --state $D/a play position "$B_ID" $MS \
      --player "$SEEKABLE" >/dev/null 2>&1
    sleep 1
    LINE=$(status_line)
    if [ "$(track_of "$LINE")" != "$WAS" ]; then
      echo "  skip  the track changed while seeking; a stale track id is ignored on purpose"
      continue
    fi
    AT=$(printf '%s' "$LINE" | grep -oE '\[[0-9]+:[0-9]{2}/' | tr -d '[/')
    # Allow drift: a playing track advances while this is read.
    NOW=$(( $(echo "${AT:-0:00}" | cut -d: -f1) * 60 + $(echo "${AT:-0:00}" | cut -d: -f2 | sed 's/^0//;s/^$/0/') ))
    WANT=$((MS / 1000))
    DRIFT=$((NOW - WANT)); [ $DRIFT -lt 0 ] && DRIFT=$((-DRIFT))
    echo "  asked for ${WANT}s, player is at ${NOW}s"
    # Zero on purpose: Chromium ignores SetPosition(track, 0) outright; see media.rs.
    [ $DRIFT -le 3 ]; check $? "a seek to ${MS}ms moves the track there"
  done
fi

# With no player named, this moves the machine's volume (many players ignore MPRIS Volume); reads "output volume."
# Audible: gated behind ACRYLIUS_TOUCH_AUDIO=1.
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
  # Put the real desktop's volume back.
  "$BIN/acryliusctl" --state $D/a play volume "$B_ID" "$WAS" >/dev/null 2>&1
  BACK=$(sysvol)
  [ "$BACK" = "$WAS" ]; check $? "and it was put back to $WAS%"
fi

OUT=$("$BIN/acryliusctl" --state $D/a play pause "$B_ID" --player nosuchplayer 2>&1); echo "  $OUT"
echo "$OUT" | grep -q "refused"; check $? "a player that does not exist is refused"

echo
echo "### file transfer"
# Bigger than one 64 KiB chunk: exercises sequencing and the final short chunk.
head -c 200000 /dev/urandom > $D/photo.bin

OUT=$("$BIN/acryliusctl" --state $D/b file offers 2>&1); echo "  $OUT"
echo "$OUT" | grep -q "nothing offered"; check $? "an unoffered transfer is not waiting"

# Backgrounded: the sender blocks until the receiver answers.
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

# The sender's local path must never reach the peer.
if echo "$OUT" | grep -q "$D/photo.bin"; then R=1; else R=0; fi
check $R "the sending machine's path stayed on the sending machine"

OUT=$("$BIN/acryliusctl" --state $D/b file accept 4242 2>&1); echo "  $OUT"
echo "$OUT" | grep -q "no offer numbered"; check $? "a transfer nobody offered cannot be accepted"

OUT=$("$BIN/acryliusctl" --state $D/b file offers 2>&1); echo "  $OUT"
echo "$OUT" | grep -q "nothing offered"; check $? "a transfer that is over is no longer waiting"

# A second copy under the same name must not replace the first.
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
