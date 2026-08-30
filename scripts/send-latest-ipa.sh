#!/usr/bin/env bash
#
# Pull the newest iOS build off CI and offer it to the phone.
#
# The last mile of the sideload loop. CI builds an unsigned IPA, SideStore signs
# it on the device with a free Apple ID, and the part in between — getting the
# file onto the phone — is what this removes.
#
#   ./scripts/send-latest-ipa.sh                the newest successful build
#   ./scripts/send-latest-ipa.sh --run 1234     one particular run
#   ./scripts/send-latest-ipa.sh --device <id>  when more than one phone is paired
#   ./scripts/send-latest-ipa.sh --wait 0       fail at once instead of waiting
#
# Two things have to be true on the phone, and neither can be arranged from
# here: the app has to be open, because there is no background execution yet,
# and it has to be on the same Wi-Fi, because the file moves over a side channel
# the desktop dials. The script waits for the first and says so plainly about
# the second.
#
# It then blocks until someone taps Accept. That is not a hang: an offer is a
# question, and `acryliusctl file send`waits for the answer.
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
        -h|--help) sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done

need() { command -v "$1" >/dev/null 2>&1 || { echo "$1 is not installed" >&2; exit 1; }; }
need gh
need acryliusctl

# Fail here rather than three steps in with a confusing API error.
gh auth status >/dev/null 2>&1 || {
    echo "gh is not logged in. Run: gh auth login" >&2
    exit 1
}

# ------------------------------------------------------------------- the build

# `|| true` on each read: at end of input `read` reports failure, and under
# `set -e` that would end the script one line before the message explaining
# why. The emptiness is checked straight after instead.
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

# Under the cache directory, not /tmp, and that is load-bearing rather than
# tidiness. `acryliusctl file send`hands the daemon a path and the daemon is what
# opens the file — and the unit sets `PrivateTmp=yes`, so the daemon's /tmp is
# not this shell's. A file downloaded to /tmp is one it cannot see, and the
# error it reports is "No such file or directory" against a path that plainly
# exists, which is a confusing thing to be told.
#
# `ProtectHome=read-only` leaves everything under $HOME readable, which is all
# a send needs.
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/acrylius"
mkdir -p "$CACHE"
WORK=$(mktemp -d "$CACHE/send-XXXXXX")
cleanup() { [ "$KEEP" = 1 ] || rm -rf "$WORK"; }
trap cleanup EXIT

gh run download "$RUN" --name "$ARTIFACT" --dir "$WORK" >/dev/null

# `gh run download` unzips the artifact for you, so this is normally already an
# IPA. The zip branch is here because downloading the same artifact any other
# way — the web UI, the REST API — hands you one, and a file that arrives on a
# phone as a .zip is a build nobody can install.
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

# Named for the commit it came from. Every build is otherwise
# `acrylius-unsigned.ipa`, and a phone full of those is a phone where you cannot
# tell which one you are about to install.
NAMED="$WORK/acrylius-$SHA.ipa"
mv "$IPA" "$NAMED"
SIZE=$(du -h "$NAMED" | cut -f1)
echo "built  $(basename "$NAMED")  $SIZE"

# ------------------------------------------------------------------ the device

# A device line is unindented; the fingerprint under it is not. The name may
# have spaces in it, so the id is the first field and the state is the last.
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

# The phone dials out and is never dialled, so nothing here can wake it. What
# this waits for is a person opening the app.
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
