#!/usr/bin/env bash
# Assemble Conduit.app from the universal core + Swift GUI, then a .dmg.
# Run on macOS after build-gui.sh and a universal core build. Needs rsvg-convert
# (brew install librsvg) for the icon, plus iconutil/hdiutil (system tools).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
CORE="target/universal/conduit"
PKG="packaging/macos/ConduitGUI"
GUI="$PKG/.build/apple/Products/Release/conduit-gui"
[ -x "$GUI" ] || GUI="$PKG/.build/release/conduit-gui"

for b in "$CORE" "$GUI"; do
  [ -x "$b" ] || { echo "missing $b" >&2; exit 1; }
done

APP="dist/Conduit.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

install -m755 "$CORE" "$APP/Contents/MacOS/conduit"
install -m755 "$GUI"  "$APP/Contents/MacOS/conduit-gui"
sed "s/__VERSION__/$VERSION/g" packaging/macos/Info.plist > "$APP/Contents/Info.plist"

# App icon: render the SVG to an iconset and compile with iconutil.
ICONSET="$(mktemp -d)/conduit.iconset"
mkdir -p "$ICONSET"
for s in 16 32 128 256 512; do
  rsvg-convert -w $s -h $s assets/conduit.svg -o "$ICONSET/icon_${s}x${s}.png"
  rsvg-convert -w $((s*2)) -h $((s*2)) assets/conduit.svg -o "$ICONSET/icon_${s}x${s}@2x.png"
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/conduit.icns"

# Ad-hoc sign so Gatekeeper doesn't kill an unsigned bundle outright. (A real
# Developer ID signature + notarisation would replace this for distribution.)
codesign --force --deep --sign - "$APP" || echo "warning: ad-hoc codesign failed" >&2

# .dmg with a drag-to-Applications layout.
STAGE="$(mktemp -d)"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
mkdir -p dist
DMG="dist/Conduit-${VERSION}.dmg"
rm -f "$DMG"
hdiutil create -volname "Conduit" -srcfolder "$STAGE" -ov -format UDZO "$DMG"
echo "built $DMG"
