#!/usr/bin/env bash
# Builds ClipBridge.app and a drag-to-install DMG from a release binary.
# Usage: packaging/macos/build-app.sh <path-to-binary> <version> <out-dir>
set -euo pipefail

BIN="$1"
VERSION="$2"
OUT="$3"
HERE="$(cd "$(dirname "$0")" && pwd)"

APP="$OUT/ClipBridge.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp "$BIN" "$APP/Contents/MacOS/clipbridge"
chmod +x "$APP/Contents/MacOS/clipbridge"
sed "s/__VERSION__/$VERSION/g" "$HERE/Info.plist" > "$APP/Contents/Info.plist"
plutil -lint "$APP/Contents/Info.plist"

# Ad-hoc signature (free, no Apple Developer account). Apple Silicon refuses to run
# an unsealed bundle ("app is damaged"); this seals it. Gatekeeper still shows the
# "unidentified developer" prompt on first launch.
codesign --force --deep --sign - "$APP"
codesign --verify --verbose "$APP"

STAGE="$(mktemp -d)"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
DMG="$OUT/ClipBridge-$VERSION-macos-arm64.dmg"
rm -f "$DMG"
hdiutil create -volname "ClipBridge" -srcfolder "$STAGE" -ov -format UDZO "$DMG"
rm -rf "$STAGE"
echo "Built $DMG"
