#!/usr/bin/env bash
# Build a Conduit AppImage from already-built release binaries.
#
# Lean bundle: it carries the two Conduit binaries, the desktop entry and the
# icon, and uses the host's GTK (every GTK-based desktop already ships it). This
# keeps the image a couple of MB instead of ~80 MB, at the cost of needing a GTK
# runtime on the target — which the .deb depends on anyway.
#
# Needs appimagetool on PATH (or in $APPIMAGETOOL). Run from the repo root after
# building the release binaries. Produces dist/Conduit-<version>-<arch>.AppImage.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
CORE="target/release/conduit"
GUI="gui-gtk/target/release/conduit-gtk"
ARCH="$(uname -m)"

for b in "$CORE" "$GUI"; do
  [ -x "$b" ] || { echo "missing $b — build release binaries first" >&2; exit 1; }
done

APPIMAGETOOL="${APPIMAGETOOL:-appimagetool}"
run_tool() { "$APPIMAGETOOL" "$@" 2>/dev/null || "$APPIMAGETOOL" --appimage-extract-and-run "$@"; }

APPDIR="$(mktemp -d)/Conduit.AppDir"
trap 'rm -rf "$(dirname "$APPDIR")"' EXIT
mkdir -p "$APPDIR/usr/bin"

install -Dm755 "$CORE" "$APPDIR/usr/bin/conduit"
install -Dm755 "$GUI"  "$APPDIR/usr/bin/conduit-gtk"

# Desktop entry + icon at the AppDir root (AppImage convention) and under the
# hicolor tree so an installed .AppImage integrates too.
install -Dm644 packaging/linux/conduit.desktop "$APPDIR/usr/share/applications/conduit.desktop"
cp packaging/linux/conduit.desktop "$APPDIR/conduit.desktop"
install -Dm644 assets/conduit-256.png "$APPDIR/usr/share/icons/hicolor/256x256/apps/conduit.png"
cp assets/conduit-256.png "$APPDIR/conduit.png"
cp assets/conduit.svg "$APPDIR/conduit.svg"

cat > "$APPDIR/AppRun" <<'EOF'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
export PATH="$HERE/usr/bin:$PATH"
exec "$HERE/usr/bin/conduit" "$@"
EOF
chmod 755 "$APPDIR/AppRun"

mkdir -p dist
OUT="dist/Conduit-${VERSION}-${ARCH}.AppImage"
ARCH="$ARCH" run_tool "$APPDIR" "$OUT"
echo "built $OUT"
