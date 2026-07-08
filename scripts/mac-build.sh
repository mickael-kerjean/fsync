#!/bin/sh
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
MAC="$ROOT/crates/fsync-mac/macos"
HEADERS="$ROOT/target/mac-headers"

rustup target add aarch64-apple-darwin x86_64-apple-darwin

cargo build -p fsync-mac --release --target aarch64-apple-darwin
cargo build -p fsync-mac --release --target x86_64-apple-darwin

cargo run -p fsync-mac --bin uniffi-bindgen-swift -- \
    generate --library "$ROOT/target/aarch64-apple-darwin/release/libfsync_mac.a" \
    --language swift --no-format --out-dir "$MAC/Generated"

rm -rf "$HEADERS" "$MAC/Fsync.xcframework" "$ROOT/target/libfsync_mac_universal.a"
mkdir -p "$HEADERS"
cp "$MAC/Generated/fsyncFFI.h" "$HEADERS/"
cp "$MAC/Generated/fsyncFFI.modulemap" "$HEADERS/module.modulemap"
lipo -create \
    "$ROOT/target/aarch64-apple-darwin/release/libfsync_mac.a" \
    "$ROOT/target/x86_64-apple-darwin/release/libfsync_mac.a" \
    -output "$ROOT/target/libfsync_mac_universal.a"
xcodebuild -create-xcframework \
    -library "$ROOT/target/libfsync_mac_universal.a" -headers "$HEADERS" \
    -output "$MAC/Fsync.xcframework"

xcodegen generate --spec "$MAC/project.yml" --project "$MAC"
xcodebuild -project "$MAC/Filestash.xcodeproj" -scheme Filestash \
    -destination 'platform=macOS' build
