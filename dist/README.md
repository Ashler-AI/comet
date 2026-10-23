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
ASHLER_INCREMENTAL_TSC_CHECKS=false scripts/package-macos.sh
open --env ASHLER_INCREMENTAL_TSC_CHECKS=false --env COMET_IPC_PORT=27954 \
  --env "COMET_DATA_DIR=$HOME/.crew-upstream-canary" \
  'target/package/Crew Canary.app' --args \
  --ipc-port 27954 --data-dir "$HOME/.crew-upstream-canary"
```

The script builds the upstream Cargo package/binary `zeron` with `--locked
--release`, discovers Cargo's actual target directory, and copies that exact
artifact into `target/package/Crew Canary.app/Contents/MacOS/comet`. Internal
Cargo/crate and artwork filenames deliberately retain upstream names.

Pin the launch coordinates when running from another Crew agent session: its
inherited `COMET_IPC_PORT` addresses that session's engine, not Canary. For the
existing lightweight local build cache, use `CARGO_BUILD_JOBS=2`,
`CARGO_INCREMENTAL=0`, `CARGO_PROFILE_DEV_DEBUG=0` and `CREW_PACKAGE_PROFILE=dev`.
Do not switch these flags between incremental smoke runs: Cargo rebuilds the
dependency graph for a different profile configuration.

The bundle has identifier `ai.ashler.crew.upstream-canary`, display name
`Crew Canary`, and registers **only** `crew-upstream-canary:`. Runtime defaults
are `~/.crew-upstream-canary` and IPC port `27954`; the bundle does not replace
Crew.app or register its production/staging schemes. Move only Crew Canary.app
to Applications if a persistent installation is wanted.
New worktrees default to the selected data directory's `worktrees` folder,
not another installed client's `~/.zeron/worktrees`. Explicit existing worktree
paths and user-selected custom locations remain unchanged.

Artifacts:

- `target/package/Crew Canary.app`
- `target/package/crew-upstream-canary-<version>-macos-<arch>.dmg`
- `target/package/crew-upstream-canary-<version>-macos-<arch>-app.tar.gz`

Ad-hoc signing is the local default. Set `CODESIGN_IDENTITY` for an explicitly
selected Developer ID. Optional `NOTARY_KEY_PATH`, `NOTARY_KEY_ID`, and
`NOTARY_ISSUER_ID` submit to Apple and staple the bundle and disk image. Neither
signing nor notarization creates or enables a release channel.

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
requires the exact reviewed SHA and only builds/packages that immutable
`github.sha`, uploading private Actions artifacts with archive `SHA256SUMS`, raw
Linux executable `COMET_SHA256SUMS`, and `provenance.json`, with `contents: read`.
Behavior checks and typechecks are not deployment-pipeline jobs. Run required
focused checks and actual runtime/inference smokes separately.
`COMET_SHA256SUMS` names the extracted archive-root `comet` file; its digest
is the value for `COMET_SCAFFOLD_RUNTIME_SHA256`, not the archive digest.
The isolated auth maintenance branch `crew/canary-agent-auth-fix-20261022` is
also accepted by the same exact-SHA gate. Dispatch it with that explicit `--ref`;
never move `crew/upstream-canary` backwards or include unrelated staging changes.

It does not deploy, publish GitHub Releases, use shared release credentials, or
change signed stable/staging channels. Dispatch the registered workflow after review:

```sh
gh workflow run release.yml --repo Ashler-AI/comet --ref crew/upstream-canary \
  -f "reviewed_sha=${REVIEWED_SHA:?Set the exact reviewed integration commit}"
```

Verify that the run's `headSha` is the reviewed commit before consuming its private
artifacts. Edge deployment is a separate operator action: from `edge/`, run
`npx wrangler deploy --env canary` after separately verifying the changed Worker
behavior; unrelated desktop/runtime packaging does not block it. The isolated
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

The private runtime reuses the shared desktop/mobile `ScaffoldAuth` verifier:
one credential-bound verification per 60-second freshness window, aged by both
monotonic and wall clocks. Cold starts still verify; sign-out, profile changes
and explicit rejection invalidate authority. Expired success is not an outage
fallback. Concurrent transient failures share a 1–5-second cooldown; valid
`Retry-After` dates or seconds set an uncapped minimum delay.

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
