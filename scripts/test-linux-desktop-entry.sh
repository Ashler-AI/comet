#!/usr/bin/env bash
# Offline canonical tarball installer check. Uses only a throwaway HOME;
# never starts an engine, service, network request, or installed application.
set -euo pipefail
export ASHLER_INCREMENTAL_TSC_CHECKS=false

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
fail() { echo "FAIL: $*" >&2; exit 1; }
PKG="$WORK/comet-9.9.9-linux-x86_64"
mkdir -p "$PKG"
printf '#!/bin/sh\nexit 0\n' >"$PKG/comet"
chmod 755 "$PKG/comet"
cp "$ROOT/dist/comet.desktop" "$PKG/comet.desktop"
printf 'fixture-png' >"$PKG/comet.png"
printf 'fixture-svg' >"$PKG/comet.svg"
sed -n "/<<'INSTALL'/,/^INSTALL\$/p" "$ROOT/scripts/package-linux.sh" | sed '1d;$d' >"$PKG/install.sh"

for home in "$WORK/home" "$WORK/h o\$me\"x%y"; do
  mkdir -p "$home"
  for profile in .comet-native .comet-native-staging .crew-upstream-canary .crew-staging .crew; do
    mkdir -p "$home/$profile"
    printf 'preserve' >"$home/$profile/sentinel"
  done
  for attempt in 1 2; do
    env -i HOME="$home" PATH=/usr/bin:/bin bash "$PKG/install.sh"
    cmp "$PKG/comet" "$home/.local/bin/comet" || fail 'executable differs'
    [ -x "$home/.local/bin/comet" ] || fail 'executable mode'
    entry="$home/.local/share/applications/comet.desktop"
    cmp "$PKG/comet.desktop" "$entry" || fail 'desktop entry differs'
    grep -qxF 'Exec=comet %u' "$entry" || fail 'canonical executable'
    grep -qxF 'MimeType=x-scheme-handler/comet;' "$entry" || fail 'canonical URL scheme'
    cmp "$PKG/comet.png" "$home/.local/share/icons/hicolor/1024x1024/apps/comet.png" || fail 'PNG icon differs'
    cmp "$PKG/comet.svg" "$home/.local/share/icons/hicolor/scalable/apps/comet.svg" || fail 'SVG icon differs'
    for profile in .comet-native .comet-native-staging .crew-upstream-canary .crew-staging .crew; do
      [ "$(cat "$home/$profile/sentinel")" = preserve ] || fail "changed $profile"
    done
    if command -v desktop-file-validate >/dev/null 2>&1; then
      desktop-file-validate "$entry" || fail 'desktop-file-validate'
    fi
  done
done
echo 'ok: canonical tarball installer preserves profile data'
