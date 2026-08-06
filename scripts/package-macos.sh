#!/usr/bin/env bash
# macOS packaging: build the release binary for the host arch and produce
#   target/package/comet-<version>-macos-<arch>.dmg          (user download)
#   target/package/comet-<version>-macos-<arch>-app.tar.gz   (auto-updater)
# containing Comet.app (unsigned unless CODESIGN_IDENTITY is set).
#
# Usage: scripts/package-macos.sh
# Env:   CODESIGN_IDENTITY="Developer ID Application: …" to sign the bundle.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "macOS packaging requires a macOS runner" >&2
  exit 1
fi
WORKSPACE_VERSION="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\([^"]*\)"/\1/p' "$ROOT/Cargo.toml")"
VERSION="${COMET_RELEASE_VERSION:-$WORKSPACE_VERSION}"
if [[ -z "$WORKSPACE_VERSION" || "$VERSION" != "$WORKSPACE_VERSION" ]]; then
  echo "release version '$VERSION' does not match workspace version '$WORKSPACE_VERSION'" >&2
  exit 1
fi
ARCH="$(uname -m)"
if [[ "$ARCH" != "arm64" ]]; then
  echo "macOS releases require an Apple-silicon runner (found $ARCH)" >&2
  exit 1
fi
OUT_DIR="$ROOT/target/package"
APP="$OUT_DIR/Comet.app"
DMG="$OUT_DIR/comet-$VERSION-macos-$ARCH.dmg"
APP_TARBALL="$OUT_DIR/comet-$VERSION-macos-$ARCH-app.tar.gz"

cd "$ROOT"
cargo build --release -p comet

rm -rf "$APP" "$DMG" "$APP_TARBALL"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
install -m 755 "$ROOT/target/release/comet" "$APP/Contents/MacOS/comet"
sed "s/__VERSION__/$VERSION/" "$ROOT/dist/macos/Info.plist" >"$APP/Contents/Info.plist"

# Icon: iconset from the brand mark on the Apple squircle tile. The Dock/.icns
# variant is deliberately rounder than the Ashler tile radius used elsewhere —
# see assets/brand/README.md. Source vector: assets/brand/crew-icon-macos.svg.
ICON_SRC="$ROOT/assets/brand/png/crew-icon-macos-1024.png"
ICONSET="$OUT_DIR/comet.iconset"
rm -rf "$ICONSET" && mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$ICON_SRC" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
  retina=$((size * 2))
  sips -z "$retina" "$retina" "$ICON_SRC" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/comet.icns"
rm -rf "$ICONSET"

if [[ -n "${CODESIGN_IDENTITY:-}" ]]; then
  codesign --deep --force --options runtime --sign "$CODESIGN_IDENTITY" "$APP"
else
  # Ad-hoc signature so the app launches on Apple silicon (Gatekeeper still
  # requires right-click → Open on first launch without notarization).
  codesign --deep --force --sign - "$APP"
fi

# The auto-updater artifact is the exact signed bundle promoted by CI.
COPYFILE_DISABLE=1 tar -czf "$APP_TARBALL" -C "$OUT_DIR" Comet.app
echo "packaged: $APP_TARBALL"

hdiutil create -volname "Ashler Comet" -srcfolder "$APP" -ov -format UDZO "$DMG"
echo "packaged: $DMG"
