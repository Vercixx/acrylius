#!/usr/bin/env bash
#
# Install or update acrylius for the current user.
#
# Run it again whenever you have pulled: it replaces the binaries, adds settings
# a newer version introduced without touching what you have written, and
# restarts the service. Nothing here needs root, and nothing here asks for it.
#
#   ./scripts/install.sh            build and install
#   ./scripts/install.sh --no-build use the binaries already in target/release
#
set -euo pipefail

BIN_DIR="${HOME}/.local/bin"
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
UNIT="acryliusd.service"
BUILD=1

for arg in "$@"; do
    case "$arg" in
        --no-build) BUILD=0 ;;
        -h|--help) sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown option: $arg" >&2; exit 2 ;;
    esac
done

say()  { printf '  %s\n' "$*"; }
step() { printf '\n%s\n' "$*"; }
warn() { printf '  ! %s\n' "$*" >&2; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }

# --- what we are working with ------------------------------------------------

[ "$(id -u)" -ne 0 ] || die "do not run this as root; acrylius runs as you"
command -v systemctl >/dev/null || die "no systemctl; this expects a systemd user session"
cd "$(git rev-parse --show-toplevel 2>/dev/null || dirname "$(dirname "$(readlink -f "$0")")")"

# An update is "the binary is already where we put it". Not the unit and not the
# config: either can be absent on a machine that was installed before they
# existed, and treating that as a fresh install would overwrite a config.
UPDATE=0
[ -x "$BIN_DIR/acryliusd" ] && UPDATE=1

if [ "$UPDATE" -eq 1 ]; then
    step "Updating an existing installation"
    say "$($BIN_DIR/acryliusd --version 2>/dev/null || echo 'installed version unknown')"
else
    step "Installing"
fi

# --- build -------------------------------------------------------------------

if [ "$BUILD" -eq 1 ]; then
    step "Building"
    command -v cargo >/dev/null || die "no cargo; install Rust or pass --no-build"
    cargo build --release --bin acryliusd --bin acryliusctl
fi
for b in acryliusd acryliusctl; do
    [ -x "target/release/$b" ] || die "target/release/$b is missing; build first"
done

# --- binaries ----------------------------------------------------------------

step "Binaries"
mkdir -p "$BIN_DIR"
for b in acryliusd acryliusctl; do
    # Copy beside and rename. Writing over a running binary fails with ETXTBSY,
    # and renaming onto it does not — the running process keeps the inode it
    # already opened and the next start gets the new one.
    install -m 755 "target/release/$b" "$BIN_DIR/.$b.new"
    mv -f "$BIN_DIR/.$b.new" "$BIN_DIR/$b"
    say "$BIN_DIR/$b"
done

case ":${PATH}:" in
    *":${BIN_DIR}:"*) ;;
    *) warn "$BIN_DIR is not on your PATH; add it or use the full path" ;;
esac

# --- config ------------------------------------------------------------------

step "Config"
CONFIG="$("$BIN_DIR/acryliusd" config path)"
if [ -e "$CONFIG" ]; then
    # Adds what is missing and leaves everything else exactly as written,
    # comments included. Printing what it did matters more than it looks: a
    # setting nobody can see is a setting nobody uses.
    "$BIN_DIR/acryliusd" config update | sed 's/^/  /'
else
    "$BIN_DIR/acryliusd" config init | sed 's/^/  /'
fi
if CHECK="$("$BIN_DIR/acryliusd" config check 2>&1)"; then
    # Where files land is worth saying out loud. It is the one setting whose
    # being wrong looks like a broken feature rather than a wrong setting: files
    # arrive, and nobody can find them.
    echo "$CHECK" | grep -E '^  (files|NOTE)' | sed 's/^  /  /'
else
    warn "the config does not parse; the service will refuse to start. Run: acryliusd config check"
fi

# --- service -----------------------------------------------------------------

step "Service"
mkdir -p "$UNIT_DIR"
install -m 644 "systemd/$UNIT" "$UNIT_DIR/$UNIT"
say "$UNIT_DIR/$UNIT"

# The unit runs under ProtectHome=read-only, so anything the daemon writes to
# has to be named in ReadWritePaths= or it fails with "read-only file system"
# at the moment it is used — which for a download directory means halfway
# through receiving a file.
#
# The shipped unit cannot name it: where downloads go is a config setting, and
# on this desktop that folder may be called Загрузки or Téléchargements or
# anything else. So the daemon is asked, and the answer becomes a drop-in.
# Rewritten on every run, so moving the directory and re-running is enough.
DROPIN_DIR="$UNIT_DIR/$UNIT.d"
mkdir -p "$DROPIN_DIR"
{
    echo "# Written by scripts/install.sh from share.directory. Edit the config,"
    echo "# not this file, and run the installer again."
    echo "[Service]"
    "$BIN_DIR/acryliusd" config writable-paths | while read -r p; do
        [ -n "$p" ] || continue
        # A leading '-' so a directory that is not there yet is not a reason
        # for the whole service to refuse to start.
        echo "ReadWritePaths=-$p"
    done
} > "$DROPIN_DIR/writable.conf"
chmod 644 "$DROPIN_DIR/writable.conf"
grep '^ReadWritePaths=' "$DROPIN_DIR/writable.conf" | sed 's/^ReadWritePaths=-*/  may write to /'

systemctl --user daemon-reload

# A daemon someone started by hand holds the port, and the service would then
# fail to bind with an error that says nothing about why.
STRAY="$(pgrep -u "$(id -u)" -x acryliusd 2>/dev/null | while read -r pid; do
    systemctl --user status "$UNIT" 2>/dev/null | grep -q "PID: $pid" || echo "$pid"
done || true)"
if [ -n "${STRAY:-}" ]; then
    warn "acryliusd is already running outside systemd (pid: $(echo "$STRAY" | tr '\n' ' '))"
    warn "stop it first, or the service will fail to bind the port"
fi

systemctl --user reset-failed "$UNIT" 2>/dev/null || true
if [ "$UPDATE" -eq 1 ] && systemctl --user is-enabled "$UNIT" >/dev/null 2>&1; then
    systemctl --user restart "$UNIT"
    say "restarted"
else
    systemctl --user enable --now "$UNIT"
    say "enabled and started"
fi

sleep 1
if systemctl --user is-active "$UNIT" >/dev/null 2>&1; then
    say "running"
else
    warn "not running. journalctl --user -u $UNIT -n 30"
fi

# --- what is left to you -----------------------------------------------------

PORT="$(sed -n 's/^port *= *\([0-9]\+\).*/\1/p' "$CONFIG" 2>/dev/null | head -1)"
PORT="${PORT:-1971}"

step "Still yours to do"
say "Open the firewall for pairing and discovery:"
say "  ufw:       sudo ufw allow $PORT/tcp && sudo ufw allow 5353/udp"
say "  firewalld: sudo firewall-cmd --add-port=$PORT/tcp --add-port=5353/udp --permanent && sudo firewall-cmd --reload"
if [ "$UPDATE" -eq 0 ]; then
    say ""
    say "Then pair a phone:"
    say "  acryliusctl pair"
fi
say ""
say "acryliusctl status        what this machine offers"
say "journalctl --user -u $UNIT -f"
