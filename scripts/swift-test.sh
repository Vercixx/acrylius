#!/usr/bin/env bash
#
# Build and run the Swift host runtime's tests on Linux: two CoreRuntimes over an in-memory transport pair.
# Run from the repo root. Darwin-only files guard themselves with `#if canImport(...)`.
set -euo pipefail

OUT=${OUT:-target/swift}
LIB=target/debug

cargo build -p acrylius-ffi
mkdir -p "$OUT"
cargo run -q -p acrylius-ffi --bin uniffi-bindgen -- \
    generate --library "$LIB/libacrylius_ffi.so" --language swift --out-dir "$OUT"

# -swift-version 6 and complete concurrency checking match the Xcode target;
# catching concurrency errors here beats a fifteen-minute macOS run.
swiftc -o "$OUT/runtime-tests" \
    -swift-version 6 \
    -strict-concurrency=complete \
    "$OUT/acrylius_ffi.swift" \
    ios/Acrylius/Runtime/*.swift \
    swift/tests/main.swift \
    -Xcc -fmodule-map-file="$OUT/acrylius_ffiFFI.modulemap" -I "$OUT" \
    -L "$LIB" -lacrylius_ffi

LD_LIBRARY_PATH="$PWD/$LIB" "$OUT/runtime-tests"
