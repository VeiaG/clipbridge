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

# App icon: build an .iconset from the 1024px master and convert it to .icns.
ICONSET="$(mktemp -d)/AppIcon.iconset"
mkdir -p "$ICONSET"
MASTER="$HERE/../../assets/icon-1024.png"
for s in 16 32 128 256 512; do
  sips -z $s $s "$MASTER" --out "$ICONSET/icon_${s}x${s}.png" >/dev/null
  d=$((s * 2))
  sips -z $d $d "$MASTER" --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null
done
mkdir -p "$APP/Contents/Resources"
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns"
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
