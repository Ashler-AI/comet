#!/bin/bash
# Ad-hoc signatures only; distribution identity is applied at export, never here.
set -euo pipefail
archive="${1:?Usage: prepare-staging-archive.sh ARCHIVE BUILD_NUMBER}"
build="${2:?Expected build number required}"
app="$archive/Products/Applications/Zeron.app"
test "$(plutil -extract CFBundleIdentifier raw "$app/Info.plist")" = ai.ashler.crew.staging
test "$(plutil -extract CFBundleVersion raw "$app/Info.plist")" = "$build"
test "$(plutil -extract CFBundleIdentifier raw "$app/PlugIns/ZeronLiveActivity.appex/Info.plist")" = ai.ashler.crew.staging.LiveActivity
entitlements="$(mktemp)"
trap 'rm -f "$entitlements"' EXIT
plutil -create xml1 "$entitlements"
plutil -insert aps-environment -string production "$entitlements"
find "$app/Frameworks" -maxdepth 1 -name '*.framework' -exec codesign --force --sign - {} \;
codesign --force --sign - "$app/PlugIns/ZeronLiveActivity.appex"
codesign --force --sign - --entitlements "$entitlements" "$app"
codesign --verify --deep --strict "$app"
