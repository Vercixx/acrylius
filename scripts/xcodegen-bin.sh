#!/usr/bin/env bash
#
# Print the path to a pinned XcodeGen, building it once if not cached; the tag is checked against its commit since tags can be repointed.
# Progress goes to stderr; stdout is only the path.
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

# Absolute: callers run it from other directories. XCODEGEN_CACHE may already
# be absolute — CI puts it outside target/, which rust-cache prunes.
case "$BIN" in
    /*) printf '%s\n' "$BIN" ;;
    *)  printf '%s\n' "$PWD/$BIN" ;;
esac
