#!/usr/bin/env bash
# Offline check of the private canary tarball installer. It must write
# isolated absolute launchers, preserve existing installations, and support
# repeated packages of the same workspace version. No network or services.
#
# Usage: scripts/test-linux-desktop-entry.sh
set -euo pipefail
export ASHLER_INCREMENTAL_TSC_CHECKS=false

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
VERSION=9.9.9
fail() { echo "FAIL: $*" >&2; exit 1; }

# A fixture executable, never an installed app or a shared release feed.
PKG="crew-upstream-canary-$VERSION-linux-x86_64"
mkdir -p "$WORK/pkg/$PKG"
printf '#!/bin/sh\nexit 0\n' >"$WORK/pkg/$PKG/comet"
chmod 755 "$WORK/pkg/$PKG/comet"
cp "$ROOT/dist/zeron.desktop" "$WORK/pkg/$PKG/crew-upstream-canary.desktop"
printf 'not-really-a-png' >"$WORK/pkg/$PKG/crew-upstream-canary.png"
mkdir -p "$WORK/pkg/$PKG/crew-web"
printf 'native browser assets' >"$WORK/pkg/$PKG/crew-web/server.mjs"

# The tarball's install.sh lives in a heredoc inside package-linux.sh.
sed -n "/<<'INSTALL'/,/^INSTALL\$/p" "$ROOT/scripts/package-linux.sh" | sed '1d;$d' \
  | sed "s/__VERSION__/$VERSION/" >"$WORK/pkg/$PKG/install.sh"
chmod 755 "$WORK/pkg/$PKG/install.sh"

mkdir -p "$WORK/tmp"
run_tarball() {
  local home="$1"; shift
  env -i HOME="$home" USER=tester PATH="/usr/bin:/bin" TMPDIR="$WORK/tmp" "$@" \
    bash "$WORK/pkg/$PKG/install.sh" >"$WORK/out.log" 2>&1 \
    || { cat "$WORK/out.log" >&2; fail "tarball installer exited non-zero"; }
  [ -z "$(ls -A "$WORK/tmp")" ] || fail "tarball installer left files in TMPDIR: $(ls -A "$WORK/tmp")"
}

# check HOME DATA_HOME
check() {
  local home="$1" data="$2" entry="$2/applications/crew-upstream-canary.desktop"
  [ -f "$entry" ] || fail "missing $entry"
  [ -f "$data/icons/hicolor/1024x1024/apps/crew-upstream-canary.png" ] || fail "missing hicolor icon"
  grep -qxF "TryExec=$home/.crew-upstream-canary/app/current/comet" "$entry" || fail "TryExec"
  grep -qxF "Icon=$home/.crew-upstream-canary/app/current/crew-upstream-canary.png" "$entry" || fail "Icon"
  grep -qxF "MimeType=x-scheme-handler/crew-upstream-canary;" "$entry" || fail "scheme"
  [ -L "$home/.local/bin/crew-upstream-canary" ] || fail "canary launcher missing"
  grep -qxF "StartupWMClass=zeron" "$entry" || fail "StartupWMClass changed"
  [ "$(grep -c '^\[Desktop Entry\]' "$entry")" = 1 ] || fail "duplicated entry"
  [ "$(grep -c '^Exec=' "$entry")" = 1 ] || fail "Exec lines"
  [ -z "$(find "$data" -name '.crew-upstream-canary*')" ] || fail "temp files left behind"
  # A user-level icon cache is only ever refreshed, never created.
  [ ! -e "$data/icons/hicolor/icon-theme.cache" ] || fail "created a hicolor icon cache"
  if command -v desktop-file-validate >/dev/null 2>&1; then
    desktop-file-validate "$entry" || fail "desktop-file-validate"
  fi
}

for installer in tarball; do
  run() { "run_$installer" "$@"; }

  # Default XDG location, then a re-run (how updates are installed) is stable.
  home="$WORK/$installer-a/home"; mkdir -p "$home"
  mkdir -p "$home/.crew" "$home/.zeron" "$home/.local/bin"
  printf 'preserve' >"$home/.crew/sentinel"
  printf 'preserve' >"$home/.zeron/sentinel"
  printf 'preserve' >"$home/.local/bin/comet"
  run "$home"
  check "$home" "$home/.local/share"
  entry="$home/.local/share/applications/crew-upstream-canary.desktop"
  grep -qxF "Exec=$home/.crew-upstream-canary/app/current/comet %u" "$entry" || fail "Exec line"
  before="$(cat "$entry")"
  first="$(readlink "$home/.crew-upstream-canary/app/current")"
  run "$home"
  check "$home" "$home/.local/share"
  [ "$before" = "$(cat "$entry")" ] || fail "re-run changed the entry"
  printf '# changed build, same version\n' >>"$WORK/pkg/$PKG/comet"
  run "$home"
  second="$(readlink "$home/.crew-upstream-canary/app/current")"
  [ "$first" != "$second" ] || fail "same-version new artifact not installed"
  [ -x "$first/comet" ] || fail "old artifact was deleted"
  printf '\nchanged browser, same native binary\n' >>"$WORK/pkg/$PKG/crew-web/server.mjs"
  run "$home"
  third="$(readlink "$home/.crew-upstream-canary/app/current")"
  [ "$second" != "$third" ] || fail "same-version browser asset update not installed"
  cmp "$WORK/pkg/$PKG/crew-web/server.mjs" "$third/crew-web/server.mjs" || fail "stale browser assets installed"
  [ -x "$second/comet" ] || fail "previous native bundle was deleted"
  run "$home"
  [ "$third" = "$(readlink "$home/.crew-upstream-canary/app/current")" ] || fail "identical native bundle changed install identity"
  [ "$(cat "$home/.crew/sentinel")" = preserve ] || fail "Crew changed"
  [ "$(cat "$home/.zeron/sentinel")" = preserve ] || fail "Zeron changed"
  [ "$(cat "$home/.local/bin/comet")" = preserve ] || fail "shared comet launcher changed"

  # XDG_DATA_HOME wins when absolute; a relative value is ignored per the spec.
  home="$WORK/$installer-b/home"; mkdir -p "$home"
  run "$home" XDG_DATA_HOME="$WORK/$installer-b/xdg"
  check "$home" "$WORK/$installer-b/xdg"
  [ ! -e "$home/.local/share/applications" ] || fail "$installer: wrote outside XDG_DATA_HOME"
  home="$WORK/$installer-c/home"; mkdir -p "$home"
  run "$home" XDG_DATA_HOME=relative/dir
  check "$home" "$home/.local/share"

  # A home with a space and characters the Exec key must quote and escape.
  home="$WORK/$installer-d/h o\$me\"x%y"; mkdir -p "$home"
  run "$home"
  check "$home" "$home/.local/share"
  # Spec: quote the argument, `\` before " and $ (doubled again for the file's
  # string escaping), and `%%` for a literal `%`.
  want="Exec=\"$WORK/$installer-d/"'h o\\$me\\"x%%y'"/.crew-upstream-canary/app/current/comet\" %u"
  grep -qxF "$want" "$home/.local/share/applications/crew-upstream-canary.desktop" \
    || fail "$installer: Exec quoting"
  echo "ok: $installer installer"
done
