#!/usr/bin/env bash
# Build a Conduit .deb from already-built release binaries.
#
#   conduit       -> /usr/bin/conduit        (core: server + tray)
#   conduit-gtk   -> /usr/bin/conduit-gtk     (GTK4 window process)
#   conduit.desktop, hicolor icons, conduit:// scheme handler
#
# Run from the repo root after `cargo build --release` in both the workspace and
# gui-gtk/. Produces dist/conduit_<version>_<arch>.deb.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
CORE="target/release/conduit"
GUI="gui-gtk/target/release/conduit-gtk"

for b in "$CORE" "$GUI"; do
  [ -x "$b" ] || { echo "missing $b — build release binaries first" >&2; exit 1; }
done

# Debian architecture from the core binary's ELF machine.
case "$(uname -m)" in
  x86_64)  DEB_ARCH=amd64 ;;
  aarch64) DEB_ARCH=arm64 ;;
  *) DEB_ARCH="$(dpkg --print-architecture)" ;;
esac

PKG="$(mktemp -d)"
trap 'rm -rf "$PKG"' EXIT
chmod 755 "$PKG"  # dpkg root must be world-readable (mktemp -d is 700)

install -Dm755 "$CORE" "$PKG/usr/bin/conduit"
install -Dm755 "$GUI"  "$PKG/usr/bin/conduit-gtk"
install -Dm644 packaging/linux/conduit.desktop "$PKG/usr/share/applications/conduit.desktop"
install -Dm644 assets/conduit.svg "$PKG/usr/share/icons/hicolor/scalable/apps/conduit.svg"
for s in 16 24 32 48 64 128 256 512; do
  install -Dm644 "assets/conduit-$s.png" "$PKG/usr/share/icons/hicolor/${s}x${s}/apps/conduit.png"
done

# Strip already done by the release profile; keep the sizes honest for control.
INSTALLED_KB=$(du -ks "$PKG/usr" | cut -f1)

mkdir -p "$PKG/DEBIAN"
cat > "$PKG/DEBIAN/control" <<EOF
Package: conduit
Version: $VERSION
Section: utils
Priority: optional
Architecture: $DEB_ARCH
Maintainer: Conduit <conduit@localhost>
Installed-Size: $INSTALLED_KB
Depends: libgtk-3-0 | libgtk-3-0t64, libc6
Description: Consent-gated bridge from approved websites to the local system
 Conduit is a small local host that lets a website you approve keep real files
 in a per-site sandbox, launch applications, read hardware and live stats, and
 control media, power and the clipboard. Every ability is gated by a permission
 the user grants with a click. Ships a GTK dashboard and consent pop-up, speaks
 WebSocket and plain HTTP JSON on 127.0.0.1, and registers the conduit:// URL
 scheme so a page can bring its consent window to the front.
EOF

# Refresh the icon cache and desktop database on install/removal.
cat > "$PKG/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if [ -x "$(command -v gtk-update-icon-cache 2>/dev/null)" ]; then
  gtk-update-icon-cache -q -t -f /usr/share/icons/hicolor || true
fi
if [ -x "$(command -v update-desktop-database 2>/dev/null)" ]; then
  update-desktop-database -q /usr/share/applications || true
fi
EOF
cp "$PKG/DEBIAN/postinst" "$PKG/DEBIAN/postrm"
chmod 755 "$PKG/DEBIAN/postinst" "$PKG/DEBIAN/postrm"

mkdir -p dist
OUT="dist/conduit_${VERSION}_${DEB_ARCH}.deb"
dpkg-deb --build --root-owner-group "$PKG" "$OUT"
echo "built $OUT"
