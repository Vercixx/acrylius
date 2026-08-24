#!/usr/bin/env bash
#
# Build and run the Swift side of the FFI seam **on Linux**.
#
# This works, and it matters more than it looks: it means the Swift that talks
# to the core can be written and tested here, with no Mac and no 15-minute CI
# round trip. Only SwiftUI views and Network.framework genuinely need macOS.
#
# Run from the repo root.
set -euo pipefail

OUT=${OUT:-target/swift}
LIB=target/debug

cargo build -p acrylius-ffi
mkdir -p "$OUT"
cargo run -q -p acrylius-ffi --bin uniffi-bindgen -- \
    generate --library "$LIB/libacrylius_ffi.so" --language swift --out-dir "$OUT"

swiftc -o "$OUT/smoke" swift/smoke/main.swift "$OUT/acrylius_ffi.swift" \
    -Xcc -fmodule-map-file="$OUT/acrylius_ffiFFI.modulemap" -I "$OUT" \
    -L "$LIB" -lacrylius_ffi

LD_LIBRARY_PATH="$PWD/$LIB" "$OUT/smoke"
