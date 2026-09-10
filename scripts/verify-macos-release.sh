#!/usr/bin/env bash
# Read-only verification for newly built AND reused release candidates.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ $# != 2 ]]; then
  echo "Usage: verify-macos-release.sh CANDIDATE_DIR VERSION" >&2
  exit 1
fi
if [[ ! "${COMET_MACOS_SIGNING_TEAM_ID:-}" =~ ^[A-Z0-9]{10}$ ]]; then
  echo "COMET_MACOS_SIGNING_TEAM_ID must identify the expected Developer ID team." >&2
  exit 1
fi
VERSION="$2"
if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([+-][0-9A-Za-z.-]+)?$ ]]; then
  echo "Invalid release version." >&2
  exit 1
fi
CANDIDATE="$(cd "$1" && pwd)"
DMG="$CANDIDATE/comet-$VERSION-macos-arm64.dmg"
ARCHIVE="$CANDIDATE/comet-$VERSION-macos-arm64-app.tar.gz"
test -f "$DMG"
test -f "$ARCHIVE"
(cd "$CANDIDATE" && shasum -a 256 --check desktop-SHA256SUMS)

TEMP="$(mktemp -d)"
MOUNT="$TEMP/volume"
MOUNTED=false
cleanup() {
  local status=$?
  trap - EXIT
  if [[ "$MOUNTED" == true ]] && ! hdiutil detach "$MOUNT" >/dev/null; then
    echo "Could not detach verification volume $MOUNT; leaving temporary files intact." >&2
    exit 1
  fi
  rm -rf "$TEMP"
  exit "$status"
}
trap cleanup EXIT
mkdir "$TEMP/updater" "$MOUNT"
tar -xzf "$ARCHIVE" -C "$TEMP/updater"
APP="$TEMP/updater/Crew.app"
bash "$ROOT/scripts/verify-macos-app.sh" "$APP" ai.ashler.comet
for key in CFBundleShortVersionString CFBundleVersion; do
  test "$(/usr/libexec/PlistBuddy -c "Print :$key" "$APP/Contents/Info.plist")" = "$VERSION"
done

# The DMG's identifier can include its version; only the application's identity
# owns TCC grants. Its signer must still be the same expected Developer ID team.
REQUIREMENT="anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] exists and certificate leaf[field.1.2.840.113635.100.6.1.13] exists and certificate leaf[subject.OU] = \"$COMET_MACOS_SIGNING_TEAM_ID\""
/usr/bin/codesign --verify --strict --test-requirement "=$REQUIREMENT" "$DMG"
/usr/bin/xcrun stapler validate "$DMG"
/usr/sbin/spctl --assess --type open --context context:primary-signature "$DMG"
hdiutil attach -readonly -nobrowse -mountpoint "$MOUNT" "$DMG" >/dev/null
MOUNTED=true
bash "$ROOT/scripts/verify-macos-app.sh" "$MOUNT/Crew.app" ai.ashler.comet
/usr/bin/diff -qr "$APP" "$MOUNT/Crew.app"
echo "Verified Crew $VERSION: same-team signed, notarized DMG and updater bundle match."
