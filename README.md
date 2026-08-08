# Ashler Comet

Ashler Comet is Ashler's internal, multi-device controller for Claude Code and Codex sessions. Each device runs a small engine; shared threads merge agent output while each owning device remains responsible for its own session commands.

## Install the Linux daemon

```bash
export COMET_RELEASES_GCS_BUCKET='the private production release bucket'
export COMET_RELEASES_URL="https://storage.googleapis.com/$COMET_RELEASES_GCS_BUCKET/releases"
export COMET_RELEASES_AUTHORIZATION="Bearer $(gcloud auth print-access-token)"
gcloud storage cat "gs://$COMET_RELEASES_GCS_BUCKET/releases/install.sh" | sh
unset COMET_RELEASES_AUTHORIZATION

comet login
systemctl --user start comet-native
```

The installer and artifacts live only in the private GCS release bucket. The installer removes `COMET_RELEASES_AUTHORIZATION` from the environment before curl starts and sends it as a header through stdin; it never persists the bearer. Release artifacts must never be public.

The installer requires the release `SHA256SUMS` entry to match before extracting an artifact. Day-to-day commands:

```bash
comet status
comet update
comet daemon start|stop|restart|status
```

Download the macOS DMG from the same release feed. Comet uses OMP over ACP. On macOS, bootstrap or validate the exact supported upstream binary explicitly (the installer never silently replaces an existing `omp`):

```bash
# Run the same private install.sh downloaded above:
sh install.sh --install-omp
```

This installs the official [oh-my-pi v17.2.9](https://github.com/can1357/oh-my-pi/releases/tag/v17.2.9) artifact to `~/.local/bin/omp` after SHA-256 verification. The pins are `omp-darwin-arm64` = `3f9c44c465da8428b5a81a0c9cdac22ced982319fe93d534914cb61838a63118` and `omp-darwin-x64` = `35c36f893a68feb6df3a61ff9359bb6ad13a5534687bb0396508aabc69c5f347`. Linux OMP distribution is managed separately and must not reuse these pins.

To use a remote OMP auth broker, launch Comet with `OMP_AUTH_BROKER_URL` and either `OMP_AUTH_BROKER_TOKEN` or `OMP_AUTH_BROKER_TOKEN_FILE`. The token-file form is preferred for service managers: it must be mode `0600`, is removed before parsing/spawn on every outcome, and Comet passes the bearer only in the OMP child environment, never argv or logs. Do not print or interpolate the token in shell commands. Scaffold-host OMP launches remain isolated with `--profile scaffold-host --no-extensions --no-skills --no-rules`.

## Local collaboration smoke

The deterministic smoke uses two in-memory headless devices and needs no cloud credentials, agent CLI, network, or persistent state:

```bash
node scripts/headless-collaboration-smoke.mjs
# or
npm --prefix edge run smoke:collaboration
```

It covers a scope-bound invite and join, two concurrent agent sessions, shared transcript provenance, owner-only teammate command execution and audit, reconnect replay, stable annotations, and attachment metadata without embedding blob bytes.

## GitHub deployment setup

The checked-in `edge/wrangler.jsonc` is the deployment contract. It defines isolated `staging` and `production` Worker, Durable Object, and R2 resources. It contains no Cloudflare account ID. Scaffold access uses verified Google Cloud IAP principals and environment-specific Scaffold project scope, independent of Ashler's customer-facing application stack.

The staging contract uses `SCAFFOLD_CONTROL_PLANE_URL=https://scaffold-staging.internal.ashler.com` with `SCAFFOLD_PROJECT_SCOPE=ashler-staging`. Production uses `SCAFFOLD_CONTROL_PLANE_URL=https://scaffold.internal.ashler.com` with `SCAFFOLD_PROJECT_SCOPE=ashler-production`. These values live in the two checked-in Wrangler environments, not GitHub secrets or command-line overrides.

Create these GitHub environments:

- `comet-staging`
- `comet-production`, with required reviewers and deployment-branch protection for `main`
- `comet-release-staging`
- `comet-release-production`, with required reviewers and deployment-branch protection for release tags

Add the following environment-scoped secrets to both `comet-staging` and `comet-production`, using different values in each environment:

- `CLOUDFLARE_API_TOKEN`: token limited to that environment's Worker, Durable Objects, and R2 resources
- `CLOUDFLARE_ACCOUNT_ID`: Cloudflare account selected at runtime, never checked in
- `GCP_WORKLOAD_IDENTITY_PROVIDER`: GitHub Actions Workload Identity Provider for that environment
- `GCP_SERVICE_ACCOUNT`: environment-specific deploy service account email allowed to mint an IAP identity token

Add these environment-scoped variables to both deployment environments, again with environment-specific values:

- `GCP_PROJECT_ID`
- `GCP_IAP_AUDIENCE`

The staging job only references values from `comet-staging`. The production job only starts after staging, verifies the downloaded candidate digest equals both the build digest and staging digest, then enters `comet-production` for approval. Production secrets are never present in the staging job.

Add these environment-scoped secrets to `comet-release-staging` and `comet-release-production`:

- `GCP_WORKLOAD_IDENTITY_PROVIDER`
- `GCP_SERVICE_ACCOUNT`
- `COMET_RELEASES_GCS_BUCKET`

Add `GCP_PROJECT_ID` as an environment-scoped variable to both. Use separate private buckets and identities. The publisher's bucket IAM must grant exactly the operations it preflights: `storage.buckets.get`, `storage.buckets.getIamPolicy`, `storage.objects.get`, `storage.objects.create`, and `storage.objects.delete`; scope object access to that environment's `releases/` prefix. Deployment and publication are disabled unless the repository owner is `Ashler-AI`. Production promotes the candidate accepted by staging and never creates a public GitHub Release or object.

The deployed Comet edge also needs `COMET_RELEASES_GCS_BUCKET`,
`GCP_RELEASE_SERVICE_ACCOUNT_EMAIL`, and `GCP_RELEASE_SERVICE_ACCOUNT_PRIVATE_KEY`
as environment-specific Wrangler secrets. The service account is read-only on
that environment's release bucket. Installed CLI and desktop clients send their
renewable Comet login to the edge; GCS credentials never reach the device.

## Deploy

Pushes to `main` that change `edge/` deploy staging only. Run either path from GitHub's **Deploy** workflow, or with GitHub CLI:

```bash
gh workflow run deploy.yml -f target=staging
gh workflow run deploy.yml -f target=production
```

The production command still deploys staging first. The `comet-production` environment approval is the production gate.

For an authenticated local deployment, use the checked-in environment contract and environment-specific Cloudflare credentials:

```bash
cd edge
npm ci
CLOUDFLARE_API_TOKEN=... CLOUDFLARE_ACCOUNT_ID=... npx wrangler deploy --env staging
CLOUDFLARE_API_TOKEN=... CLOUDFLARE_ACCOUNT_ID=... npx wrangler deploy --env production
```

Never reuse production credentials for the staging command.

## Release

A `v<workspace-version>` tag builds and checks these artifacts:

- `comet-<version>-linux-x86_64.tar.gz`
- `comet-<version>-linux-aarch64.tar.gz`
- `comet-<version>-macos-arm64.dmg`
- `comet-<version>-macos-arm64-app.tar.gz`
- `install.sh`

CI verifies the tag or dispatch version against both `[workspace.package].version` and Cargo metadata. It emits `SHA256SUMS`, `manifest.json`, source commit and workflow provenance, and a digest-sealed candidate. Staging and production consume that same archive. Every `releases/<version>/…` object and version-named root artifact is create-only; a byte-identical re-publish is a no-op and differing bytes fail. Only `latest.txt`, `manifest.json`, `SHA256SUMS`, and `install.sh` are moving latest-channel aliases. All objects remain private.

```bash
version="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\([^"]*\)"/\1/p' Cargo.toml)"
git tag "v$version"
git push origin "v$version"

# Build without publishing
gh workflow run release.yml -f version="$version" -f publish=false
```

Manual publication still requires both private release environments and the production approval. It does not create a GitHub Release or public object. Cargo workspace packages keep `publish = false`, so the workflow cannot publish crates.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the runtime design.
The required local/Scaffold cutover and multi-client acceptance contract is in [docs/ASHLER-SCAFFOLD-END-STATE.md](docs/ASHLER-SCAFFOLD-END-STATE.md).

## Provenance and licensing

Ashler Comet is derived from `zeronsh/comet` at commit `82ce44193a32b5ae5610f8a4542e5e30b992e6a9`. The inherited [MIT LICENSE](LICENSE) is preserved verbatim.

The native UI depends only on the Apache-2.0 GPUI packages and uses permitted GPUI examples as API references. GPL-licensed Zed application crates, UI code, and editor code are not copied, linked, or redistributed.

Licensed under the [MIT License](LICENSE).
