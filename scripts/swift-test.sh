#!/usr/bin/env bash
#
# Build and run the Swift host runtime's tests **on Linux**.
#
# Two CoreRuntimes joined by an in-memory transport pair, connect and ping —
# the exact code the iOS app runs, minus SwiftUI and Network.framework. Files
# that need Darwin guard themselves with `#if canImport(...)`, so the same
# sources compile here and in Xcode.
#
# Run from the repo root.
set -euo pipefail

OUT=${OUT:-target/swift}
LIB=target/debug

cargo build -p acrylius-ffi
mkdir -p "$OUT"
cargo run -q -p acrylius-ffi --bin uniffi-bindgen -- \
    generate --library "$LIB/libacrylius_ffi.so" --language swift --out-dir "$OUT"

# -swift-version 6 and complete concurrency checking match what the Xcode
# target uses. The Darwin-only files are excluded by their own #if guards, but
# CoreRuntime and Ports are the parts where concurrency is actually hard — so
# catching those errors here beats finding them in a fifteen-minute macOS run.
swiftc -o "$OUT/runtime-tests" \
    -swift-version 6 \
    -strict-concurrency=complete \
    "$OUT/acrylius_ffi.swift" \
    ios/Acrylius/Runtime/*.swift \
    swift/tests/main.swift \
    -Xcc -fmodule-map-file="$OUT/acrylius_ffiFFI.modulemap" -I "$OUT" \
    -L "$LIB" -lacrylius_ffi

LD_LIBRARY_PATH="$PWD/$LIB" "$OUT/runtime-tests"
