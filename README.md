# Crew

Crew is Ashler's internal, multi-device controller for coding-agent sessions. The repository, binary, protocols, and service identifiers retain the `Comet` name for compatibility.

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

Download the macOS DMG from the same release feed. Comet uses OMP over ACP. The installer bootstraps any missing agent CLI (OMP, Claude Code, Codex) after the comet install — failures there never abort the install, and `COMET_SKIP_AGENT_BOOTSTRAP=1` skips the phase for managed environments. An existing `omp` is never silently replaced; to bootstrap or validate it explicitly:

```bash
# Run the same private install.sh downloaded above:
sh install.sh --install-omp
```

This installs the official [oh-my-pi v17.2.9](https://github.com/can1357/oh-my-pi/releases/tag/v17.2.9) artifact to `~/.local/bin/omp` after SHA-256 verification against the per-platform pins in `install.sh` (darwin arm64/x64, linux glibc and musl arm64/x64). Updates after that are in-app: the engine tracks agent CLI versions on its release-check cadence, Settings → Agents offers per-agent updates through each CLI's own self-updater (`omp update`, `claude update`, `codex update`), and by default the first boot of a new Comet version refreshes installed agents automatically (Settings toggle or `COMET_UPDATE_HARNESSES=0` to opt out).

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
- `comet-release-staging`
- `comet-release-production`, with required reviewers and deployment-branch protection for release tags

Add these environment-scoped deployment secrets to `comet-staging`:

- `CLOUDFLARE_API_TOKEN`: token limited to the Crew staging and production Worker, Durable Object, and R2 resources
- `CLOUDFLARE_ACCOUNT_ID`: Cloudflare account selected at runtime, never checked in
- `GCP_WORKLOAD_IDENTITY_PROVIDER`: GitHub Actions Workload Identity Provider
- `GCP_SERVICE_ACCOUNT`: deploy service account email allowed to mint an IAP identity token

Add `GCP_PROJECT_ID` and `GCP_IAP_AUDIENCE` as variables. The staging job
uses this environment directly. The current production deploy reuses the same
platform-scoped credentials only after the staged candidate digest and GCP
project/provider assertions pass. Release-feed synchronization does not reuse
that boundary: it enters the matching protected `comet-release-*` environment
before either edge deployment.

Add these environment-scoped secrets to `comet-release-staging` and `comet-release-production`:

- `GCP_WORKLOAD_IDENTITY_PROVIDER`
- `GCP_SERVICE_ACCOUNT`
- `COMET_RELEASES_GCS_BUCKET`

Add `GCP_PROJECT_ID` as an environment-scoped variable to both. Use separate private buckets and identities. The publisher's bucket IAM must grant exactly the operations it preflights: `storage.buckets.get`, `storage.buckets.getIamPolicy`, `storage.objects.get`, `storage.objects.create`, and `storage.objects.delete`; scope object access to that environment's `releases/` prefix. Deployment and publication are disabled unless the repository owner is `Ashler-AI`. Production promotes the candidate accepted by staging and never creates a public GitHub Release or object.

The deployed Crew edge release feed is synchronized by the **Deploy** workflow
from the matching `comet-release-staging` or `comet-release-production`
environment. Add `CLOUDFLARE_API_TOKEN`, `CLOUDFLARE_ACCOUNT_ID`,
`GCP_RELEASE_SERVICE_ACCOUNT_EMAIL`, and
`GCP_RELEASE_SERVICE_ACCOUNT_PRIVATE_KEY` there alongside
`COMET_RELEASES_GCS_BUCKET`. The reader service account is read-only on that
environment's release bucket. When both reader credentials are absent, the
workflow preserves the existing Worker secrets; a partial or malformed reader
configuration fails before Wrangler runs. A complete configuration is applied
with one bulk secret update. Do not provision release-feed credentials with a
local `wrangler secret put`. Installed CLI and desktop clients send their
renewable Crew login to the edge; GCS credentials never reach the device.

## Deploy

Pushes to `main` that change `edge/` deploy staging only. Run either path from GitHub's **Deploy** workflow, or with GitHub CLI:

```bash
gh workflow run deploy.yml -f target=staging
gh workflow run deploy.yml -f target=production
```

The production command still deploys staging first. Every production edge
deploy then requires one approval through the protected
`comet-release-production` synchronization job. That approval authorizes the
production deployment, so the gate intentionally remains when the reader pair
is absent and secret synchronization is a no-op. Production starts only after
the gate succeeds.
For an authenticated local deployment, use the checked-in environment contract and environment-specific Cloudflare credentials:

```bash
cd edge
npm ci
CLOUDFLARE_API_TOKEN=... CLOUDFLARE_ACCOUNT_ID=... npx wrangler deploy --env staging
CLOUDFLARE_API_TOKEN=... CLOUDFLARE_ACCOUNT_ID=... npx wrangler deploy --env production
```

Never reuse production credentials for the staging command.

## Release

Manual releases choose an explicit surface:

- `desktop` builds and promotes only `comet-<version>-macos-arm64.dmg` and the macOS app tarball. It advances `desktop-manifest.json` and `desktop-latest.txt` only.
- `desktop-and-scaffold` also builds both Linux archives, emits a `scaffold.comet-runtime.v1` compatibility manifest, and advances `scaffold-manifest.json` plus `scaffold-latest.txt`. Use this whenever headless engine, auth, relay, OMP, or Scaffold-host behavior changed.

Version tags remain complete `desktop-and-scaffold` releases for backward compatibility. CI verifies the tag or dispatch version against both `[workspace.package].version` and Cargo metadata. Every `releases/<version>/…` object and version-named root artifact is create-only; a byte-identical re-publish is a no-op and differing bytes fail. Moving desktop and Scaffold channel aliases are independent. All objects remain private.

`scaffold-runtime-version.txt` is the compatibility boundary between Comet and
the Scaffold control plane. A breaking runtime change is platform-first:

1. Increment that file and update both repositories to accept the new contract.
2. Deploy and verify the compatible ashler-platform change in Scaffold staging.
3. Publish Comet with `release_surface=desktop-and-scaffold` and
   `scaffold_runtime_deployment=staging-deployed`.
4. Deploy and verify the compatible Scaffold change in production before a
   production Comet publication with
   `scaffold_runtime_deployment=production-deployed`.

The release workflow compares the candidate runtime version with the currently
published Scaffold manifest before writing any release objects. A changed
contract cannot ship as a desktop-only release. An unacknowledged bump,
including one introduced by a tag push, fails with the required deployment
sequence.

```bash
version="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\([^"]*\)"/\1/p' Cargo.toml)"

# Build or promote a desktop-only release.
gh workflow run release.yml \
  -f version="$version" \
  -f release_surface=desktop \
  -f promotion_target=staging

# Build or promote a release that Scaffold may pin.
gh workflow run release.yml \
  -f version="$version" \
  -f release_surface=desktop-and-scaffold \
  -f promotion_target=staging

# A version tag builds and promotes the complete release through production.
git tag "v$version"
git push origin "v$version"
```

Manual production publication still requires both private release environments and the production approval. It does not create a GitHub Release or public object. Cargo workspace packages keep `publish = false`, so the workflow cannot publish crates.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the runtime design.
The required local/Scaffold cutover and multi-client acceptance contract is in [docs/ASHLER-SCAFFOLD-END-STATE.md](docs/ASHLER-SCAFFOLD-END-STATE.md).

## Provenance and licensing

Crew is derived from `zeronsh/comet` at commit `82ce44193a32b5ae5610f8a4542e5e30b992e6a9`. The inherited [MIT LICENSE](LICENSE) is preserved verbatim.

The native UI depends only on the Apache-2.0 GPUI packages and uses permitted GPUI examples as API references. GPL-licensed Zed application crates, UI code, and editor code are not copied, linked, or redistributed.

Licensed under the [MIT License](LICENSE).
