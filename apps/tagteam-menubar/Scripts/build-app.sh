#!/bin/bash
# Builds and signs a self-contained native menu bar app bundle without external dependencies.
# The script writes only within this app directory and uses an ad-hoc local code signature.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APP_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BUILD_DIR="$APP_ROOT/.build"
APP_BUNDLE="$APP_ROOT/dist/TagTeam.app"
CONTENTS="$APP_BUNDLE/Contents"

swift build --package-path "$APP_ROOT" --configuration release --product TagTeamMenuBar
rm -rf "$APP_BUNDLE"
mkdir -p "$CONTENTS/MacOS"
cp "$BUILD_DIR/release/TagTeamMenuBar" "$CONTENTS/MacOS/TagTeamMenuBar"
cat > "$CONTENTS/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleExecutable</key>
    <string>TagTeamMenuBar</string>
    <key>CFBundleIdentifier</key>
    <string>dev.tagteam.menubar</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>Tag Team</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>1.0</string>
    <key>CFBundleVersion</key>
    <string>1</string>
    <key>LSMinimumSystemVersion</key>
    <string>14.0</string>
    <key>LSUIElement</key>
    <true/>
    <key>NSPrincipalClass</key>
    <string>NSApplication</string>
</dict>
</plist>
PLIST
codesign --force --deep --sign - "$APP_BUNDLE"
codesign --verify --deep --strict --verbose=2 "$APP_BUNDLE"
printf 'Built app: %s\n' "$APP_BUNDLE"
