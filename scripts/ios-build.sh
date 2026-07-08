#!/bin/sh
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
IOS="$ROOT/crates/fsync-ios/ios"
HEADERS="$ROOT/target/ios-headers"

rustup target add aarch64-apple-ios aarch64-apple-ios-sim

cargo build -p fsync-ios --release --target aarch64-apple-ios
cargo build -p fsync-ios --release --target aarch64-apple-ios-sim

cargo run -p fsync-ios --bin uniffi-bindgen-swift -- \
    generate --library "$ROOT/target/aarch64-apple-ios/release/libfsync_ios.a" \
    --language swift --no-format --out-dir "$IOS/Generated"

rm -rf "$HEADERS" "$IOS/Fsync.xcframework"
mkdir -p "$HEADERS"
cp "$IOS/Generated/fsyncFFI.h" "$HEADERS/"
cp "$IOS/Generated/fsyncFFI.modulemap" "$HEADERS/module.modulemap"
xcodebuild -create-xcframework \
    -library "$ROOT/target/aarch64-apple-ios/release/libfsync_ios.a" -headers "$HEADERS" \
    -library "$ROOT/target/aarch64-apple-ios-sim/release/libfsync_ios.a" -headers "$HEADERS" \
    -output "$IOS/Fsync.xcframework"

xcodegen generate --spec "$IOS/project.yml" --project "$IOS"
xcodebuild -project "$IOS/Filestash.xcodeproj" -scheme Filestash \
    -destination 'generic/platform=iOS Simulator' build
