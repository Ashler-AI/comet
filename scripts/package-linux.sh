#!/usr/bin/env bash
# Linux packaging: build the release binary and produce
#   target/package/crew-upstream-canary-<version>-linux-<arch>.tar.gz
# containing comet, the matching crew-web bundle and contract, desktop assets,
# and a manual install.sh. The browser bundle requires Node 22.4 or newer.
# Installs only into ~/.crew-upstream-canary/app and a canary-named launcher.
#
# Usage: scripts/package-linux.sh
# Env:   PROFILE=debug for a fast unoptimized package (CI smoke); default release.

set -euo pipefail
export ASHLER_INCREMENTAL_TSC_CHECKS=false

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
PROFILE="${PROFILE:-release}"
ARCH="$(uname -m)"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')"
OUT_DIR="$ROOT/target/package"
STAGE="$OUT_DIR/crew-upstream-canary-$VERSION-linux-$ARCH"
TARBALL="$STAGE.tar.gz"

cd "$ROOT"
TARGET_DIR="$(cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
if [[ "$PROFILE" == "release" ]]; then
  cargo build --locked --release -p zeron
  BIN="$TARGET_DIR/release/zeron"
elif [[ "$PROFILE" == "debug" ]]; then
  cargo build --locked -p zeron
  BIN="$TARGET_DIR/debug/zeron"
else
  echo "PROFILE must be release or debug" >&2
  exit 1
fi

rm -rf "$STAGE" "$TARBALL"
mkdir -p "$STAGE"
install -m 755 "$BIN" "$STAGE/comet"
install -m 644 "$ROOT/dist/zeron.desktop" "$STAGE/crew-upstream-canary.desktop"
install -m 644 "$ROOT/dist/zeron.png" "$STAGE/crew-upstream-canary.png"
install -m 644 "$ROOT/LICENSE" "$ROOT/THIRD_PARTY_NOTICES.md" "$STAGE/"
mkdir -p "$STAGE/licenses/fonts"
cp "$ROOT/crates/ui/assets/fonts/licenses/"* "$STAGE/licenses/fonts/"
cp "$ROOT/crates/voice/NOTICE.md" "$STAGE/licenses/parakeet-v3.txt"
mkdir -p "$STAGE/crew-web/public"
install -m 644 "$ROOT/apps/web/server.mjs" "$ROOT/apps/web/rpc.mjs" "$ROOT/apps/web/package.json" "$ROOT/apps/web/contract.json" "$STAGE/crew-web/"
install -m 644 "$ROOT/apps/web/public/app.js" "$ROOT/apps/web/public/index.html" "$ROOT/apps/web/public/style.css" "$STAGE/crew-web/public/"
install -m 644 "$ROOT/apps/web/contract.json" "$STAGE/crew-web-contract.json"

cat >"$STAGE/install.sh" <<'INSTALL'
#!/usr/bin/env bash
# Install Crew Canary for this user; never overwrite Crew/Zeron installations.
# This manual delivery layout does not opt into a shared release channel.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
VERSION="__VERSION__"
APP_ROOT="$HOME/.crew-upstream-canary/app"
DIGEST="$(sha256sum "$HERE/comet" | cut -d' ' -f1)"
DEST="$APP_ROOT/$VERSION-$DIGEST"
mkdir -p "$APP_ROOT"
if [ ! -x "$DEST/comet" ]; then
  # Copy beside the final name, then rename: an interrupted install never
  # leaves a half-copied version the updater would trust.
  STAGE="$(mktemp -d "$APP_ROOT/.install-$VERSION-XXXXXX")"
  cp -R "$HERE/." "$STAGE/"
  rm -rf "$DEST"
  mv "$STAGE" "$DEST"
fi
if ! "$DEST/comet" --version >/dev/null; then
  echo "Crew Canary could not start; see the loader error above. Install the missing runtime libraries (including ALSA, libasound.so.2), then retry." >&2
  exit 1
fi
ln -sfn "$DEST" "$APP_ROOT/current"
mkdir -p "$HOME/.local/bin"
ln -sfn "$APP_ROOT/current/comet" "$HOME/.local/bin/crew-upstream-canary"

# Absolute paths work even when ~/.local/bin is absent from the GUI PATH.
install_desktop_entry() {
  src="$1"
  app="$2"
  [ -f "$src/crew-upstream-canary.desktop" ] && [ -f "$src/crew-upstream-canary.png" ] || return 1
  case "${XDG_DATA_HOME:-}" in
    /*) data_home="$XDG_DATA_HOME" ;;
    *) data_home="$HOME/.local/share" ;;
  esac
  apps_dir="$data_home/applications"
  icon_dir="$data_home/icons/hicolor/1024x1024/apps"
  bin="$app/current/comet"
  icon="$app/current/crew-upstream-canary.png"
  # Desktop Entry `Exec` quoting: double-quote an argument with reserved
  # characters, backslash-escape ", `, $ and \ inside, then double every
  # backslash again for the file's own string escaping. `%` must be `%%`.
  case "$bin" in
    *[!A-Za-z0-9_./-]*)
      exec_bin="\"$(printf '%s' "$bin" | sed -e 's/\\/\\\\\\\\/g' -e 's/["`$]/\\\\&/g' -e 's/%/%%/g')\""
      ;;
    *) exec_bin="$bin" ;;
  esac
  try_bin="$(printf '%s' "$bin" | sed 's/\\/\\\\/g')"
  icon_val="$(printf '%s' "$icon" | sed 's/\\/\\\\/g')"

  mkdir -p "$apps_dir" "$icon_dir" || return 1
  # Write beside the final name, then rename, so a launcher watching the
  # directory never reads a half-written entry (a leading dot is ignored).
  entry_tmp="$apps_dir/.crew-upstream-canary.desktop.$$"
  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in
      Exec=*) printf 'Exec=%s %%u\n' "$exec_bin" ;;
      TryExec=*) printf 'TryExec=%s\n' "$try_bin" ;;
      Icon=*) printf 'Icon=%s\n' "$icon_val" ;;
      *) printf '%s\n' "$line" ;;
    esac
  done <"$src/crew-upstream-canary.desktop" >"$entry_tmp" || { rm -f "$entry_tmp"; return 1; }
  mv -f "$entry_tmp" "$apps_dir/crew-upstream-canary.desktop" || { rm -f "$entry_tmp"; return 1; }
  cp "$src/crew-upstream-canary.png" "$icon_dir/.crew-upstream-canary.png.$$" \
    && mv -f "$icon_dir/.crew-upstream-canary.png.$$" "$icon_dir/crew-upstream-canary.png" || return 1

  # Best-effort cache refresh; both tools are optional. The icon cache is only
  # refreshed, never created: a user-level hicolor cache nobody else maintains
  # would hide icons other apps later install there, and the entry above
  # references the icon by path anyway.
  command -v update-desktop-database >/dev/null 2>&1 \
    && update-desktop-database "$apps_dir" >/dev/null 2>&1 || true
  [ -f "$data_home/icons/hicolor/icon-theme.cache" ] \
    && command -v gtk-update-icon-cache >/dev/null 2>&1 \
    && gtk-update-icon-cache -q -t -f "$data_home/icons/hicolor" >/dev/null 2>&1 || true
  return 0
}
install_desktop_entry "$HERE" "$APP_ROOT" \
  || echo "warn: could not install the desktop entry — Crew Canary won't appear in application launchers"

echo "Installed Crew Canary $VERSION. Updates require another private/manual package."
case ":$PATH:" in
  *":$HOME/.local/bin:"*) ;;
  *) echo "Add ~/.local/bin to your PATH to run \`crew-upstream-canary\` from a terminal." ;;
esac
INSTALL
sed -i "s/__VERSION__/$VERSION/" "$STAGE/install.sh"
chmod 755 "$STAGE/install.sh"

tar -czf "$TARBALL" -C "$OUT_DIR" "$(basename "$STAGE")"
rm -rf "$STAGE"
echo "packaged: $TARBALL"
tar -tzf "$TARBALL"
