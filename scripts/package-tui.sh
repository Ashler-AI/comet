#!/usr/bin/env bash
# Terminal-UI packaging: build the `comet-tui` release binary and produce
#   target/package/comet-tui-<version>-<plat>-<arch>.tar.gz
# containing the `cometui` binary and a tiny install.sh.
#
# Deliberately separate from package-linux.sh / package-macos.sh: the TUI has
# no gpui dependency (no display libs, ~12MB), so it ships as a plain CLI on
# BOTH Linux and macOS — the same script runs on either host and stamps the
# platform into the artifact name.
#
# Usage: scripts/package-tui.sh
# Env:   PROFILE=debug for a fast unoptimized package (CI smoke); default release.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
PROFILE="${PROFILE:-release}"

case "$(uname -s)" in
  Linux) PLAT=linux ;;
  Darwin) PLAT=macos ;;
  *) echo "package-tui: unsupported OS '$(uname -s)'" >&2; exit 1 ;;
esac
ARCH="$(uname -m)"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')"
OUT_DIR="$ROOT/target/package"
STAGE="$OUT_DIR/comet-tui-$VERSION-$PLAT-$ARCH"
TARBALL="$STAGE.tar.gz"

cd "$ROOT"
if [[ "$PROFILE" == "release" ]]; then
  cargo build --release --bin comet-tui
  BIN="$ROOT/target/release/comet-tui"
else
  cargo build --bin comet-tui
  BIN="$ROOT/target/debug/comet-tui"
fi

rm -rf "$STAGE" "$TARBALL"
mkdir -p "$STAGE"
# Installed command name is `cometui` (single word) — the binary crate is
# `comet-tui`, but that hyphen is only an internal target name.
install -m 755 "$BIN" "$STAGE/cometui"

cat >"$STAGE/install.sh" <<'INSTALL'
#!/usr/bin/env bash
# Install the Comet terminal UI into ~/.local (no root needed).
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
install -Dm755 "$HERE/cometui" "$HOME/.local/bin/cometui"
echo "Installed cometui. Make sure ~/.local/bin is on your PATH, then run: cometui"
INSTALL
chmod 755 "$STAGE/install.sh"

tar -czf "$TARBALL" -C "$OUT_DIR" "$(basename "$STAGE")"
rm -rf "$STAGE"
echo "packaged: $TARBALL"
tar -tzf "$TARBALL"
