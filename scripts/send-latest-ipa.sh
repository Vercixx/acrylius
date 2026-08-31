#!/usr/bin/env bash
#
# Pull the newest iOS build off CI and offer it to the phone.
#
#   ./scripts/send-latest-ipa.sh                the newest successful build
#   ./scripts/send-latest-ipa.sh --run 1234     one particular run
#   ./scripts/send-latest-ipa.sh --device <id>  when more than one phone is paired
#   ./scripts/send-latest-ipa.sh --wait 0       fail at once instead of waiting
#
# The app must be open on the phone (no background execution) and on the
# same Wi-Fi; the script waits for both, then blocks until Accept is tapped.
set -euo pipefail

WORKFLOW=ios-ipa.yml
ARTIFACT=acrylius-unsigned-ipa
RUN=""
DEVICE=""
WAIT=120
KEEP=0

while [ $# -gt 0 ]; do
    case "$1" in
        --run) RUN="${2:-}"; shift 2 ;;
        --device) DEVICE="${2:-}"; shift 2 ;;
        --wait) WAIT="${2:-}"; shift 2 ;;
        --keep) KEEP=1; shift ;;
        -h|--help) sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done

need() { command -v "$1" >/dev/null 2>&1 || { echo "$1 is not installed" >&2; exit 1; }; }
need gh
need acryliusctl

gh auth status >/dev/null 2>&1 || {
    echo "gh is not logged in. Run: gh auth login" >&2
    exit 1
}

# ------------------------------------------------------------------- the build

# read exits nonzero at EOF; || true keeps set -e from aborting before the check below.
if [ -z "$RUN" ]; then
    read -r RUN SHA BRANCH WHEN <<EOF || true
$(gh run list --workflow "$WORKFLOW" --status success --limit 1 \
    --json databaseId,headSha,headBranch,createdAt \
    --jq '.[] | "\(.databaseId) \(.headSha[0:7]) \(.headBranch) \(.createdAt)"')
EOF
    [ -n "${RUN:-}" ] || {
        echo "no successful $WORKFLOW run to download" >&2
        exit 1
    }
else
    read -r SHA BRANCH WHEN <<EOF || true
$(gh run view "$RUN" --json headSha,headBranch,createdAt \
    --jq '"\(.headSha[0:7]) \(.headBranch) \(.createdAt)"' 2>/dev/null)
EOF
    [ -n "${SHA:-}" ] || { echo "no run $RUN, or it is not readable" >&2; exit 1; }
fi
echo "run $RUN  $SHA  $BRANCH  $WHEN"

# Under the cache dir, not /tmp: the unit's PrivateTmp=yes gives the daemon a
# different /tmp than this shell's.
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/acrylius"
mkdir -p "$CACHE"
WORK=$(mktemp -d "$CACHE/send-XXXXXX")
cleanup() { [ "$KEEP" = 1 ] || rm -rf "$WORK"; }
trap cleanup EXIT

gh run download "$RUN" --name "$ARTIFACT" --dir "$WORK" >/dev/null

# gh run download unzips automatically; the zip branch covers an artifact
# fetched another way (web UI, REST API), which arrives zipped.
IPA=$(find "$WORK" -type f -name '*.ipa' | head -1)
if [ -z "$IPA" ]; then
    ZIP=$(find "$WORK" -type f -name '*.zip' | head -1)
    if [ -n "$ZIP" ]; then
        need unzip
        unzip -q "$ZIP" -d "$WORK/unpacked"
        IPA=$(find "$WORK/unpacked" -type f -name '*.ipa' | head -1)
    fi
fi
[ -n "$IPA" ] || { echo "no .ipa in artifact $ARTIFACT of run $RUN" >&2; exit 1; }

# Named for the commit: every build is otherwise `acrylius-unsigned.ipa`.
NAMED="$WORK/acrylius-$SHA.ipa"
mv "$IPA" "$NAMED"
SIZE=$(du -h "$NAMED" | cut -f1)
echo "built  $(basename "$NAMED")  $SIZE"

# ------------------------------------------------------------------ the device

# A device line is unindented, with an indented fingerprint below it; names
# may contain spaces, so the id is the first field and state is the last.
ios_devices() {
    acryliusctl device list 2>/dev/null | awk '!/^[[:space:]]/ && /\(ios\)/ { print $1, $NF }'
}

if [ -z "$DEVICE" ]; then
    COUNT=$(ios_devices | wc -l)
    if [ "$COUNT" = 0 ]; then
        echo "no iPhone is paired. Pair one first: acryliusctl pair" >&2
        exit 1
    elif [ "$COUNT" != 1 ]; then
        echo "more than one iPhone is paired; name one with --device:" >&2
        ios_devices | sed 's/^/  /' >&2
        exit 1
    fi
    DEVICE=$(ios_devices | awk '{print $1}')
fi

state() { ios_devices | awk -v d="$DEVICE" '$1 == d { print $2 }'; }

# The phone dials out and is never dialled; this waits for a person to open the app.
if [ "$(state)" != "reachable" ]; then
    if [ "$WAIT" = 0 ]; then
        echo "$DEVICE is not connected" >&2
        exit 1
    fi
    echo "waiting up to ${WAIT}s for the phone — open Acrylius on it"
    for _ in $(seq 1 "$WAIT"); do
        [ "$(state)" = "reachable" ] && break
        sleep 1
    done
fi
if [ "$(state)" != "reachable" ]; then
    echo "$DEVICE never connected. Is the app open, and on this Wi-Fi?" >&2
    exit 1
fi

# ------------------------------------------------------------------- the offer

echo "offering to $DEVICE — tap Accept on the phone"
acryliusctl file send "$DEVICE" "$NAMED"
echo "then open it from Files to install."
