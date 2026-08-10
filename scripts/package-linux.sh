#!/usr/bin/env bash
# Linux packaging: build the release binary and produce
#   target/package/comet-<version>-linux-<arch>.tar.gz
# containing the binary, the .desktop entry, and the icon, plus an install.sh
# that drops them into ~/.local (XDG) paths.
#
# Usage: scripts/package-linux.sh
# Env:   PROFILE=debug for a fast unoptimized package (CI smoke); default release.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
if [[ "$(uname -s)" != "Linux" ]]; then
  echo "Linux packaging requires a Linux runner" >&2
  exit 1
fi
PROFILE="${PROFILE:-release}"
ARCH="$(uname -m)"
case "$ARCH" in
  amd64) ARCH=x86_64 ;;
  arm64) ARCH=aarch64 ;;
esac
WORKSPACE_VERSION="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\([^"]*\)"/\1/p' "$ROOT/Cargo.toml")"
VERSION="${COMET_RELEASE_VERSION:-$WORKSPACE_VERSION}"
if [[ -z "$WORKSPACE_VERSION" || "$VERSION" != "$WORKSPACE_VERSION" ]]; then
  echo "release version '$VERSION' does not match workspace version '$WORKSPACE_VERSION'" >&2
  exit 1
fi
OUT_DIR="$ROOT/target/package"
STAGE="$OUT_DIR/comet-$VERSION-linux-$ARCH"
TARBALL="$STAGE.tar.gz"

cd "$ROOT"
if [[ "$PROFILE" == "release" ]]; then
  cargo build --release -p comet
  BIN="$ROOT/target/release/comet"
else
  cargo build -p comet
  BIN="$ROOT/target/debug/comet"
fi

rm -rf "$STAGE" "$TARBALL"
mkdir -p "$STAGE"
install -m 755 "$BIN" "$STAGE/comet"
install -m 644 "$ROOT/dist/comet.desktop" "$STAGE/comet.desktop"
install -m 644 "$ROOT/assets/brand/png/crew-icon-1024.png" "$STAGE/comet.png"
install -m 644 "$ROOT/assets/brand/crew-icon.svg" "$STAGE/comet.svg"

cat >"$STAGE/install.sh" <<'INSTALL'
#!/usr/bin/env bash
# Install Ashler Comet into ~/.local (no root needed).
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
install -Dm755 "$HERE/comet" "$HOME/.local/bin/comet"
install -Dm644 "$HERE/comet.desktop" "$HOME/.local/share/applications/comet.desktop"
install -Dm644 "$HERE/comet.png" "$HOME/.local/share/icons/hicolor/1024x1024/apps/comet.png"
install -Dm644 "$HERE/comet.svg" "$HOME/.local/share/icons/hicolor/scalable/apps/comet.svg"
command -v update-desktop-database >/dev/null 2>&1 \
  && update-desktop-database "$HOME/.local/share/applications" || true
echo "Ashler Comet installed. Make sure ~/.local/bin is on your PATH."
INSTALL
chmod 755 "$STAGE/install.sh"

GZIP=-n tar --sort=name --mtime="@${SOURCE_DATE_EPOCH:-0}" --owner=0 --group=0 \
  --numeric-owner -czf "$TARBALL" -C "$OUT_DIR" "$(basename "$STAGE")"
rm -rf "$STAGE"
echo "packaged: $TARBALL"
tar -tzf "$TARBALL"
