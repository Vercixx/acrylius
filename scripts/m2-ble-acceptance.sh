#!/usr/bin/env bash
#
# M2 acceptance: the BLE transport.
#
# Everything a machine can check about itself is checked automatically — that
# the adapter can be a peripheral at all, that an unprivileged daemon registers
# a GATT application and gets on the air, and that what it publishes is exactly
# what a phone needs to find it. The half that needs a phone is a checklist,
# because no script here can turn Wi-Fi off on someone's iPhone.
#
# Self-skipping, like the M1 run in CI: a machine with no Bluetooth is a normal
# machine, and this reports what it has rather than failing.
set -u
D=/tmp/acr-m2; BIN="$PWD/target/debug"
PORT=19731
ADAPTER=/org/bluez/hci0

# Restated here on purpose rather than read out of the binary. This is the one
# part of the design that can never change: iOS caches a peripheral's attribute
# table, so a phone that has seen the old layout would keep using it. An
# independent copy of the contract is what makes a silent edit fail loudly.
SERVICE=61637279-6c69-7573-8001-000000000001
IDENTITY=61637279-6c69-7573-8001-000000000002
RX=61637279-6c69-7573-8001-000000000003
TX=61637279-6c69-7573-8001-000000000004

# Matches this run's state directory, never the binary name: a pattern naming
# the binary also matches the shell running this script, which then kills
# itself and takes the installed daemon with it.
mine() { pgrep -f "acryliusd --state $D/" 2>/dev/null; }
cleanup() { mine | xargs -r kill 2>/dev/null || true; }
trap cleanup EXIT

fail=0
check() { if [ "$1" = 0 ]; then echo "  ok   $2"; else echo "  FAIL $2"; fail=1; fi; }
skip() { echo "  skip $1"; }

prop() { busctl --system get-property org.bluez "$ADAPTER" "$1" "$2" 2>/dev/null; }

# Built here rather than assumed, because nothing else in this script would
# notice it was stale. A run against a binary from an earlier day reported a
# failure that had been fixed hours before, and would just as happily report a
# pass for a fix that is not in it.
if ! cargo build --quiet; then
  echo "  FAIL the workspace does not build; nothing to accept"
  exit 1
fi

echo "### the adapter"
if ! busctl --system status org.bluez >/dev/null 2>&1; then
  echo "  skip no bluetoothd on this machine; nothing to accept"
  exit 0
fi
ROLES=$(prop org.bluez.Adapter1 Roles)
if ! echo "$ROLES" | grep -q peripheral; then
  echo "  skip the adapter cannot be a peripheral (roles: ${ROLES:-none})"
  exit 0
fi
echo "  roles: $ROLES"
POWERED=$(prop org.bluez.Adapter1 Powered)
if ! echo "$POWERED" | grep -q true; then
  echo "  skip the adapter is off"
  exit 0
fi

# One radio, and an advertisement already on it is almost certainly the
# installed daemon. Two GATT applications offering the same service UUID would
# make every result below meaningless, so say so rather than measure noise.
BEFORE=$(prop org.bluez.LEAdvertisingManager1 ActiveInstances | awk '{print $2}')
if [ "${BEFORE:-0}" != "0" ]; then
  echo "  skip something is already advertising ($BEFORE instance(s));"
  echo "       stop the installed daemon first:  systemctl --user stop acryliusd"
  exit 0
fi

cleanup
for i in $(seq 1 50); do mine >/dev/null || break; sleep 0.1; done
rm -rf $D; mkdir -p $D/on $D/off
export RUST_LOG=acryliusd=info,acrylius_linux=info

echo
echo "### a daemon that is allowed to advertise"
cat > $D/on/config.toml <<CFG
name = "m2-ble"

[ble]
enabled = true
CFG
"$BIN/acryliusd" --state $D/on --port $PORT --config $D/on/config.toml > $D/on.log 2>&1 &
PID=$!
ready() { for i in $(seq 1 100); do "$BIN/acryliusctl" --state $D/on status >/dev/null 2>&1 && return 0; sleep 0.1; done; return 1; }
ready || { echo "  FAIL the daemon never came up"; cat $D/on.log; exit 1; }

# Registration is two round trips to bluetoothd, so it is not instant.
for i in $(seq 1 50); do
  AFTER=$(prop org.bluez.LEAdvertisingManager1 ActiveInstances | awk '{print $2}')
  [ "${AFTER:-0}" != "0" ] && break
  sleep 0.2
done
[ "${AFTER:-0}" != "0" ]; check $? "it got on the air (ActiveInstances $BEFORE -> ${AFTER:-0})"

# The whole point of the hardened unit: none of this needs root.
[ "$(id -u)" != "0" ]; check $? "and did it as an unprivileged user"

grep -q "GATT application registered" $D/on.log
check $? "bluetoothd accepted the GATT application"

echo
echo "### what a phone would actually find"
# Read our own exported tree back over the system bus — the same way
# bluetoothd read it. Our daemon holds no well-known name, so it is found by
# PID; and it holds *more than one* system-bus connection, because the BLE
# transport opens its own alongside the one logind already uses. So every
# connection this PID owns is a candidate and only one of them answers here.
NAME=""
for n in $(busctl --system list --no-pager 2>/dev/null | awk -v p="$PID" '$2==p {print $1}'); do
  if busctl --system call "$n" /org/acrylius/gatt \
    org.freedesktop.DBus.ObjectManager GetManagedObjects >/dev/null 2>&1; then
    NAME=$n
    break
  fi
done
if [ -z "$NAME" ]; then
  # The ordinary outcome, and not a failure. The system bus denies method
  # calls between unprivileged connections, so only bluetoothd — which is
  # root — can read this tree back, and that it did so is exactly what the
  # "GATT application registered" line above proves: bluetoothd validates an
  # application before accepting it, and rejects a malformed one.
  #
  # The shape and the flags are pinned instead by the unit tests in
  # crates/acrylius-linux/src/ble.rs, which need no bus at all.
  skip "the tree is not readable without root; bluetoothd already validated it"
else
  ADV=$(busctl --system call "$NAME" /org/acrylius/adv0 \
    org.freedesktop.DBus.Properties GetAll s org.bluez.LEAdvertisement1 2>/dev/null)
  echo "$ADV" | grep -q "$SERVICE"
  check $? "the service UUID is in the advertisement, not merely in the database"

  # Without this bluetoothd emits no Flags element at all, and an advertisement
  # with flags 0x00 is one iOS will not surface.
  echo "$ADV" | grep -q 'Discoverable.*true'
  check $? "the advertisement is discoverable"

  echo "$ADV" | grep -q 'peripheral'
  check $? "and connectable, by being type peripheral"

  TREE=$(busctl --system call "$NAME" /org/acrylius/gatt \
    org.freedesktop.DBus.ObjectManager GetManagedObjects 2>/dev/null)
  for u in $SERVICE $IDENTITY $RX $TX; do
    echo "$TREE" | grep -q "$u"
    check $? "the tree publishes $u"
  done

  # An encrypt-* or secure-* flag is what raises an iOS pairing dialog, and
  # every recurring iOS/BlueZ GATT failure in the wild is a pairing failure.
  # Noise is the security boundary here, not the link layer.
  #
  # Guarded on the tree being there at all: "we read nothing, and nothing we
  # read asks for encryption" is a pass this check must never report.
  if [ -z "$TREE" ]; then
    skip "the tree came back empty; cannot judge the flags"
    fail=1
  else
    if echo "$TREE" | grep -qE 'encrypt|secure'; then R=1; else R=0; fi
    check $R "no characteristic asks for encryption"
  fi
fi

kill $PID 2>/dev/null || true
for i in $(seq 1 50); do mine >/dev/null || break; sleep 0.1; done

echo
echo "### a daemon that is not allowed to advertise"
# A radio that announces the machine continuously is the owner's call, so the
# switch has to actually switch something off.
cat > $D/off/config.toml <<CFG
name = "m2-ble-off"

[ble]
enabled = false
CFG
"$BIN/acryliusd" --state $D/off --port $PORT --config $D/off/config.toml > $D/off.log 2>&1 &
for i in $(seq 1 100); do "$BIN/acryliusctl" --state $D/off status >/dev/null 2>&1 && break; sleep 0.1; done
sleep 1.5
IDLE=$(prop org.bluez.LEAdvertisingManager1 ActiveInstances | awk '{print $2}')
[ "${IDLE:-0}" = "0" ]; check $? "[ble] enabled = false really does stay off the air"
cleanup

echo
echo "### with a phone"
if [ "${ACRYLIUS_BLE_PHONE:-0}" != "1" ]; then
  cat <<'MANUAL'
  skip  needs a paired iPhone and a person. Run it deliberately:

          ACRYLIUS_BLE_PHONE=1 ./scripts/m2-ble-acceptance.sh

MANUAL
else
  cat <<'MANUAL'
  Pair over the LAN first — pairing never runs over Bluetooth — then turn
  Wi-Fi off on the phone and work down this list. Every line is something
  only a phone can answer:

    [ ] the Bluetooth screen says "advertises the acrylius service" in green
    [ ] with Wi-Fi off, the desktop still appears and shows a fingerprint
    [ ] `session query` from the phone answers
    [ ] sending a file over BLE is refused with a clear message, not a hang
    [ ] turning Wi-Fi back on does not lose the desktop, and TCP takes over
    [ ] force-quit the app, reopen it, and it reconnects without a second try

  The last one is the regression this milestone actually shipped a fix for:
  a peripheral stops advertising while something is connected to it, so a
  scan alone never finds a desktop iOS is still holding open.
MANUAL
fi

echo
[ $fail = 0 ] && echo "M2 BLE acceptance passed" || echo "M2 BLE acceptance FAILED"
exit $fail
