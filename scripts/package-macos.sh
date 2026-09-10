#!/usr/bin/env bash
# Native macOS bundles. Production preserves Crew.app and comet-<version>-macos-arm64*.
# COMET_PACKAGE_ENVIRONMENT=staging builds an independent Crew Staging.app.
# Distribution requires Developer ID signing and a notarytool keychain profile.
# COMET_MACOS_SIGNING=adhoc explicitly opts into local-only, non-distribution output.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "macOS packaging requires a macOS runner" >&2
  exit 1
fi
WORKSPACE_VERSION="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\([^"]*\)"/\1/p' "$ROOT/Cargo.toml")"
VERSION="${COMET_RELEASE_VERSION:-$WORKSPACE_VERSION}"
if [[ -z "$WORKSPACE_VERSION" || "$VERSION" != "$WORKSPACE_VERSION" ]]; then
  echo "release version '$VERSION' does not match workspace version '$WORKSPACE_VERSION'" >&2
  exit 1
fi
ARCH="$(uname -m)"
if [[ "$ARCH" != "arm64" ]]; then
  echo "macOS releases require an Apple-silicon runner (found $ARCH)" >&2
  exit 1
fi

PACKAGE_ENVIRONMENT="${COMET_PACKAGE_ENVIRONMENT:-production}"
OUT_DIR="$ROOT/target/package"
BUILD_DIR="$ROOT/target"
case "$PACKAGE_ENVIRONMENT" in
  production)
    APP_NAME="Crew"
    BUNDLE_ID="ai.ashler.comet"
    ARTIFACT_PREFIX="comet"
    ;;
  staging)
    APP_NAME="Crew Staging"
    BUNDLE_ID="ai.ashler.comet.staging"
    ARTIFACT_PREFIX="comet-staging"
    OUT_DIR="$OUT_DIR/staging"
    BUILD_DIR="$ROOT/target/staging"
    ;;
  *) echo "COMET_PACKAGE_ENVIRONMENT must be production or staging" >&2; exit 1 ;;
esac
if [[ -n "${COMET_DEFAULT_ENVIRONMENT:-}" && "$COMET_DEFAULT_ENVIRONMENT" != "$PACKAGE_ENVIRONMENT" ]]; then
  echo "COMET_DEFAULT_ENVIRONMENT must match COMET_PACKAGE_ENVIRONMENT" >&2
  exit 1
fi
export COMET_DEFAULT_ENVIRONMENT="$PACKAGE_ENVIRONMENT"
export COMET_PACKAGE_ENVIRONMENT="$PACKAGE_ENVIRONMENT"

SIGNING="${COMET_MACOS_SIGNING:-distribution}"
KEYCHAIN_ARGS=()
if [[ -n "${CODESIGN_KEYCHAIN:-}" ]]; then
  KEYCHAIN_ARGS=(--keychain "$CODESIGN_KEYCHAIN")
fi
case "$SIGNING" in
  distribution)
    if [[ -z "${CODESIGN_IDENTITY:-}" || -z "${NOTARYTOOL_KEYCHAIN_PROFILE:-}" ]]; then
      echo "Distribution requires CODESIGN_IDENTITY (Developer ID Application) and NOTARYTOOL_KEYCHAIN_PROFILE." >&2
      echo "For local-only packaging, explicitly set COMET_MACOS_SIGNING=adhoc." >&2
      exit 1
    fi
    if [[ ! "${COMET_MACOS_SIGNING_TEAM_ID:-}" =~ ^[A-Z0-9]{10}$ ]]; then
      echo "Distribution requires COMET_MACOS_SIGNING_TEAM_ID (10 uppercase alphanumeric characters)." >&2
      exit 1
    fi
    export COMET_MACOS_SIGNING_TEAM_ID
    # Resolve a valid Developer ID certificate/private-key pair before compiling.
    IDENTITY_KEYCHAINS=()
    if [[ -n "${CODESIGN_KEYCHAIN:-}" ]]; then
      IDENTITY_KEYCHAINS=("$CODESIGN_KEYCHAIN")
    fi
    IDENTITIES="$(security find-identity -v -p codesigning ${IDENTITY_KEYCHAINS[@]+"${IDENTITY_KEYCHAINS[@]}"})"
    SIGNING_HASH=""
    IDENTITY_PATTERN='^[[:space:]]*[0-9]+\) ([[:xdigit:]]{40}) "(Developer ID Application: .+)"$'
    while IFS= read -r line; do
      if [[ "$line" =~ $IDENTITY_PATTERN ]]; then
        hash="${BASH_REMATCH[1]}"
        name="${BASH_REMATCH[2]}"
        if [[ "$CODESIGN_IDENTITY" == "$hash" || "$CODESIGN_IDENTITY" == "$name" ]]; then
          if [[ -n "$SIGNING_HASH" ]]; then
            echo "Ambiguous CODESIGN_IDENTITY; select the certificate by SHA-1 fingerprint." >&2
            exit 1
          fi
          SIGNING_HASH="$hash"
        fi
      fi
    done <<< "$IDENTITIES"
    if [[ -z "$SIGNING_HASH" ]]; then
      echo "CODESIGN_IDENTITY is not a valid, available Developer ID Application signing identity." >&2
      exit 1
    fi
    # Validate the actual selected certificate's OU, not only its display name.
    # Read-only keychain inspection precedes cargo and notarization requests.
    CERTIFICATES="$(security find-certificate -a -p ${IDENTITY_KEYCHAINS[@]+"${IDENTITY_KEYCHAINS[@]}"})"
    CERTIFICATE=""
    CERTIFICATE_TEAM=""
    while IFS= read -r line; do
      if [[ "$line" == '-----BEGIN CERTIFICATE-----' ]]; then
        CERTIFICATE="$line"$'\n'
      elif [[ -n "$CERTIFICATE" ]]; then
        CERTIFICATE+="$line"$'\n'
        if [[ "$line" == '-----END CERTIFICATE-----' ]]; then
          fingerprint="$(printf '%s' "$CERTIFICATE" | openssl x509 -noout -fingerprint -sha1)"
          fingerprint="${fingerprint#*=}"
          if [[ "${fingerprint//:/}" == "$SIGNING_HASH" ]]; then
            subject="$(printf '%s' "$CERTIFICATE" | openssl x509 -noout -subject -nameopt sep_multiline,sname,utf8)"
            while IFS= read -r subject_line; do
              if [[ "$subject_line" =~ ^[[:space:]]*OU[[:space:]]*=[[:space:]]*([A-Z0-9]{10})$ ]]; then
                CERTIFICATE_TEAM="${BASH_REMATCH[1]}"
              fi
            done <<< "$subject"
            break
          fi
          CERTIFICATE=""
        fi
      fi
    done <<< "$CERTIFICATES"
    if [[ "$CERTIFICATE_TEAM" != "$COMET_MACOS_SIGNING_TEAM_ID" ]]; then
      echo "CODESIGN_IDENTITY certificate team does not match COMET_MACOS_SIGNING_TEAM_ID." >&2
      exit 1
    fi
    APP_REQUIREMENT="identifier \"$BUNDLE_ID\" and anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] exists and certificate leaf[field.1.2.840.113635.100.6.1.13] exists and certificate leaf[subject.OU] = \"$COMET_MACOS_SIGNING_TEAM_ID\""
    # Authenticate the stored profile before the expensive build as well.
    xcrun notarytool history --keychain-profile "$NOTARYTOOL_KEYCHAIN_PROFILE" \
      ${KEYCHAIN_ARGS[@]+"${KEYCHAIN_ARGS[@]}"} --output-format json >/dev/null
    SIGN_ARGS=(--force --options runtime --timestamp --sign "$SIGNING_HASH" ${KEYCHAIN_ARGS[@]+"${KEYCHAIN_ARGS[@]}"})
    APP_SIGN_ARGS=(--identifier "$BUNDLE_ID" --requirements "=designated => $APP_REQUIREMENT")
    ;;
  adhoc)
    if [[ -n "${CODESIGN_IDENTITY:-}" || ( -n "${CI:-}" && "${CI:-}" != false && "${CI:-}" != 0 ) ]]; then
      echo "Ad-hoc signing is local-only and cannot be combined with CI or CODESIGN_IDENTITY." >&2
      exit 1
    fi
    OUT_DIR="$OUT_DIR/local"
    SIGN_ARGS=(--force --sign -)
    APP_SIGN_ARGS=()
    echo "LOCAL ONLY: ad-hoc signatures are not a stable distribution identity or notarized."
    ;;
  *) echo "COMET_MACOS_SIGNING must be distribution or adhoc" >&2; exit 1 ;;
esac

APP="$OUT_DIR/$APP_NAME.app"
DMG="$OUT_DIR/$ARTIFACT_PREFIX-$VERSION-macos-$ARCH.dmg"
APP_TARBALL="$OUT_DIR/$ARTIFACT_PREFIX-$VERSION-macos-$ARCH-app.tar.gz"

cd "$ROOT"
cargo build --release -p comet --target-dir "$BUILD_DIR"

rm -rf "$APP" "$DMG" "$APP_TARBALL"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
install -m 755 "$BUILD_DIR/release/comet" "$APP/Contents/MacOS/comet"
sed "s/__VERSION__/$VERSION/" "$ROOT/dist/macos/Info.plist" >"$APP/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleIdentifier $BUNDLE_ID" "$APP/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleName $APP_NAME" "$APP/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleDisplayName $APP_NAME" "$APP/Contents/Info.plist"
if [[ "$PACKAGE_ENVIRONMENT" == staging ]]; then
  # The shared comet:// protocol has no environment discriminator. Do not steal
  # production invitation links when both native apps are installed.
  /usr/libexec/PlistBuddy -c "Delete :CFBundleURLTypes" "$APP/Contents/Info.plist"
fi
plutil -lint "$APP/Contents/Info.plist"

# The Dock icon uses the brand's Apple-squircle variant.
ICON_SRC="$ROOT/assets/brand/png/crew-icon-macos-1024.png"
ICONSET="$OUT_DIR/comet.iconset"
rm -rf "$ICONSET" && mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$ICON_SRC" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
  retina=$((size * 2))
  sips -z "$retina" "$retina" "$ICON_SRC" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/comet.icns"
rm -rf "$ICONSET"

# Sign inside-out rather than relying on --deep to repair nested signatures.
codesign "${SIGN_ARGS[@]}" ${APP_SIGN_ARGS[@]+"${APP_SIGN_ARGS[@]}"} "$APP/Contents/MacOS/comet"
codesign "${SIGN_ARGS[@]}" ${APP_SIGN_ARGS[@]+"${APP_SIGN_ARGS[@]}"} "$APP"
codesign --verify --deep --strict --verbose=2 "$APP"

NOTARY_DIR="$(mktemp -d)"
trap 'rm -rf "$NOTARY_DIR"' EXIT
notarize() {
  xcrun notarytool submit "$1" --keychain-profile "$NOTARYTOOL_KEYCHAIN_PROFILE" \
    ${KEYCHAIN_ARGS[@]+"${KEYCHAIN_ARGS[@]}"} --wait --output-format json >"$NOTARY_DIR/result.json"
  if [[ "$(plutil -extract status raw -o - "$NOTARY_DIR/result.json")" != Accepted ]]; then
    echo "Notarization was not accepted; inspect the submission with notarytool log before distributing." >&2
    cat "$NOTARY_DIR/result.json" >&2
    exit 1
  fi
}
if [[ "$SIGNING" == distribution ]]; then
  ditto -c -k --keepParent "$APP" "$NOTARY_DIR/app.zip"
  notarize "$NOTARY_DIR/app.zip"
  xcrun stapler staple "$APP"
  "$ROOT/scripts/verify-macos-app.sh" "$APP" "$BUNDLE_ID"
fi

# Both artifacts contain the same signed, stapled bundle. Release promotion
# continues to reuse this exact production-identity artifact in every channel.
hdiutil create -volname "$APP_NAME" -srcfolder "$APP" -ov -format UDZO "$DMG"
if [[ "$SIGNING" == distribution ]]; then
  codesign "${SIGN_ARGS[@]}" "$DMG"
  notarize "$DMG"
  xcrun stapler staple "$DMG"
  xcrun stapler validate "$DMG"
  codesign --verify --strict --verbose=2 "$DMG"
  spctl --assess --type open --context context:primary-signature --verbose=2 "$DMG"
fi
COPYFILE_DISABLE=1 tar -czf "$APP_TARBALL" -C "$OUT_DIR" "$APP_NAME.app"
if [[ "$SIGNING" == distribution ]]; then
  mkdir "$NOTARY_DIR/updater"
  tar -xzf "$APP_TARBALL" -C "$NOTARY_DIR/updater"
  "$ROOT/scripts/verify-macos-app.sh" "$NOTARY_DIR/updater/$APP_NAME.app" "$BUNDLE_ID"
fi
echo "packaged: $APP_TARBALL"
echo "packaged: $DMG"
