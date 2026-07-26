#!/bin/sh
# Comet terminal UI installer.
#
#   curl -fsSL https://comet.zeron.sh/install-tui.sh | sh
#
# Installs the `cometui` binary — a terminal viewport over the same engine the
# desktop app runs. Unlike the desktop app, it has no gpui dependency, so it is
# a plain CLI on both Linux and macOS: no display libraries, no service, ~12MB.
#
# `cometui` never runs an engine of its own. It attaches to whatever is already
# listening on the IPC port — a running desktop app (which serves its embedded
# engine), or a `comet headless` daemon — and starts one only if `comet` is also
# installed. Closing the terminal detaches; work keeps running.
#
# Re-running upgrades in place. ~/.comet-native state is shared with `comet`.
set -eu

BASE="${COMET_BASE_URL:-https://comet.zeron.sh}"

# --- platform ---------------------------------------------------------------
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux) plat=linux ;;
  Darwin) plat=macos ;;
  *)
    echo "cometui install: unsupported OS '$os' — only Linux and macOS." >&2
    exit 1
    ;;
esac
case "$arch" in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  *)
    echo "cometui install: unsupported architecture '$arch'." >&2
    exit 1
    ;;
esac

# --- download ----------------------------------------------------------------
ver="$(curl -fsSL "$BASE/releases/latest.txt" | tr -d '[:space:]')"
[ -n "$ver" ] || { echo "cometui install: could not resolve latest version" >&2; exit 1; }
file="comet-tui-$ver-$plat-$arch.tar.gz"
data_root="$HOME/.comet-native"
app_root="$data_root/tui"
dest="$app_root/$ver"

if [ -x "$dest/cometui" ]; then
  echo "cometui $ver already downloaded — relinking."
else
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  echo "downloading cometui $ver ($plat-$arch)…"
  curl -fSL --progress-bar "$BASE/releases/$file" -o "$tmp/$file"
  mkdir -p "$dest"
  tar -xzf "$tmp/$file" -C "$dest" --strip-components=1
fi

ln -sfn "$dest" "$app_root/current"
mkdir -p "$HOME/.local/bin"
ln -sf "$app_root/current/cometui" "$HOME/.local/bin/cometui"

case ":$PATH:" in
  *":$HOME/.local/bin:"*) path_hint="" ;;
  *) path_hint=' (add ~/.local/bin to your PATH)' ;;
esac

echo ""
echo "✓ cometui $ver installed$path_hint"
echo ""
echo "run it with:  cometui"
echo "it attaches to a running comet desktop app or headless daemon; if neither"
echo "is up and \`comet\` is installed, it starts one. Quitting detaches."
