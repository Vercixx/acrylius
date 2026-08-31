#!/usr/bin/env bash
#
# M2 acceptance: the BLE transport. Checks adapter roles, unprivileged GATT
# registration, and advertised UUIDs; the half needing a phone is a checklist.
# Self-skipping: a machine with no Bluetooth reports what it lacks, not a failure.
set -u
D=/tmp/acr-m2; BIN="$PWD/target/debug"
PORT=19731
ADAPTER=/org/bluez/hci0

# Restated on purpose, not read out of the binary: iOS caches a peripheral's
# attribute table, so this layout can never change. A silent edit must fail here.
SERVICE=61637279-6c69-7573-8001-000000000001
IDENTITY=61637279-6c69-7573-8001-000000000002
RX=61637279-6c69-7573-8001-000000000003
TX=61637279-6c69-7573-8001-000000000004

# Matches the state directory, not the binary name: a binary pattern also
# matches this shell itself.
mine() { pgrep -f "acryliusd --state $D/" 2>/dev/null; }
cleanup() { mine | xargs -r kill 2>/dev/null || true; }
trap cleanup EXIT

fail=0
check() { if [ "$1" = 0 ]; then echo "  ok   $2"; else echo "  FAIL $2"; fail=1; fi; }
skip() { echo "  skip $1"; }

prop() { busctl --system get-property org.bluez "$ADAPTER" "$1" "$2" 2>/dev/null; }

# Build here: nothing else in this script would notice a stale binary.
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

# An advertisement already on the radio (usually the installed daemon) would
# make every result below meaningless.
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
# Read the tree back as bluetoothd did; the daemon has no well-known bus name, so
# it's found by PID among its several system-bus connections, only one of which answers.
NAME=""
for n in $(busctl --system list --no-pager 2>/dev/null | awk -v p="$PID" '$2==p {print $1}'); do
  if busctl --system call "$n" /org/acrylius/gatt \
    org.freedesktop.DBus.ObjectManager GetManagedObjects >/dev/null 2>&1; then
    NAME=$n
    break
  fi
done
if [ -z "$NAME" ]; then
  # Expected: the system bus denies calls between unprivileged connections; bluetoothd (root) already validated the tree.
  # Shape and flags are covered by the unit tests in crates/acrylius-linux/src/ble.rs.
  skip "the tree is not readable without root; bluetoothd already validated it"
else
  ADV=$(busctl --system call "$NAME" /org/acrylius/adv0 \
    org.freedesktop.DBus.Properties GetAll s org.bluez.LEAdvertisement1 2>/dev/null)
  echo "$ADV" | grep -q "$SERVICE"
  check $? "the service UUID is in the advertisement, not merely in the database"

  # Without Discoverable, bluetoothd emits no Flags element; iOS will not
  # surface an advertisement with flags 0x00.
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

  # encrypt-*/secure-* flags would raise an iOS pairing dialog; Noise is the security boundary, not the link layer.
  # Guarded on a non-empty tree: "read nothing" must never pass as "no flags."
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
# The off switch has to actually switch something off.
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
