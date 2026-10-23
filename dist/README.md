# Crew Canary packaging

This upstream-port branch delivers **private/manual canary artifacts**, not a
replacement for signed Crew staging/production releases. It does not publish a
shared bucket, public manifest, `latest` pointer, or release channel. Packaging
does not modify existing Crew apps, services or profiles. The separately approved
staging broker deployment is recorded in the [port status](../docs/UPSTREAM-PORT-PLAN.md).

## Brand artwork

`dist/zeron.svg` is the Crew mark: the original 46-unit four-piece square in
`#fafafa`, a 10-unit `#09de5e` core, and solid black. No frame, shadow, or nested
tile is added. The mark occupies 46/72 of the artboard, preserving its geometry
while leaving app-icon safe area. Internal `zeron` filenames remain compatible
with existing consumers.

Regenerate the desktop, launcher, landing-page and iOS artwork with
`python3 scripts/gen-brand-icons.py` (Pillow required only for regeneration).
The shared and iOS PNGs are opaque 1024×1024 RGB; iOS applies its own corner mask
and derives dark/tinted appearances from this universal asset. The macOS PNG is
1024×1024 RGBA with only the platform tile's exterior transparent. Packaging
uses the existing `sips`/`iconutil` path to make `.icns`; the dev launcher refreshes
its cached icon when the source changes. Repackage/reinstall Canary to pick up
new bundle assets; generation alone never changes a running/installed app.


## macOS

From this worktree on a macOS host:

```sh
CI=false COMET_PACKAGE_ENVIRONMENT=canary COMET_DEFAULT_ENVIRONMENT=canary \
  COMET_MACOS_SIGNING=adhoc ASHLER_INCREMENTAL_TSC_CHECKS=false \
  scripts/package-macos.sh
open --env ASHLER_INCREMENTAL_TSC_CHECKS=false --env COMET_IPC_PORT=27954 \
  --env "COMET_DATA_DIR=$HOME/.crew-upstream-canary" \
  'target/package/canary/local/Crew Canary.app' --args \
  --ipc-port 27954 --data-dir "$HOME/.crew-upstream-canary"
```

The script builds the upstream Cargo package/binary `zeron` with `--locked
--release` and copies that exact artifact into the bundle. `scripts/local-cargo.py`
selects the shared local Cargo cache, limits build concurrency and snapshots the
binary while holding the build lock. Internal Cargo/crate and artwork filenames
deliberately retain upstream names.

Pin the launch coordinates when running from another Crew agent session: its
inherited `COMET_IPC_PORT` addresses that session's engine, not Canary.

The bundle has identifier `ai.ashler.crew.upstream-canary`, display name
`Crew Canary`, and registers **only** `crew-upstream-canary:`. Runtime defaults
are `~/.crew-upstream-canary` and IPC port `27954`; the bundle does not replace
Crew.app or register its production/staging schemes. For a persistent install,
quit only Crew Canary, preserve the previous app, and copy the complete new bundle
to `/Applications/Crew Canary.app`. Do not overlay an existing bundle: obsolete
resources invalidate its signature. Verify the installed copy with
`codesign --verify --deep --strict '/Applications/Crew Canary.app'` before launch.
Keep the existing data directory, device identity and native session histories.
New worktrees default to the selected data directory's `worktrees` folder,
not another installed client's `~/.zeron/worktrees`. Explicit existing worktree
paths and user-selected custom locations remain unchanged.

Artifacts:

- `target/package/canary/local/Crew Canary.app`
- `target/package/canary/local/crew-upstream-canary-<version>-macos-<arch>.dmg`
- `target/package/canary/local/crew-upstream-canary-<version>-macos-<arch>-app.tar.gz`

Ad-hoc signing requires the explicit local-only mode above; it is not notarized
distribution. Distribution mode requires `CODESIGN_IDENTITY`,
`COMET_MACOS_SIGNING_TEAM_ID` and `NOTARYTOOL_KEYCHAIN_PROFILE`, and omits the
`local` directory. Neither signing nor notarization enables a release channel.

For an additional debug-only instance, `scripts/run-macos-dev.sh` builds
`target/macos-dev/Crew Canary Dev.app`, uses bundle identifier
`ai.ashler.crew.upstream-canary.dev`, a separate data directory under
`target/macos-dev/data`, and IPC port `27955`. `COMET_DEV_BUILD_ONLY=1` builds
without launching. `COMET_DEV_DATA_DIR`, `COMET_DEV_IPC_PORT` and
`COMET_DEV_CODESIGN_IDENTITY` select the development profile and signing identity.

## Linux and Scaffold runtime

```sh
ASHLER_INCREMENTAL_TSC_CHECKS=false scripts/package-linux.sh
# PROFILE=debug is available for local fixture packages.
```

Output: `target/package/crew-upstream-canary-<version>-linux-<arch>.tar.gz`.
The archive root has the same stem and contains `comet`, licenses,
`crew-upstream-canary.desktop`, `crew-upstream-canary.png`, and `install.sh`.

The manual installer stores immutable binary-digest-qualified versions under
`~/.crew-upstream-canary/app/<version>-<sha256>`, changes only that directory's
`current` link, and links `~/.local/bin/crew-upstream-canary`. Desktop entry and
icon names are also canary-specific. It does not overwrite a shared `comet`
launcher, touch existing Crew/Zeron data, or install/restart any service. A new
artifact of the same workspace version gets a distinct installation directory.
The Scaffold handoff must select this exact binary with its SHA-256 and the
checked-in `scaffold-runtime-version.txt` contract, not a mutable release feed.

`bash scripts/test-linux-desktop-entry.sh` is an offline Linux regression for
launcher quoting, XDG paths, repeated installation, same-version new artifacts,
and preservation of existing installations. It uses a throwaway HOME.

## Build-only workflow and update policy

`.github/workflows/release.yml` is **Crew Canary artifacts**. Manual dispatch
requires the exact reviewed SHA and builds that immutable `github.sha`, uploading
private Actions artifacts with archive `SHA256SUMS`, raw Linux executable
`COMET_SHA256SUMS`, and `provenance.json`, and has only `contents: read`.
`COMET_SHA256SUMS` names the extracted archive-root `comet` file; its digest
is the value for `COMET_SCAFFOLD_RUNTIME_SHA256`, not the archive digest.
It does not deploy, publish GitHub Releases, use shared release credentials, or
change signed stable/staging channels. Dispatch the registered workflow after review:

```sh
gh workflow run release.yml --repo Ashler-AI/comet --ref crew/upstream-canary \
  -f "reviewed_sha=${REVIEWED_SHA:?Set the exact reviewed integration commit}"
```

Verify that the run's `headSha` is the reviewed commit before consuming its private
artifacts. Edge deployment is a separate operator action: from `edge/`, run
`npx wrangler deploy --env canary` only after that revision passes CI. The isolated
controller uses Platform's `internal/scaffold/scripts/deploy-upstream-canary.mjs`.

`zeron-update::AUTOMATIC_UPDATES_SUPPORTED` is false for this branch. Direct
CLI/UI checks, staging and apply paths fail closed before a shared channel can
be contacted or an install replaced. Environment URL overrides cannot opt in.
The dormant upstream downloader remains for mergeability; eventual signed Crew
delivery must port the authenticated/signed candidate contract, not merely flip
this constant or expose a public feed. Windows packaging/update assets are
retained upstream tooling, not Crew Canary delivery artifacts; this workflow
does not build or distribute them. iOS is packaged by its separate workstream.

The **refresh agents after Crew updates** preference is retained independently
of per-agent monitoring. The coordinator imports `harnessAutoUpdate` from an
explicitly copied `<data_dir>/update-prefs.json` into `harness-update-prefs.json`
without reading the live legacy store. Subsequent changes persist atomically.
Manual canary delivery preserves the value but does not execute the post-update
hook. Supported signed delivery calls `post_update_refresh(data_dir, true)` on
boot: only a version transition triggers sequential enabled/installed-agent
checks and applicable updates through existing idle/update locks. Explicit
per-agent Off and `COMET_UPDATE_HARNESSES=0|false|no` remain respected. OMP uses
its native `omp update` and `can1357/oh-my-pi` release metadata, never Pi's npm
installer. Local typechecks are intentionally not part of this workflow.

## Managed authority verification

The private runtime reuses the shared desktop/mobile `ScaffoldAuth` verifier.
User bearers verify at startup/sign-in and on generation-matched authentication
failure, not on a freshness timer. Sign-out and profile replacement fence old
responses. Device grants renew near their actual expiry; expired grants are never
served after a failed renewal. Concurrent transient failures share a 1–5-second
fallback cooldown; valid `Retry-After` dates or seconds remain an uncapped minimum.

Canary Edge still validates every protected request. Equivalent concurrent
checks share only their in-flight request and body decoding; token, control
origin, project and required capabilities bind the key. The entry is removed
on settlement, so the next request freshly checks revocation. It does not
immediately retry HTTP 429 or responses carrying `Retry-After`; unavailable
HTTP responses retain that header on the existing 503 contract. No quota or
broker change is part of this fix. Existing profiles and independently pinned runtimes stay in
place; verify each executable and runtime-artifact digest separately. A newer
Mac build already carrying this verifier must not be downgraded to update an
older Linux runtime pin.
