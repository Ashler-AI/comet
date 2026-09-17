---
name: comet-work
description: Standard release lifecycle for any change scoped to this repository alone.
---

# Comet-only work

Use this path only when the complete change is in this repository and `scaffold-runtime-version.txt` stays unchanged. A runtime-contract bump requires the platform-first sequence in `README.md` and is not Comet-only work.

## 1. Test a local Crew instance

Run the repository's local headed demo:

```bash
scripts/dev-demo.sh
# Use scripts/dev-demo.sh --slow to inspect streamed UI states.
```

The script builds `comet`, starts a mock headless engine on port 27921, seeds isolated data under `/tmp/comet-demo-*`, and opens the headed Crew app. Exercise the changed behavior in that instance and run the narrow checks required by the change. Do not substitute tests alone for the local-instance check.

## 2. Land the change on `main`

Use the repository's reviewed merge path, merge the approved change into `main`, and push it to `origin`. Do not release an unmerged commit. Verify the released commit is on remote `main`:

```bash
git fetch origin main
git merge-base --is-ancestor "$commit" origin/main
```

**Open mechanism:** this checkout does not document one required merge command or merge strategy. The release workflow only establishes the invariant above; it rejects a production source that is not reachable from `origin/main`. Use the current repository merge controls rather than inventing a direct-push procedure.

## 3. Publish and verify staging

Read the workspace version and dispatch the complete desktop-and-Scaffold surface from merged source:

```bash
version="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\([^"]*\)"/\1/p' Cargo.toml)"
gh workflow run release.yml \
  --ref main \
  -f version="$version" \
  -f release_surface=desktop-and-scaffold \
  -f promotion_target=staging \
  -f scaffold_runtime_deployment=unchanged
```

Wait for the `release` workflow to succeed. Preserve its run URL and run ID: production must reuse that exact candidate through `candidate_run_id`. The workflow builds Linux with `scripts/package-linux.sh`, macOS production and staging apps with `scripts/package-macos.sh`, verifies both signed apps with `scripts/verify-macos-release.sh`, publishes the immutable candidate, and reads back the staging channels. `scripts/validate-release-candidate-reuse.mjs` enforces exact-candidate reuse later.

## 4. Captain manual-verification stop

Give the captain the staging run URL and version, then **stop**. The captain must update and restart his installed local Crew app and manually exercise the result. Installed macOS builds expose this at **Settings → Crew update**; source builds are report-only.

Do not publish production and do not change either Scaffold pin until the captain explicitly confirms the restarted local build behaves correctly. Silence, a successful workflow, or automated checks are not approval.

## 5. Publish production and update both Scaffold lanes

Only after the captain's explicit approval, promote the byte-identical staging candidate:

```bash
gh workflow run release.yml \
  --ref main \
  -f version="$version" \
  -f release_surface=desktop-and-scaffold \
  -f promotion_target=production \
  -f scaffold_runtime_deployment=unchanged \
  -f candidate_run_id="$staging_run_id"
```

Wait for production channel readback to succeed. Then use the released `scaffold-manifest.json` as one immutable tuple: version, private release bucket, and `comet-<version>-linux-x86_64.tar.gz` SHA-256.

In `/Users/czhen/ashler-platform`, update and verify both lanes without hand-editing generated image references:

1. **Staging Scaffold.** `.github/workflows/scaffold-staging-candidate.yml` pins the tuple through `workflow_dispatch.inputs.comet_version`, `comet_releases_gcs_bucket`, and `comet_linux_sha256`; lines 82–84 project them to `SCAFFOLD_ASHLER_COMET_VERSION`, `COMET_RELEASES_GCS_BUCKET`, and `SCAFFOLD_ASHLER_COMET_LINUX_SHA256`. Dispatch the documented staging flow from that repository:

   ```bash
   candidate_sha="$(git rev-parse 'origin/master^{commit}')"
   gh workflow run scaffold-staging-candidate.yml \
     --ref master \
     -f selected_ref="$candidate_sha" \
     -f comet_version="${COMET_VERSION:?Select the verified Crew release}" \
     -f comet_releases_gcs_bucket="${COMET_RELEASES_GCS_BUCKET:?Set its private GCS bucket}" \
     -f comet_linux_sha256="${COMET_LINUX_SHA256:?Set its Linux artifact digest}" \
     -f migration_mode=apply-forward
   ```

   Wait for the staging candidate build, template verification, and staging promotion to succeed. There is no checked-in staging semver to edit; the three dispatch inputs are the pin.

2. **Primary/production Scaffold.** `.github/workflows/scaffold-sandbox-images.yml` reads repository variables `SCAFFOLD_ASHLER_COMET_VERSION`, `COMET_RELEASES_GCS_BUCKET`, and `SCAFFOLD_ASHLER_COMET_LINUX_SHA256`. Its release step runs `internal/scaffold/scripts/stamp-platform-image-lock.mjs` and updates these generated fields in `deploy/environments/platform/image-locks/scaffold.yaml`: `sessionSourceRevision`, `sandboxProvider.e2bTemplate`, and `snapshotBuilder.sessionImageRef`. The workflow commits that lock to `master` and waits for Argo CD rollout; do not edit the lock by hand.

   **Open mechanism:** the platform checkout does not document the approved command for changing those GitHub repository variables or the exact manual `Scaffold Sandbox Images` dispatch flags for a release-tuple-only bump. This was checked in `.github/workflows/scaffold-sandbox-images.yml`, `internal/scaffold/README.md`, `internal/scaffold/docs/platform-operator-runbook.md`, `deploy/environments/**`, and the root `package.json`. Establish that operator command before this step; do not guess it. Completion requires the three variables to name the same verified release, the workflow-generated lock commit on `master`, and successful live rollout verification.
