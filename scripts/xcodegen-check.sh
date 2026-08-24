#!/usr/bin/env bash
#
# Validate ios/project.yml **on Linux**, in seconds rather than a CI round trip.
#
# XcodeGen is a Swift package with no Darwin-only dependencies, so it builds and
# runs here. The .xcodeproj it produces is thrown away — this only proves the
# manifest is well-formed and that every source path it names exists. Xcode is
# still the only thing that can *build* the result.
#
# First run clones and builds XcodeGen (~90s); afterwards it is cached.
set -euo pipefail

VERSION=${XCODEGEN_VERSION:-2.44.1}
CACHE=${XCODEGEN_CACHE:-target/xcodegen}
BIN="$CACHE/.build/release/xcodegen"

if [ ! -x "$BIN" ]; then
    echo "building XcodeGen $VERSION (once)…"
    rm -rf "$CACHE"
    git clone --depth 1 --branch "$VERSION" https://github.com/yonaskolb/XcodeGen.git "$CACHE"
    (cd "$CACHE" && swift build -c release)
fi

# The manifest names the generated bindings; stand in for them so the path check
# passes without having to run the whole Rust build first.
mkdir -p ios/Generated && touch ios/Generated/acrylius_ffi.swift

"$PWD/$BIN" generate --spec ios/project.yml --project ios
echo
echo "sources picked up:"
grep -oE '[A-Za-z_]+\.swift' ios/Acrylius.xcodeproj/project.pbxproj | sort -u | sed 's/^/  /'

rm -rf ios/Acrylius.xcodeproj
echo
echo "project.yml is valid."
