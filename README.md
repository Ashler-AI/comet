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

### Desktop gateway extension discovery

Crew 0.1.83 installs a credential-free, Crew-owned discovery adapter at
`extensions/crew-auth-gateway.ts` in the selected OMP agent directory. Ordinary
cold-revived subagents discover the same gateway provider as their parent instead
of removing its authentication registration. The adapter is inert unless both
`COMET_SESSION_ID` and `COMET_INFERENCE_TOKEN` are present; bare OMP sessions do not
gain Crew providers merely because the file exists.

An existing user-owned `extensions/omp-auth-gateway.ts` keeps precedence, including
when added after Crew's adapter was installed. Crew does not overwrite unrelated
extensions. Symlinked extension directories and conflicting managed-path files
are rejected rather than overwritten or silently falling back to the broken
explicit-only revival path. Scaffold's no-discovery policy and Prime's explicit
adapter remain separate. This desktop-only release does not replace the installed
OMP executable or advance the Scaffold release channel.

The isolated installed-runtime regression can be run without provider credentials:

```bash
node scripts/omp-gateway-revival-smoke.mjs ~/.local/bin/omp --compare-explicit
```

## Native OMP handoff to Scaffold

For an explicitly requested remote task, local OMP runs inside Crew use the
native handoff action instead of creating a standalone OpenCode task:

```bash
"$COMET_EXECUTABLE" session handoff "$COMET_SESSION_ID" --prompt-file "$PROMPT_FILE" --database-environment local
```

Crew supplies `COMET_LOCAL_AGENT_RUNTIME=1`, `COMET_EXECUTABLE`, the source Crew
chat ID in `COMET_SESSION_ID`, and `COMET_IPC_PORT`. Preserve those values. The
prompt file must contain nonblank UTF-8 text of at most 1 MiB; create it privately
with mode `0600` and remove it afterward. Database snapshots require an explicit
request; the configured Scaffold deployment remains unchanged.

The CLI and composer share native preparation: create or recover the target,
attach its host, transfer OMP history and the worktree, then queue the task in a
distinct Crew chat. The receipt contains `chatId`, `sandboxId`, `commandId`, and
`environment`; it confirms command admission, not remote task completion.
Monitor the returned chat in Crew, not standalone `handoff.*` lifecycle tools.

Native transfer captures the source repository's HEAD and reachable Git history,
plus its dirty and untracked files, into a bounded, verified archive. Scaffold
reconstructs it at `/workspace/crew-handoff`, preserving a nested source cwd;
the provisioned `/workspace/ashler-platform` checkout is left untouched. The two
repositories do not need a shared commit. OMP context is rebased to the imported
cwd. Capture/import fail closed on archive, expanded-object, checkout-size, path,
or symlink safety violations; Git submodules are not reconstructed.

The September 9 development-build smoke completed through the native CLI and
remote Crew chat `030e3ef6-57c4-4749-bd17-cf3ec7435eda` in sandbox
`rcs_cc61f7c6b3c3a63e04a6d5d5` (staging, local database). The remote agent recovered
the prior conversation marker, source HEAD, committed file, and uncommitted file,
and confirmed the platform repository remained separate. This verifies the
correction in the development controller, not an installed-app or release-channel
rollout. The original grant rejection's HTTP response was unavailable; subsequent
grant rejections now preserve bounded HTTP status and machine-code diagnostics
without exposing bearer tokens or response details.

The correction shipped to the desktop staging channel as **0.1.85** from
`e771970915b4b10565a825dca6140e13703b682f` in
[release run 34387642609](https://github.com/Ashler-AI/comet/actions/runs/34387642609).
CI passed **537 UI tests**, the existing fork/gateway checks, and the native
preparation, worktree, materializer, grant-diagnostic, and CLI checks before
packaging. The downloaded release archive and desktop artifacts passed SHA-256
verification; the packaged app passed strict code-signature verification and
reported **0.1.85**. Live staging manifest readback matched the candidate's exact
source and hashes. The installed **0.1.84** app's read-only update check reported
**0.1.85 available**; it was not replaced during publication. Desktop production
remained **0.1.83**, both Scaffold channels remained **0.1.81**, and mobile was
unchanged. OpenCode reviewed the final release changes with no actionable findings.

Missing runtime values or an unsupported CLI/RPC require updating Crew's binary
and running engine together, then starting a fresh local agent run. Never guess
an executable, retry creation blindly, or fall back to OpenCode. After an error,
inspect Crew for an accepted sandbox before retrying. Standalone handoff recovery
remains separate and unchanged.

## Native worker integration

The [native worker adapter contract](docs/reference/native-workers.md) describes
owned isolated Crew sessions, durable messaging/outcomes, retention-only lifecycle
controls, and the separate Firstmate cutover requirements. This branch's new
endpoints are uncompiled/unverified. The source is reconciled onto current
upstream handoff; complete authorized remote verification before installation.

## Local collaboration smoke

The deterministic smoke uses two in-memory headless devices and needs no cloud credentials, agent CLI, network, or persistent state:

```bash
node scripts/headless-collaboration-smoke.mjs
# or
npm --prefix edge run smoke:collaboration
```

It covers a scope-bound invite and join, two concurrent agent sessions, shared transcript provenance, owner-only teammate command execution and audit, reconnect replay, stable annotations, and attachment metadata without embedding blob bytes.

## Session attention notifications

Crew 0.1.72 adds opt-in native desktop alerts for fresh session input requests,
errors, and working-to-idle completion. Enable them in **Settings → Notifications**;
use **Send test notification** to check OS delivery. Existing chime preferences
remain independent. Initial/reconnected snapshots, stale or reordered updates,
archived sessions, and the actively viewed session do not produce banners.
Notification actions open the corresponding session, including reopening a
closed main window. On macOS, launch the installed Crew app bundle rather than
the bare executable; OS permission and Focus settings still control delivery.

Mobile attention alerts use the same factual transition policy. Production iOS
1.0 (5) was uploaded with the missing-session visibility fixes and verified
production APNs entitlement. Its preceding simulator crash was confined to the
Debug regression harness, not Release. App Store Connect reports processing
complete and assignment to Ashler Internal; the tester remains Invited. Background
delivery remains blocked on production Worker credentials; see
[iOS release evidence](apps/ios/README.md#crew-0172--production-ios-10-5) and
[notification setup](apps/ios/README.md#session-attention-notifications).

## GitHub deployment setup

The checked-in `edge/wrangler.jsonc` is the deployment contract. It defines isolated `staging` and `production` Worker, Durable Object, and R2 resources. It contains no Cloudflare account ID. Scaffold access uses verified Google Cloud IAP principals and environment-specific Scaffold project scope, independent of Ashler's customer-facing application stack.

The staging contract uses `SCAFFOLD_CONTROL_PLANE_URL=https://scaffold-staging.internal.ashler.com` with `SCAFFOLD_PROJECT_SCOPE=ashler-staging`. Production uses `SCAFFOLD_CONTROL_PLANE_URL=https://scaffold.internal.ashler.com` with `SCAFFOLD_PROJECT_SCOPE=ashler-production`. These values live in the two checked-in Wrangler environments, not GitHub secrets or command-line overrides.

Create these GitHub environments:

- `comet-staging`
- `comet-release-staging`
- `comet-release-production`, with required reviewers and deployment-branch protection for release tags

Required reviewers and deployment-branch protection are recommended setup, not
properties supplied by this workflow. As inspected on 2026-09-07,
`comet-release-production` has `protection_rules: []` and
`deployment_branch_policy: null`. It currently provides **no required-reviewer
approval or branch restriction**. An authorized production dispatch can proceed
without a review after its prerequisite jobs succeed.

Add these environment-scoped deployment secrets to `comet-staging`:

- `CLOUDFLARE_API_TOKEN`: token limited to the Crew staging and production Worker, Durable Object, and R2 resources
- `CLOUDFLARE_ACCOUNT_ID`: Cloudflare account selected at runtime, never checked in
- `GCP_WORKLOAD_IDENTITY_PROVIDER`: GitHub Actions Workload Identity Provider
- `GCP_SERVICE_ACCOUNT`: deploy service account email allowed to mint an IAP identity token

Add `GCP_PROJECT_ID` and `GCP_IAP_AUDIENCE` as variables. The staging job
uses this environment directly. The current production deploy reuses the same
platform-scoped credentials only after the staged candidate digest and GCP
project/provider assertions pass. Release-feed synchronization does not reuse
that boundary: it enters the matching `comet-release-*` environment
before either edge deployment.

The candidate job requires its TypeScript check on both push and manual triggers;
there is no supported input to skip it. Builds, tests, staging verification and
candidate-digest checks are also required.
The notification release was integrated directly into main and deployed by
[34141398021](https://github.com/Ashler-AI/comet/actions/runs/34141398021), with
the existing desktop candidate promoted from main by
[34141507706](https://github.com/Ashler-AI/comet/actions/runs/34141507706).

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

The production command deploys staging first, then waits for the
`comet-release-production` synchronization job and verifies the same candidate
digest. A manual review is required **only when** the GitHub environment has
required reviewers configured. With the current empty protection rules, this is
job ordering and credential scoping, not an approval gate. Synchronization remains
a prerequisite even when the reader credential pair is absent and it is a no-op.
Do not deploy production locally; use `workflow_dispatch target=production`.
For an authenticated local **staging** deployment:

```bash
cd edge
npm ci
CLOUDFLARE_API_TOKEN=... CLOUDFLARE_ACCOUNT_ID=... npx wrangler deploy --env staging
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

Crew preserves the accepted Scaffold sandbox and room while attachment is pending.
An exact `503 sandbox_runtime_starting` response (including the provider's nested
`body.error` envelope) keeps one native Attach operation waiting, bounded to two
minutes and cancellable. Each wait rechecks the sandbox, owner, room, and lifecycle
epoch; unrelated 404s and terminal states still fail. Manual retry uses the same
accepted sandbox. A failed launch without an accepted remote target discards its
pending draft; confirmed deletion discards an accepted pending draft. Deleting a
persisted chat keeps the ordinary chat-deletion behavior.
Deleting a chat during its first send also removes the pending sidebar entry, so
later workspace updates cannot restore a deleted session as still starting.

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

Manual production publication uses both private release environments. Reviewer approval depends on their actual GitHub protection rules; environment names alone do not require it. It does not create a GitHub Release or public object. Cargo workspace packages keep `publish = false`, so the workflow cannot publish crates.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the runtime design.
The required local/Scaffold cutover and multi-client acceptance contract is in [docs/ASHLER-SCAFFOLD-END-STATE.md](docs/ASHLER-SCAFFOLD-END-STATE.md).

## Provenance and licensing

Crew is derived from `zeronsh/comet` at commit `82ce44193a32b5ae5610f8a4542e5e30b992e6a9`. The inherited [MIT LICENSE](LICENSE) is preserved verbatim.

The native UI depends only on the Apache-2.0 GPUI packages and uses permitted GPUI examples as API references. GPL-licensed Zed application crates, UI code, and editor code are not copied, linked, or redistributed.

Licensed under the [MIT License](LICENSE).
