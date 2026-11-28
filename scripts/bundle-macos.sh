#!/bin/sh
# Build "AppSwitchClaude.app" for both Apple Silicon and Intel Macs, and pack it into a
# disk image that can be handed to another Mac.
#
#   ./scripts/bundle-macos.sh
#   open target/release/bundle/AppSwitchClaude.app     # run it on this Mac
#   open dist/macos/AppSwitchClaude.dmg                # the file to pass on
set -eu
cd "$(dirname "$0")/.."

# The oldest macOS that runs Apple Silicon binaries; also written to Info.plist below.
export MACOSX_DEPLOYMENT_TARGET=11.0

# Source file paths are compiled into the program (they appear in panic messages), and on
# this machine they start with the home folder, which carries the user's name. The app is
# built to be handed to others, so replace those prefixes. The separator is the one
# CARGO_ENCODED_RUSTFLAGS expects; unlike RUSTFLAGS it copes with spaces in a path.
SEP=$(printf '\037')
export CARGO_ENCODED_RUSTFLAGS="--remap-path-prefix=$HOME=~$SEP--remap-path-prefix=$PWD=."

TARGETS="aarch64-apple-darwin x86_64-apple-darwin"
# shellcheck disable=SC2086 # the list is meant to be split into words
rustup target add $TARGETS
for target in $TARGETS; do
    cargo build --release --target "$target"
done

APP="target/release/bundle/AppSwitchClaude.app"
VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
lipo -create -output "$APP/Contents/MacOS/AppSwitchClaude" \
    target/aarch64-apple-darwin/release/app-switch-claude \
    target/x86_64-apple-darwin/release/app-switch-claude
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>AppSwitchClaude</string>
    <key>CFBundleDisplayName</key>
    <string>Chuyển tài khoản Claude</string>
    <key>CFBundleIdentifier</key>
    <string>local.app-switch-claude</string>
    <key>CFBundleExecutable</key>
    <string>AppSwitchClaude</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>$VERSION</string>
    <key>CFBundleVersion</key>
    <string>$VERSION</string>
    <key>LSMinimumSystemVersion</key>
    <string>$MACOSX_DEPLOYMENT_TARGET</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
PLIST

# An ad-hoc signature: Apple Silicon refuses to run unsigned code. It is not a Developer ID,
# so another Mac that downloads the app still asks its user to allow it once.
codesign --force --sign - "$APP"
echo "Built $APP"

# The disk image: the app next to a shortcut to /Applications, to drag one onto the other.
DMG="dist/macos/AppSwitchClaude.dmg"
STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
ditto "$APP" "$STAGE/AppSwitchClaude.app"
ln -s /Applications "$STAGE/Applications"
mkdir -p "$(dirname "$DMG")"
hdiutil create -quiet -ov -format UDZO -volname AppSwitchClaude -srcfolder "$STAGE" "$DMG"
echo "Built $DMG"
