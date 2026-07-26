#!/usr/bin/env bash
# macOS packaging: build the release binary for the host arch and produce
#   target/package/comet-<version>-macos-<arch>.dmg
# containing Comet.app (unsigned unless CODESIGN_IDENTITY is set).
#
# Usage: scripts/package-macos.sh
# Env:   CODESIGN_IDENTITY="Developer ID Application: …" to sign the bundle.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')"
ARCH="$(uname -m)" # arm64 on Apple silicon runners
OUT_DIR="$ROOT/target/package"
APP="$OUT_DIR/Comet.app"
DMG="$OUT_DIR/comet-$VERSION-macos-$ARCH.dmg"

cd "$ROOT"
cargo build --release --bin comet --bin comet-tui

rm -rf "$APP" "$DMG"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
install -m 755 "$ROOT/target/release/comet" "$APP/Contents/MacOS/comet"
# The terminal viewport rides inside the bundle next to `comet`, so it resolves
# the engine binary as a sibling when it needs to spawn one. The .app doesn't
# put anything on PATH; users who want the `cometui` command run the standalone
# `curl … /install-tui.sh` (or symlink Contents/MacOS/cometui themselves).
install -m 755 "$ROOT/target/release/comet-tui" "$APP/Contents/MacOS/cometui"
sed "s/__VERSION__/$VERSION/" "$ROOT/dist/macos/Info.plist" >"$APP/Contents/Info.plist"

# Icon: iconset from dist/comet.png — the comet mark from the original app
# (apps/desktop/resources/icon.png in the comet repo; source dist/comet.svg).
ICONSET="$OUT_DIR/comet.iconset"
rm -rf "$ICONSET" && mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$ROOT/dist/comet.png" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
  retina=$((size * 2))
  sips -z "$retina" "$retina" "$ROOT/dist/comet.png" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
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

hdiutil create -volname Comet -srcfolder "$APP" -ov -format UDZO "$DMG"
echo "packaged: $DMG"
