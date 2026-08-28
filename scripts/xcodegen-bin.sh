#!/usr/bin/env bash
#
# Print the path to a pinned XcodeGen, building it once if it is not cached.
#
# Both callers need the same generator. `scripts/xcodegen-check.sh` validates the
# manifest on Linux; the macOS job generates the project it actually builds. If
# those two ran different XcodeGens, the check would stop meaning anything about
# the thing CI ships, and the difference would only ever show up as a macOS-only
# failure.
#
# `brew install xcodegen` was what the workflow used, and it is not a version:
# it is whatever bottle that runner image happens to carry, which changes under
# you when the image is refreshed. The clone below is pinned to a tag and then
# checked against the commit that tag pointed at, because a tag is a name
# somebody else can repoint.
#
# Progress goes to stderr. Stdout is the path and nothing else, so callers can
# capture it.
set -euo pipefail

VERSION=${XCODEGEN_VERSION:-2.44.1}
# The commit that tag pointed at when it was pinned. See the check below.
COMMIT=${XCODEGEN_COMMIT:-21ac9944b0ab546a07422dbed86f33dd2ebd76f8}
CACHE=${XCODEGEN_CACHE:-target/xcodegen}
BIN="$CACHE/.build/release/xcodegen"

if [ ! -x "$BIN" ]; then
    echo "building XcodeGen $VERSION (once)…" >&2
    rm -rf "$CACHE"
    git clone --depth 1 --branch "$VERSION" \
        https://github.com/yonaskolb/XcodeGen.git "$CACHE" >&2
    got="$(cd "$CACHE" && git rev-parse HEAD)"
    if [ "$got" != "$COMMIT" ]; then
        echo "XcodeGen $VERSION is $got, expected $COMMIT" >&2
        echo "If the bump is deliberate, update XCODEGEN_COMMIT in this script." >&2
        rm -rf "$CACHE"
        exit 1
    fi
    (cd "$CACHE" && swift build -c release) >&2
fi

# Absolute, because callers run it from directories other than this one.
# XCODEGEN_CACHE may already be absolute: CI puts it outside target/, which
# Swatinem/rust-cache treats as its own and prunes.
case "$BIN" in
    /*) printf '%s\n' "$BIN" ;;
    *)  printf '%s\n' "$PWD/$BIN" ;;
esac
