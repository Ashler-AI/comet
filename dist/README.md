# Packaging

App icons are sourced from `assets/brand`:

| Target | Source |
| --- | --- |
| macOS `.icns` | `assets/brand/png/crew-icon-macos-1024.png` |
| Linux hicolor | `assets/brand/png/crew-icon-1024.png` and `assets/brand/crew-icon.svg` |
| iOS AppIcon | `assets/brand/png/crew-icon-1024.png` copied to `Assets.xcassets/AppIcon.appiconset/AppIcon1024.png` |

## Linux (implemented)

```sh
scripts/package-linux.sh            # release build (thin LTO, stripped)
PROFILE=debug scripts/package-linux.sh   # fast smoke package
```

Produces `target/package/comet-<version>-linux-<arch>.tar.gz` containing:

- `comet` — the binary (headed by default; `comet headless` runs the engine alone)
- `comet.desktop` — XDG desktop entry
- `comet.png` / `comet.svg` — raster and scalable Crew app icons
- `install.sh` — installs into `~/.local/{bin,share/applications,share/icons}`,
  including both the `1024x1024` and scalable hicolor icon paths

The release profile in the root `Cargo.toml` sets `lto = "thin"` and
`strip = "symbols"` for distribution builds.

## macOS

Use an Apple-silicon macOS host with the checked-in Rust toolchain and Xcode
command-line tools. Packaging builds the native `comet` executable; it never
creates a shell wrapper that launches another installed app.

### Signed distribution (default)

Provision a **Developer ID Application** certificate and its private key in a
keychain. Store notarization credentials with `xcrun notarytool store-credentials`
under a profile you choose, then run:

```sh
CODESIGN_IDENTITY="Developer ID Application: Your Organization (ABCDE12345)" \
COMET_MACOS_SIGNING_TEAM_ID=ABCDE12345 \
NOTARYTOOL_KEYCHAIN_PROFILE=crew-distribution \
scripts/package-macos.sh
```

`CODESIGN_IDENTITY` must be the exact certificate name or SHA-1 fingerprint of a
valid Developer ID Application identity. `COMET_MACOS_SIGNING_TEAM_ID` is the
expected Apple team (exactly 10 uppercase alphanumeric characters), independently
configured rather than inferred from signing or notarization credentials. The
selected certificate's team must match before compilation or notarization.
The validated team is compiled into the updater's distribution trust policy.
`CODESIGN_KEYCHAIN` optionally restricts signing and the notarization profile to
one keychain. Missing or invalid credentials fail before compilation; there is
no automatic ad-hoc fallback. The notarization profile is also authenticated
before compilation.

The script signs the executable and bundle with hardened runtime, a secure
timestamp, and an explicit version-independent designated requirement: exact
bundle identifier, Apple's Developer ID Application trust chain, and the leaf
certificate's team OU. It never pins an executable cdhash, certificate serial,
certificate fingerprint, or app version. It notarizes and staples the app, then
creates, signs, notarizes and staples the DMG. The read-only verifier checks the
final app and the app extracted from the updater archive without re-signing.
Signature, stable requirement, ticket and Gatekeeper checks must succeed.
Apple notarization requires network access and working Apple Developer
credentials. Failures stop packaging; do not distribute intermediate files.

Production preserves the installed identity **`ai.ashler.comet`**, bundle name
**`Crew.app`**, executable `Contents/MacOS/comet`, and updater artifact contract:

- `target/package/comet-<version>-macos-arm64.dmg`
- `target/package/comet-<version>-macos-arm64-app.tar.gz` (contains `Crew.app`)

Both contain the same signed, stapled app. Keep the bundle identifier and Apple
team stable across releases. Developer ID certificate renewal/rotation within
that team preserves the designated requirement; changing teams is an identity
migration, not routine credential rotation. Do not change the configured team
to bypass a failed release verification.

Previous ad-hoc installs have cdhash-bound permission identities and need a
one-time regrant when moving to this Developer ID-signed identity. Users grant
Accessibility and Screen Recording to the actual running Crew app in macOS
System Settings; signing cannot grant these permissions. Subsequent versions
with the same stable requirement preserve the identity used by those grants.

The installed updater uses macOS's built-in `codesign`, `csreq`, and Gatekeeper
(`spctl`) checks; customers do not need Xcode or Command Line Tools. The `stapler`
checks below run only on packaging/release verification hosts. The updater
checks the signed identity, stable requirement, and Gatekeeper acceptance again
after copying the bundle and before moving the installed app.

To verify an extracted release without signing or notarization credentials:

```sh
COMET_MACOS_SIGNING_TEAM_ID=ABCDE12345 \
scripts/verify-macos-app.sh /path/to/Crew.app ai.ashler.comet
```

The verifier is read-only and rejects ad-hoc, wrong-team, wrong-identifier,
unstable-requirement, damaged, unstapled, and untrusted bundles. Staging uses
`ai.ashler.comet.staging` as its expected identifier instead.

### Independent native staging app

```sh
COMET_PACKAGE_ENVIRONMENT=staging \
CODESIGN_IDENTITY="Developer ID Application: Your Organization (ABCDE12345)" \
COMET_MACOS_SIGNING_TEAM_ID=ABCDE12345 \
NOTARYTOOL_KEYCHAIN_PROFILE=crew-distribution \
scripts/package-macos.sh
```

This builds **`Crew Staging.app`** with the stable distinct bundle identity
**`ai.ashler.comet.staging`** and compiles both `COMET_DEFAULT_ENVIRONMENT=staging`
and `COMET_PACKAGE_ENVIRONMENT=staging` into its own native executable. Build
outputs use `target/staging`; distribution outputs are isolated under
`target/package/staging/comet-staging-<version>-macos-arm64*`. Production defaults
to `COMET_PACKAGE_ENVIRONMENT=production`; an explicitly conflicting
`COMET_DEFAULT_ENVIRONMENT` is rejected rather than silently building the wrong
environment.

Standalone staging defaults to `~/.comet-native-staging`, managed worktrees under
`~/.comet-native-staging/worktrees`, IPC port `27655`, and launchd label
`ai.ashler.comet.staging`, while preserving explicit runtime overrides. Do not
point staging's `COMET_WORKTREES_DIR` at production's managed root: that root
defines which checkouts staging may automatically clean up. Staging has its own
macOS permission grants. Its bundle deliberately does
not register the shared `comet://` invitation scheme, which has no environment
discriminator, so installing it cannot steal production invitation links.

**Standalone staging is not the release staging channel.** On a fresh build
(`candidate_run_id` empty), the release workflow's `macos` job packages, signs,
notarizes, and verifies both the production-default, production-identity
`Crew.app` and standalone `Crew Staging.app`, using the same CI credentials below.
Production artifacts remain in `macos-arm64`; standalone staging is uploaded
separately as the Actions artifact `macos-staging-arm64`, containing:

- `comet-staging-<version>-macos-arm64.dmg`
- `comet-staging-<version>-macos-arm64-app.tar.gz` (contains `Crew Staging.app`)
- `SHA256SUMS`

Candidate assembly downloads only production desktop and selected Linux
artifacts, never standalone staging. Release staging promotion tests the exact
production candidate before the same bytes are promoted to production; neither
release feed publishes `Crew Staging.app` or changes updater filenames. Reusing
`candidate_run_id` skips the entire `macos` job, including standalone staging:
download that app from the original fresh build's `macos-staging-arm64` artifact.
The standalone staging executable is unmanaged by the release updater and rejects
update staging/application, preventing the production bundle from replacing its
identity. Install a newly packaged standalone staging artifact to update it.

### Explicit local-only ad-hoc packaging

```sh
COMET_MACOS_SIGNING=adhoc scripts/package-macos.sh
COMET_MACOS_SIGNING=adhoc COMET_PACKAGE_ENVIRONMENT=staging scripts/package-macos.sh
```

Ad-hoc mode is rejected in CI and when `CODESIGN_IDENTITY` is also supplied. It
does not require a team or notarization profile, skips notarization, and writes only to `target/package/local` or
`target/package/staging/local`, never the distribution artifact paths. These
bundles are for local development, not distribution or durable macOS permission
identity. Do not use them to replace a signed installation for permission testing.

### Release CI credentials

`.github/workflows/release.yml` requires these repository/organization Actions
secrets accessible to the `macos` job (their availability is not implied by the
checked-in workflow):

Also set the public Actions variable **`MACOS_SIGNING_TEAM_ID`** to the expected
10-character Apple team. This immutable-by-policy release trust root is exposed
as `COMET_MACOS_SIGNING_TEAM_ID`, including credential-free candidate verification.
It must not be derived from the notarization secret or changed for certificate
renewal. `MACOS_NOTARY_TEAM_ID` must match it; CI rejects a conflict before
importing credentials.

| Secret | Value |
| --- | --- |
| `MACOS_CERTIFICATE_P12_BASE64` | Base64-encoded Developer ID Application certificate **and private key**, exported as PKCS#12 |
| `MACOS_CERTIFICATE_PASSWORD` | Nonempty password protecting that PKCS#12 export |
| `MACOS_CODESIGN_IDENTITY` | Exact Developer ID Application certificate name or SHA-1 fingerprint |
| `MACOS_NOTARY_APPLE_ID` | Apple account authorized to notarize for the signing team |
| `MACOS_NOTARY_APP_PASSWORD` | App-specific password for that Apple account |
| `MACOS_NOTARY_TEAM_ID` | Developer team ID matching the signing identity |

The job fails closed when any credential is absent. It imports the certificate
into a temporary, explicitly selected keychain and appends that keychain to the
runner's search list so `codesign` can discover its private key and certificate
chain. Existing search entries and the default keychain are preserved. The job
authenticates notarization and dry-runs a real signature before compilation,
removes the PKCS#12 file after import, and deletes the keychain (including its
search-list entry) in an `always()` cleanup step. No certificates or credentials
are checked into this repository. The
workflow must not enable shell tracing around secrets.

Existing `candidate_run_id` promotion still reuses an immutable prior candidate
without rebuilding or re-signing it. Every candidate must pass the
`verify-macos-candidate` release gate before publishing, including reused candidates. Older ad-hoc
or otherwise unverifiable candidates are rejected, not retroactively signed or
notarized. For the initial signed rollout, select a candidate produced by this
signing workflow.
