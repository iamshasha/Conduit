#!/usr/bin/env bash
# Build the macOS Swift GUI (conduit-gui) as a universal binary.
# Run on a macOS host with a Swift toolchain. Output path is printed at the end.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PKG="$ROOT/packaging/macos/ConduitGUI"

# The GUIs share one Strings.json; copy the real one over the placeholder.
cp "$ROOT/gui-winui/Strings.json" "$PKG/Sources/ConduitGUI/Resources/Strings.json"

cd "$PKG"
swift build -c release --arch arm64 --arch x86_64

# Universal builds land under .build/apple/Products/Release; a single-arch build
# under .build/release. Report whichever exists.
for p in ".build/apple/Products/Release/conduit-gui" ".build/release/conduit-gui"; do
  if [ -x "$p" ]; then echo "built $PKG/$p"; exit 0; fi
done
echo "conduit-gui binary not found after build" >&2
exit 1
