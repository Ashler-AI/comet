#!/usr/bin/env bash
# Offline checks of both Crew release installers. They must write absolute
# launchers and immutable versions while preserving existing profile data.
# All homes, feeds, and binaries are fixtures. No network or services.
#
# Usage: scripts/test-linux-desktop-entry.sh
set -euo pipefail
export ASHLER_INCREMENTAL_TSC_CHECKS=false

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
fail() { echo "FAIL: $*" >&2; exit 1; }
VERSION=9.9.9

# Exercise Linux packaging preflight with a Linux host fixture, stopping at Cargo.
mkdir -p "$WORK/build-bin"
cat >"$WORK/build-bin/cargo" <<'CARGO'
#!/bin/sh
printf 'CARGO_BOUNDARY %s %s\n' "$COMET_PACKAGE_ENVIRONMENT" "$COMET_DEFAULT_ENVIRONMENT" >&2
exit 86
CARGO
cat >"$WORK/build-bin/uname" <<'UNAME'
#!/bin/sh
case "$1" in
  -s) printf '%s\n' Linux ;;
  -m) printf '%s\n' x86_64 ;;
  *) exit 2 ;;
esac
UNAME
chmod 755 "$WORK/build-bin/cargo" "$WORK/build-bin/uname"
for selectors in default production package-staging default-staging conflict; do
  args=()
  case "$selectors" in
    production) args=(COMET_PACKAGE_ENVIRONMENT=production COMET_DEFAULT_ENVIRONMENT=production) ;;
    package-staging) args=(COMET_PACKAGE_ENVIRONMENT=staging) ;;
    default-staging) args=(COMET_DEFAULT_ENVIRONMENT=staging) ;;
    conflict) args=(COMET_PACKAGE_ENVIRONMENT=production COMET_DEFAULT_ENVIRONMENT=staging) ;;
  esac
  status=0
  env -i HOME="$WORK/build-home" PATH="$WORK/build-bin:/usr/bin:/bin" CI=true \
    CARGO_TARGET_DIR="$WORK/build-target" "${args[@]}" \
    bash "$ROOT/scripts/package-linux.sh" >"$WORK/preflight.log" 2>&1 || status=$?
  case "$selectors" in
    default|production)
      [ "$status" -ne 0 ] || fail "packaging passed the Cargo boundary"
      grep -q 'CARGO_BOUNDARY production production' "$WORK/preflight.log" || fail "wrong Linux package identity"
      ;;
    *)
      [ "$status" -eq 1 ] || fail "staging selector was not rejected"
      grep -q 'Linux packages require production identity' "$WORK/preflight.log" || fail "missing identity error"
      if grep -q CARGO_BOUNDARY "$WORK/preflight.log"; then fail "invalid identity reached Cargo"; fi
      ;;
  esac
done

# A fixture executable, never an installed app or a shared release feed.
PKG="comet-$VERSION-linux-$(uname -m)"
mkdir -p "$WORK/pkg/$PKG"
printf '#!/bin/sh\nexit 0\n' >"$WORK/pkg/$PKG/comet"
chmod 755 "$WORK/pkg/$PKG/comet"
cp "$ROOT/dist/comet.desktop" "$WORK/pkg/$PKG/comet.desktop"
printf 'not-really-a-png' >"$WORK/pkg/$PKG/comet.png"
printf '<svg xmlns="http://www.w3.org/2000/svg"/>' >"$WORK/pkg/$PKG/comet.svg"

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

# Serve a private release feed through a fake curl. Credentials must arrive on
# stdin, not in argv or the inherited environment.
mkdir -p "$WORK/bin" "$WORK/feed"
cat >"$WORK/bin/curl" <<'CURL'
#!/usr/bin/env bash
set -euo pipefail
[ -z "${COMET_RELEASES_AUTHORIZATION:-}" ] || exit 91
case "$*" in *fixture-token*) exit 92 ;; esac
IFS= read -r header
[ "$header" = 'Authorization: Bearer fixture-token' ] || exit 93
output= url= proto= redirects= tls=
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o) output="$2"; shift ;;
    --proto) proto="$2"; shift ;;
    --proto-redir) redirects="$2"; shift ;;
    --tlsv1.2) tls=yes ;;
    https://fixture.invalid/*) url="$1" ;;
  esac
  shift
done
[ "$proto" = '=https' ] && [ "$redirects" = '=https' ] && [ "$tls" = yes ] || exit 94
file="$FIXTURE_FEED/${url##*/}"
if [ -n "$output" ]; then cp "$file" "$output"; else cat "$file"; fi
CURL
chmod 755 "$WORK/bin/curl"
printf '%s\n' "$VERSION" >"$WORK/feed/scaffold-latest.txt"
prepare_feed() {
  tar -czf "$WORK/feed/$PKG.tar.gz" -C "$WORK/pkg" "$PKG"
  (cd "$WORK/feed" && sha256sum "$PKG.tar.gz") >"$WORK/feed/scaffold-SHA256SUMS"
}
invoke_release() {
  env -i HOME="$1" USER=tester PATH="$WORK/bin:/usr/bin:/bin" TMPDIR="$WORK/tmp" \
    FIXTURE_FEED="$WORK/feed" COMET_RELEASES_URL=https://fixture.invalid \
    COMET_RELEASES_AUTHORIZATION='Bearer fixture-token' COMET_SKIP_AGENT_BOOTSTRAP=1 \
    "${@:2}" sh "$ROOT/install.sh"
}
run_release() {
  prepare_feed
  invoke_release "$@" >"$WORK/out.log" 2>&1 \
    || { cat "$WORK/out.log" >&2; fail "release installer exited non-zero"; }
  [ -z "$(ls -A "$WORK/tmp")" ] || fail "release installer left files in TMPDIR"
}

# check HOME DATA_HOME
check() {
  local home="$1" data="$2" entry="$2/applications/comet.desktop"
  [ -f "$entry" ] || fail "missing $entry"
  [ -f "$data/icons/hicolor/1024x1024/apps/comet.png" ] || fail "missing hicolor icon"
  [ -f "$data/icons/hicolor/scalable/apps/comet.svg" ] || fail "missing scalable icon"
  grep -qxF "TryExec=$home/.crew/app/current/comet" "$entry" || fail "TryExec"
  grep -qxF "Icon=$home/.crew/app/current/comet.png" "$entry" || fail "Icon"
  grep -qxF "MimeType=x-scheme-handler/comet;" "$entry" || fail "scheme"
  [ -L "$home/.local/bin/comet" ] || fail "comet launcher missing"
  [ "$(readlink "$home/.local/bin/comet")" = "$home/.crew/app/current/comet" ] || fail "launcher target"
  cmp "$WORK/pkg/$PKG/comet" "$home/.local/bin/comet" || fail "executable differs"
  [ -x "$home/.local/bin/comet" ] || fail "executable mode"
  cmp "$WORK/pkg/$PKG/comet.png" "$data/icons/hicolor/1024x1024/apps/comet.png" || fail "PNG icon differs"
  cmp "$WORK/pkg/$PKG/comet.svg" "$data/icons/hicolor/scalable/apps/comet.svg" || fail "SVG icon differs"
  grep -qxF "StartupWMClass=comet" "$entry" || fail "StartupWMClass changed"
  [ "$(grep -c '^\[Desktop Entry\]' "$entry")" = 1 ] || fail "duplicated entry"
  [ "$(grep -c '^Exec=' "$entry")" = 1 ] || fail "Exec lines"
  [ -z "$(find "$data" -name '.comet*')" ] || fail "temp files left behind"
  # A user-level icon cache is only ever refreshed, never created.
  [ ! -e "$data/icons/hicolor/icon-theme.cache" ] || fail "created a hicolor icon cache"
  if command -v desktop-file-validate >/dev/null 2>&1; then
    desktop-file-validate "$entry" || fail "desktop-file-validate"
  fi
}

for installer in tarball release; do
  run() { "run_$installer" "$@"; }

  # Default XDG location, then a re-run (how updates are installed) is stable.
  home="$WORK/$installer-a/home"; mkdir -p "$home"
  mkdir -p "$home/.local/bin"
  for profile in .comet-native .comet-native-staging .crew-upstream-canary .crew-staging .crew .zeron; do
    mkdir -p "$home/$profile"
    printf 'preserve' >"$home/$profile/sentinel"
  done
  printf 'old launcher' >"$home/.local/bin/comet"
  run "$home"
  check "$home" "$home/.local/share"
  entry="$home/.local/share/applications/comet.desktop"
  grep -qxF "Exec=$home/.crew/app/current/comet %u" "$entry" || fail "Exec line"
  before="$(cat "$entry")"
  first="$(readlink "$home/.crew/app/current")"
  run "$home"
  check "$home" "$home/.local/share"
  [ "$before" = "$(cat "$entry")" ] || fail "re-run changed the entry"
  printf '# changed build, same version\n' >>"$WORK/pkg/$PKG/comet"
  run "$home"
  second="$(readlink "$home/.crew/app/current")"
  [ "$first" != "$second" ] || fail "same-version new artifact not installed"
  [ -x "$first/comet" ] || fail "old artifact was deleted"
  for profile in .comet-native .comet-native-staging .crew-upstream-canary .crew-staging .crew .zeron; do
    [ "$(cat "$home/$profile/sentinel")" = preserve ] || fail "changed $profile"
  done
  printf 'changed web bundle for %s' "$installer" >"$WORK/pkg/$PKG/crew-web-contract.json"
  run "$home"
  third="$(readlink "$home/.crew/app/current")"
  [ "$second" != "$third" ] || fail "same-binary new bundle not installed"
  [ -x "$second/comet" ] || fail "previous bundle was deleted"

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
  want="Exec=\"$WORK/$installer-d/"'h o\\$me\\"x%%y'"/.crew/app/current/comet\" %u"
  grep -qxF "$want" "$home/.local/share/applications/comet.desktop" \
    || fail "$installer: Exec quoting"
  echo "ok: $installer installer"
done

# Let a second real installer win after the first copies but before it renames.
# The loser must reuse that immutable release, not delete it or nest its stage.
mkdir -p "$WORK/race-bin"
cat >"$WORK/race-bin/cp" <<'COPY'
#!/bin/bash
set -euo pipefail
/bin/cp "$@"
if [[ "$1" == -R ]]; then
  PATH=/usr/bin:/bin bash "$FIXTURE_INSTALLER"
  touch "$(readlink "$HOME/.crew/app/current")/race-winner"
fi
COPY
chmod 755 "$WORK/race-bin/cp"
home="$WORK/concurrent/home"
mkdir -p "$home"
run_tarball "$home" PATH="$WORK/race-bin:/usr/bin:/bin" FIXTURE_INSTALLER="$WORK/pkg/$PKG/install.sh"
winner="$(readlink "$home/.crew/app/current")"
[ -f "$winner/race-winner" ] || fail "concurrent installer replaced the winning release"
[ -z "$(find "$home/.crew/app" -name '.install-*')" ] || fail "concurrent installer left or nested its stage"

# A corrupt download cannot relink current, and credentials never reach curl's
# argv/environment (enforced by the fake above).
home="$WORK/release-a/home"
current="$(readlink "$home/.crew/app/current")"
prepare_feed
printf 'corrupt' >>"$WORK/feed/$PKG.tar.gz"
if invoke_release "$home" >"$WORK/out.log" 2>&1; then
  fail "release installer accepted a checksum mismatch"
fi
grep -q 'checksum mismatch' "$WORK/out.log" || fail "missing checksum error"
[ "$(readlink "$home/.crew/app/current")" = "$current" ] || fail "corrupt release changed current"

# An ambiguous checksum manifest cannot authorize installer replacement either.
prepare_feed
sum="$(cat "$WORK/feed/scaffold-SHA256SUMS")"
printf '%s\n' "$sum" >>"$WORK/feed/scaffold-SHA256SUMS"
if invoke_release "$home" >"$WORK/out.log" 2>&1; then
  fail "release installer accepted duplicate checksum entries"
fi
grep -q 'expected one SHA-256 entry' "$WORK/out.log" || fail "missing duplicate checksum error"
[ "$(readlink "$home/.crew/app/current")" = "$current" ] || fail "ambiguous release changed current"

# A verified package whose executable cannot load must not replace current.
printf '#!/bin/sh\nexit 1\n' >"$WORK/pkg/$PKG/comet"
prepare_feed
if invoke_release "$home" >"$WORK/out.log" 2>&1; then
  fail "release installer accepted an unloadable executable"
fi
grep -q 'Crew could not start' "$WORK/out.log" || fail "missing loader error"
[ "$(readlink "$home/.crew/app/current")" = "$current" ] || fail "unloadable release changed current"
[ -z "$(ls -A "$WORK/tmp")" ] || fail "failed install left files in TMPDIR"
echo 'ok: release integrity and rollback'
