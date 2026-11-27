#!/bin/sh
# Build "AppSwitchClaude.app", a double-clickable macOS app, into target/release/bundle/.
#
#   ./scripts/bundle-macos.sh
#   open target/release/bundle/AppSwitchClaude.app
set -eu
cd "$(dirname "$0")/.."

cargo build --release

APP="target/release/bundle/AppSwitchClaude.app"
VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp target/release/app-switch-claude "$APP/Contents/MacOS/AppSwitchClaude"
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
    <string>11.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
PLIST

# An ad-hoc signature: enough for macOS to run the app on the machine that built it.
codesign --force --sign - "$APP"
echo "Built $APP"
