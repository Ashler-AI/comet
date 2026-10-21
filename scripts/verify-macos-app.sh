#!/usr/bin/env bash
# Read-only distribution gate; no signing credentials, mutation, or re-signing.
set -euo pipefail

fail() { echo "Crew macOS verification: $*" >&2; exit 1; }
[[ $# == 2 ]] || fail "usage: $0 BUNDLE BUNDLE_ID"
APP="$1"
BUNDLE_ID="$2"
case "$BUNDLE_ID" in
  ai.ashler.comet|ai.ashler.comet.staging) ;;
  *) fail "unsupported bundle identifier: $BUNDLE_ID" ;;
esac
[[ "${COMET_MACOS_SIGNING_TEAM_ID:-}" =~ ^[A-Z0-9]{10}$ ]] || fail "COMET_MACOS_SIGNING_TEAM_ID must be 10 uppercase alphanumeric characters"
[[ "$(uname -s)" == Darwin ]] || fail "verification requires macOS"
[[ -d "$APP" && ! -L "$APP" ]] || fail "bundle must be a real directory"
PLIST="$APP/Contents/Info.plist"
EXECUTABLE="$APP/Contents/MacOS/comet"
[[ -f "$PLIST" && ! -L "$PLIST" ]] || fail "bundle Info.plist is missing or symlinked"
[[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$PLIST")" == "$BUNDLE_ID" ]] || fail "bundle identifier mismatch"
[[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$PLIST")" == comet ]] || fail "unexpected bundle executable"
[[ -f "$EXECUTABLE" && -x "$EXECUTABLE" && ! -L "$EXECUTABLE" ]] || fail "bundle executable is missing, non-executable, or symlinked"

REQUIREMENT="identifier \"$BUNDLE_ID\" and anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] exists and certificate leaf[field.1.2.840.113635.100.6.1.13] exists and certificate leaf[subject.OU] = \"$COMET_MACOS_SIGNING_TEAM_ID\""
EXPECTED_REQUIREMENT="$(csreq -r "=$REQUIREMENT" -t)"
verify_code() {
  local code="$1" details requirement="" line
  codesign --verify --deep --strict --verbose=2 --test-requirement "=$REQUIREMENT" "$code"
  details="$(codesign --display --verbose=4 "$code" 2>&1)"
  details=$'\n'"$details"$'\n'
  [[ "$details" == *$'\nIdentifier='"$BUNDLE_ID"$'\n'* ]] || fail "code identifier mismatch: $code"
  [[ "$details" == *$'\nTeamIdentifier='"$COMET_MACOS_SIGNING_TEAM_ID"$'\n'* ]] || fail "code signing team mismatch: $code"
  details="$(codesign --display -r- "$code" 2>&1)"
  while IFS= read -r line; do
    if [[ "$line" == 'designated => '* ]]; then
      [[ -z "$requirement" ]] || fail "multiple designated requirements: $code"
      requirement="${line#designated => }"
    fi
  done <<< "$details"
  [[ -n "$requirement" ]] || fail "missing designated requirement: $code"
  # Parse both expressions, so codesign's display quoting and exists comments
  # cannot change the comparison. Only the stable policy is accepted: no cdhash,
  # certificate fingerprint/serial, version, alternate team, or extra clauses.
  [[ "$(csreq -r "=$requirement" -t)" == "$EXPECTED_REQUIREMENT" ]] || fail "designated requirement is not the stable Crew distribution policy: $code"
}
verify_code "$APP"
verify_code "$EXECUTABLE"
xcrun stapler validate "$APP"
spctl --assess --type execute --verbose=2 "$APP"
echo "Verified Crew distribution bundle: $APP ($BUNDLE_ID, team $COMET_MACOS_SIGNING_TEAM_ID)"
