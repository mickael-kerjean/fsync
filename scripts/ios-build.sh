#!/bin/sh
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
IOS="$ROOT/crates/fdrive-ios/ios"
HEADERS="$ROOT/target/ios-headers"

rustup target add aarch64-apple-ios aarch64-apple-ios-sim

cargo build -p fdrive-ios --release --target aarch64-apple-ios
cargo build -p fdrive-ios --release --target aarch64-apple-ios-sim

cargo run -p fdrive-ios --bin uniffi-bindgen-swift -- \
    generate --library "$ROOT/target/aarch64-apple-ios/release/libfdrive_ios.a" \
    --language swift --no-format --out-dir "$IOS/Generated"

rm -rf "$HEADERS" "$IOS/Fdrive.xcframework"
mkdir -p "$HEADERS"
cp "$IOS/Generated/fdriveFFI.h" "$HEADERS/"
cp "$IOS/Generated/fdriveFFI.modulemap" "$HEADERS/module.modulemap"
xcodebuild -create-xcframework \
    -library "$ROOT/target/aarch64-apple-ios/release/libfdrive_ios.a" -headers "$HEADERS" \
    -library "$ROOT/target/aarch64-apple-ios-sim/release/libfdrive_ios.a" -headers "$HEADERS" \
    -output "$IOS/Fdrive.xcframework"

xcodegen generate --spec "$IOS/project.yml" --project "$IOS"
xcodebuild -project "$IOS/Filestash.xcodeproj" -scheme Filestash \
    -destination 'generic/platform=iOS Simulator' ARCHS=arm64 build
