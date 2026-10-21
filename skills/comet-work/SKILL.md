---
name: comet-work
description: Standard reviewed Crew release lifecycle, staging verification, and exact-candidate promotion.
---

# Crew release lifecycle

Use this path for this repository's single upstream-based implementation. Internal Cargo packages remain `zeron`; the published executable remains `comet`. Read [packaging](../../dist/README.md) before building. Never copy a user's personal agent or workstation rules into release policy.

## 1. Verify the reviewed source

Run the narrow checks required by the change and the separate credential-free `release-contracts.yml` workflow. Existing native UI, Linux, Windows, voice and release-profile workflows remain independent of release publication. Exercise the actual changed behavior in an isolated native instance; never run an unimported legacy profile against the new engine. Profile migration is offline, copy-first, explicitly verified and single-writer.

Packaging uses the checked-in Rust toolchain, locked dependencies and `cargo build -p zeron --bin zeron`. Build/test execution requires the current task's authorization. Do not treat release packaging as a substitute for regression CI.

## 2. Land reviewed source

Use the repository's reviewed merge controls. Record the exact merged commit and the successful verification run. Do not invent a direct-push policy. Production promotion requires the release source to be reachable from `origin/main`; a push, merge or version tag never publishes a release automatically.

## 3. Publish and verify staging

The 0.2.114 cutover requires release compatibility marker `scaffold.comet-runtime.v2` in both repositories. First build, sign, verify and publish the immutable staging candidate without claiming Platform deployment:

```sh
gh workflow run release.yml --repo Ashler-AI/comet --ref main \
  -f version=0.2.114 \
  -f release_surface=desktop-and-scaffold \
  -f promotion_target=staging-candidate \
  -f scaffold_runtime_deployment=unchanged
```

Wait for that run to succeed and record its run ID as `candidate_run_id`. This phase publishes only `releases/0.2.114/*` in the private staging bucket; it does not move channel/installer aliases, latest pointers or Scaffold pins. Signature, checksum, provenance and bucket-privacy gates still apply. `unchanged` here is not a claim that the new runtime is deployed: candidate-only publication stops before the deployment acknowledgement guard.

Use the verified immutable versioned manifest and Linux archive tuple to deploy compatible staging Platform support and complete its required migration. Verify the resulting runtime/admission/browser contracts before advancing Crew channels. Preserve existing snapshots and profile/wire v1; do not redirect them blindly to the new runtime.

Only after those Platform deployment/migration checks succeed, promote the exact successful candidate without rebuilding or re-signing:

```sh
gh workflow run release.yml --repo Ashler-AI/comet --ref main \
  -f version=0.2.114 \
  -f release_surface=desktop-and-scaffold \
  -f promotion_target=staging \
  -f scaffold_runtime_deployment=staging-deployed \
  -f candidate_run_id="${candidate_run_id:?Set the successful immutable candidate run ID}"
```

Wait for exact-candidate verification and staging channel readback to succeed. The runtime guard still rejects a desktop-only breaking-contract promotion or a missing deployment acknowledgement. `promotion_target=none` only builds and verifies private Actions artifacts. Use the actual workspace version for later releases.

The release compatibility marker is not the stored profile or wire schema version: those remain v1. Never rewrite profiles merely to match the release marker. For a future release whose compatibility marker is unchanged, use `scaffold_runtime_deployment=unchanged`.

The workflow builds Linux and both independently signed/notarized macOS identities, verifies candidate signatures, uploads immutable objects to the environment's private GCS bucket, and reads back the selected staging channels. Preserve its run URL, run ID, candidate SHA-256, source commit, channel manifests and Linux archive/raw executable checksums. Do not confuse the archive SHA-256 used for image pinning with `COMET_SHA256SUMS`' extracted executable digest.

Native Edge uses the separate staging-only `deploy.yml` workflow. Dispatch its reviewed source with `--ref`; `source_sha` must equal that ref's resolved full commit SHA. The workflow rejects a different SHA before checkout or dependency/cache access, and every job checks out the dispatched commit. For a candidate-only run:

```sh
gh workflow run deploy.yml --repo Ashler-AI/comet --ref "${reviewed_ref:?Set the reviewed source ref}" \
  -f source_sha="${reviewed_sha:?Set the matching full commit SHA}" -f target=candidate
```

Use `target=staging` only with explicit staging deployment authorization; this builds and verifies an immutable candidate before secret synchronization and deployment. Environment protection settings must be checked separately, not assumed to require reviewers. Native Worker bindings are declared in `edge/src/env.ts`; the existing `typecheck` and unit/workerd tests validate them, without a generated binding step.

The `comet-release-staging` environment must also bind `CLOUDFLARE_ACCOUNT_ID`, `CLOUDFLARE_API_TOKEN`, `GCP_RELEASE_SERVICE_ACCOUNT_EMAIL` and `GCP_RELEASE_SERVICE_ACCOUNT_PRIVATE_KEY` for Edge release-feed synchronization. The release reader is distinct from the OIDC publisher identity. Missing bindings fail before Worker deployment; successful candidate build/typecheck does not establish reader access.

## 4. Manual-verification stop

Report the staging run and version, then obtain explicit operator approval before any production action. A successful workflow, silence, or a source change is not production authorization. The operator must update/restart the signed Crew Staging app and verify the changed behavior. Keep `Crew.app`, production profiles, production release pointers and production Scaffold pins untouched during a staging-only assignment.

The 0.2.114 native cutover is staging-only: do not promote production or repin production Scaffold.

## 5. Future production promotion (separate authorization required)

Only after explicit approval, dispatch `release.yml` from `main` with `promotion_target=production` and `candidate_run_id` naming the original successful immutable candidate build run used for staging—not the later staging promotion run—using the same version and release surface. Reuse the exact candidate bytes; never rebuild, re-sign, replace, or patch a published version. The workflow's `comet-release-production` environment is a separate approval/credential boundary. It excludes standalone staging artifacts from production publication.

Scaffold deployment is a separate Platform operation: carry the verified immutable tuple (version, private bucket, Linux archive SHA-256), use that repository's documented staging or production workflow, and let its release machinery generate image locks. Never hand-edit generated image references or change a production pin as a side effect of Crew staging publication. Establish the documented operator command and authorization before any platform action.
