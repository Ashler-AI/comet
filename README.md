# Crew

Crew is Ashler's internal, multi-device controller for coding-agent sessions. The repository, binary, protocols, and service identifiers retain the `Comet` name for compatibility.

## Unreleased: shared session discovery

`comet session search --query "upload retries"` searches generated and display
titles. `--source-url` accepts a Slack message/thread or Notion page link;
`--owner-id`, `--limit` (1–50), and `--cursor` narrow or page the same search.
Results contain title, owner, matched sources, and a credential-free Crew link,
never transcript contents or an assertion that the owner is currently working.
Opening a result preserves its project and deployment scope and requires sign-in.

Completed turns queue fixed-model `gpt-5.6-luna` title generation through Agent
Auth, independently of the conversation harness. Manual and legacy titles survive
refresh; generated titles update sidebar metadata, not existing branch names.
The first prompt still names a new managed branch once. The durable local outbox
coalesces snapshot metadata to a 30-second maximum scheduling delay; completed
turns get priority, but model calls are limited to one per session per minute.
One background worker bounds concurrent inference; retry deadlines survive restart.
Slack/Notion link extraction does not depend on successful title generation.
Title synchronization tracks this chat's metadata editors, not every device that
has touched the workspace; unrelated workspace peers cannot exhaust the title
update's causal-vector budget. Previously acknowledged edits remain fenced.

Available persisted owned sessions are backfilled in bounded pages without
opening their rooms. Archived and offline sessions do not expire; explicit
deletion durably queues a permanent tombstone before local source removal. Oversized
sources remain pending with a diagnostic rather than acknowledging partial links.
The server indexes project, deployment, and session together, so identical UUIDs
in distinct rooms do not overwrite or delete each other.

This requires coordinated Crew edge and Scaffold provider/control-plane rollout,
the dedicated `crew`/`crew_staging` database, and the search-only Forge credential.
Source changes alone do not provision or deploy those resources. A failed or empty
lookup is not evidence that nobody is working; search does not suppress triage.

## Unreleased: compatible upstream integration

Selected upstream improvements are adapted to Crew, not a wholesale upstream merge:

- Transcript watchers share immutable bounded snapshots, retaining Crew's paging
  cursors. Streaming Markdown shares completed blocks; cosmetic animation leases
  expire and settled scroll springs stop requesting idle frames.
- macOS uses mimalloc v2; other platforms keep the system allocator. Release
  downloads reject HTTPS downgrade redirects; Rustls is updated to 0.23.45.
- Local controllers offer Devin (`devin acp`), Grok (`grok --no-auto-update agent
  --no-leader stdio`), Hermes (`hermes acp`), and Pi (installed `pi-acp` adapter).
  Install and authenticate each agent separately; opening Crew never installs
  adapters. Executable overrides are `DEVIN_EXECUTABLE`, `GROK_EXECUTABLE`,
  `HERMES_EXECUTABLE`, and `PI_ACP_EXECUTABLE`. Pi also requires its underlying CLI.
  The selected host supplies live models, reasoning options, and slash commands.
  New agents queue followups at turn boundaries and resume through ACP when the
  agent advertises support. Permission requests always require an interactive
  response, even for auto-approve runs; missing approval cancels the request.

These ACP agents use their own CLI credentials, not Crew's shared Agent Auth
accounts. Unsupported shared-account routing, native forks, and read-only sandbox
requests fail closed. Scaffold hosts remain OMP-only; existing native OMP/Scaffold
handoff and mid-turn steering are unchanged. Existing clients must be upgraded
before sharing sessions containing the new harness identities. This change does
not migrate workspace/session sync, rename storage or services, change release
channels, or deploy a new build. Protocol-fixture and isolated native-UI checks do
not establish live-provider compatibility or a measured CPU/memory improvement.

## Scaffold session web view

`apps/web` provides the browser surface for one existing Crew/OMP session in a
Scaffold sandbox: empty and active conversation states, streamed messages, tool
details, model/reasoning selection, attachments, input answers, and send/steer/stop.
It deliberately has no session creation, session list, settings, or checkout controls.
User messages are right-aligned; Crew responses remain left-aligned. **Open in Crew**
continues the assigned session in the installed desktop or mobile app using a
credential-free, deployment-specific link. The app authenticates the attachment
with its own identity; execution stays in Scaffold. Mobile currently requires an
online desktop Crew controller. This is not a transfer of execution or files onto
the local device, and requires app builds containing the new Scaffold link handler.

The dependency-free Node service connects to the assigned engine's loopback IPC.
Scaffold's authenticated attach proxy supplies a dedicated server-side credential;
the browser never receives that credential or unrestricted RPC access. The service
requires exact trusted session/sandbox bindings and existing Crew capability grants.
Model changes go through Scaffold's owner-authorized
`/sessions/<sandbox>/opencode/api/model-route` before the
next message; active turns must be stopped before switching model or reasoning.
Protected reads and stream updates require a current live read grant; losing it
clears the cached transcript. Queued commands retain submitted content until their
durable outcome is applied; rejection or expiry remains visible and retryable.

Linux archives include `crew-web/` beside `comet`, so the viewport and its
[scoped engine RPCs](docs/ASHLER-SCAFFOLD-END-STATE.md#durable-mirroring-invariants)
ship together. Runtime: Node >=22.4,
`node /opt/crew-web/server.mjs`; Scaffold configures `COMET_IPC_PORT`, `COMET_DATA_DIR`,
`SCAFFOLD_RUNTIME_DIR`, and `CREW_WEB_AUTH_TOKEN`. Trusted `sessionId` and `sandboxId`
bindings come from `SCAFFOLD_COMET_RUNTIME_PROFILE_JSON`; explicit
`CREW_WEB_SESSION_ID` (which must match any profile session) and
`CREW_WEB_SANDBOX_ID` (used when the profile omits the sandbox) are also supported.
`CREW_WEB_HOST` and `CREW_WEB_PORT` default to `127.0.0.1` and `4096`.
The attach proxy must follow the authentication and mutation-origin contract in
`authorize` in `apps/web/server.mjs`.
Tests: `node --test apps/web/server.test.mjs`.

Rollout requires publishing a new immutable Crew Linux release containing these
assets, selecting its verified release tuple in the platform repository, rebuilding
the sandbox image, and deploying the coordinated provider/control-plane changes.
Existing images are not upgraded by editing this repository. No rollout is implied
by the presence of this code.

## Crew 0.1.106 manual updates

Installed macOS builds now expose **Settings → Crew update** for an ad hoc
release check. The page reuses the signed download, verification, replacement,
and relaunch path from the update notice, and reports a current installation as
success instead of an error. Source builds remain report-only because they
cannot safely replace their own installation.

## Crew 0.1.103 OMP model discovery

Desktop OMP catalogs now include Scaffold's shared model roster even when local
OMP has no provider credentials. Local entries retain their labels, reasoning
options, and precedence; custom providers remain selectable. Mobile consumes
the same host `ListModels` response. Scaffold hosts keep their authority-scoped
catalog, and every run still passes the existing Agent Auth checks.

The bundled `crates/harness/src/omp/scaffold-models.json` is generated from the
canonical Platform `ompInferenceModelCatalog`, with its source commit recorded
in the file. It is a release snapshot, not a live availability promise. Refresh
it with `node scripts/sync-omp-model-catalog.mjs <platform commit SHA>`; do not
hand-edit the model list. Unknown source formats fail regeneration.

Catalog regressions cover credential-free defaults, local overrides/custom
providers, deduplication, and preservation of the Scaffold-scoped catalog.

## Crew 0.1.104 journal recovery

Journal lookup no longer stops after 10,000 directories. Existing sessions retain
their native identities and histories. Resume waits up to five seconds for a
writer to release its journal, without relying on ancestry that may disappear
during teardown; an active or unverifiable writer still prevents resume. Waiting
does not terminate any process or relax explicit takeover ownership checks.

[Staging publication](https://github.com/Ashler-AI/comet/actions/runs/34909996223)
passed for `6af6c6ec6b32010c58724b8b949b5f5560b899f6`, including macOS recovery
and restart gates, Linux artifacts, and signed candidate verification. Local harness
and integration checks passed 128 tests. All seven incident sessions returned a
health reply through exact-path resume with their original native IDs. The signed
0.1.104 app was installed and its updater confirmed the staging version; the running
client was not restarted because recovered sessions were executing new work.
Production publication was not requested. Local typechecks were intentionally skipped.

## Crew 0.1.102 restart recovery

Crew preserves the exact interrupted request and native OMP session across a
planned restart, including attachments, model options, and the original user
message ID. Shutdown closes admission before draining owned runtimes. Completed
requests carry a durable retirement marker so another restart cannot replay them.

OMP journal paths and session IDs resolve to the same canonical native identity.
Resume waits briefly for verified Crew-owned teardown; unrelated writers remain
blocked. Explicit takeover survives another restart, is safe to retry, and resumes
without stopping anything when the original writer has already exited. Native
identity and process ownership checks remain fail-closed.

The desktop shows current waiting, stopping, resuming, failed, and completed
recovery states. Recovery actions no longer depend on historical error text.
Detached tools are cleaned up only when their ancestry and process birth identity
were observed; an unprovable orphan is not automatically killed.

This release changes the desktop and Linux runtime without changing the Scaffold
runtime compatibility contract. Publication does not upgrade existing sandboxes.

[Linux and macOS verification](https://github.com/Ashler-AI/comet/actions/runs/34902878407)
passed for `dc6725992beb312f134607c0bc0bfd5b8a6c10b0`, including real supervisor
process cleanup, two consecutive active-turn restarts, retired-request replay
prevention, 547 desktop tests, and isolated native CLI restart smoke checks.
Local release-contract checks passed 31 tests; local typechecks were intentionally
not run.

[Staging publication](https://github.com/Ashler-AI/comet/actions/runs/34903937027)
published 0.1.102 from merged `e57761d`. Downloaded desktop checksums, strict
signature validation, and Gatekeeper acceptance passed. An isolated signed-client
smoke displayed the actionable recovery failure banner; the live staging updater
reported 0.1.102 available. Production publication was not requested.

## Crew 0.1.99 release

Source `4b4b7bcd12ba90780f89c6c8e9488cc86730838b` is merged into `main`.
[Staging release](https://github.com/Ashler-AI/comet/actions/runs/34772456440)
built and verified the desktop and Linux artifacts; [production promotion](https://github.com/Ashler-AI/comet/actions/runs/34773638162)
reused the exact candidate and passed both channel readbacks. Standalone Crew
Staging.app is included in the build's separate staging artifact.

Scaffold [PR #6330](https://github.com/Ashler-AI/ashler-platform/pull/6330) merged
the Anthropic credential projection fix and native-open list action. Its
[staging rollout](https://github.com/Ashler-AI/ashler-platform/actions/runs/34773638333)
and [primary rollout](https://github.com/Ashler-AI/ashler-platform/actions/runs/34774708243)
passed sandbox-provider verification and promotion. Effective and fallback pins
select 0.1.99; Linux x86_64 SHA-256 is
`620fcb8b3858410114a5342a88977f526956a431e8edacb3a4a0649cf4eb748e`.
Existing running sandboxes are not claimed to have been restarted or upgraded.

Mobile [staging 1.0 (21)](https://github.com/Ashler-AI/comet/actions/runs/34772456221)
and [production 1.0 (15)](https://github.com/Ashler-AI/comet/actions/runs/34772456098)
passed simulator and archive verification; downloaded checksums and source
provenance match. Both were subsequently distribution-exported and uploaded on
2026-09-13 for internal TestFlight only, without local compilation. Apple accepted
staging upload `eabb0795-1470-4c17-9705-ea2f00bfc2bc` and production upload
`af77bb1d-2dfd-413c-87fa-ef983f0aeaac`; both entered processing. Inspection and
exact uploaded IPAs passed strict deep signature verification. Uploaded SHA-256:

- Staging: `e976185e6544632e0c04e0363a5b45f0de50515d85f351efe73dc667215b0fe0`
- Production: `acafadc2c4f95fc67418123dbf8cb64d466fc3b803c0b1cfc4688fdf868e2a7c`

Authenticated App Store Connect readback confirmed both uploads **Complete** and
both builds **Testing**, internal-only, in their existing **Ashler Internal**
groups (staging: one invite; production: two). No tester groups were changed.
Device installation, notification receipt, native URL launch on installed devices,
and live Anthropic completion remain unverified; local typechecks were not run.

## Scaffold sidebar activity

Local Crew controllers observe the exact Scaffold session rooms referenced by their
workspace, including sessions that are not selected. Remote owner status and
heartbeats feed the normal sidebar indicators; explicit completion updates
`lastMessageAt` with the source completion time, moving the session to the top of
the recency-sorted list. Reconnecting does not manufacture new activity or unread
state, and removing a session reference stops its activity observer.

Scaffold hosts remain excluded from workspace-room access. Status publication
changes require an updated sandbox runtime; local projection changes require an
updated Crew controller. These source changes do not upgrade running installations.

## Crew 0.1.90 release

Release source `356ccff31934f5eca6310d7a128345a13daa7456` is merged into `main`
and pinned on `release/crew-0.1.90`. Native handoff now transfers bounded Git
deltas or exact-HEAD shallow snapshots, with isolated partial-clone hydration
and safe process-group cleanup instead of bundling all reachable history.

[Staging publication](https://github.com/Ashler-AI/comet/actions/runs/34517634439)
built the macOS distribution and both Linux Scaffold archives.
[Production promotion](https://github.com/Ashler-AI/comet/actions/runs/34521123713)
reused the byte-identical candidate and passed authenticated production channel
readback. Artifact hashes, macOS signatures, notarization, Gatekeeper acceptance,
and DMG/updater bundle parity were verified. The Scaffold runtime contract remains
`scaffold.comet-runtime.v1`; no Edge or Scaffold control-plane deployment occurred.

Mobile staging **1.0 (18)** and production **1.0 (12)** are processed and available
in their existing **Ashler Internal** TestFlight groups; see the
[mobile release evidence](apps/ios/README.md#crew-0190-upload-evidence).
OpenCode pre-push review reported no actionable findings. The focused local gates
passed 31 Rust tests and 24 release-contract tests; local typechecks were skipped.
No installed app or engine was replaced or restarted. A live retry of the original
failed handoff, physical-phone installation, and tester notification receipt remain
unverified.

## Crew 0.1.88 release

Source `3d4c161f27bc9a61ff1118710a18c2c9dbc085f9` is merged into `main`.
It combines inter-session message reveal/collapse, readiness-driven Scaffold
startup with draft retention through admission, and mobile workspace backfill,
projection, recovery, and identity-normalization fixes. Firstmate-only worker
lifecycle integration is not included. OpenCode review was explicitly waived by
the user after the configured provider rejected review with a usage limit.

[Staging publication](https://github.com/Ashler-AI/comet/actions/runs/34421499147)
built desktop and both Linux Scaffold artifacts. All four downloaded artifact
checksums and the macOS bundle's strict signature verified. The packaged app and
live staging update feed both report **0.1.88**.
[Production promotion](https://github.com/Ashler-AI/comet/actions/runs/34422692407)
reused the exact candidate without rebuilding and successfully read back both
desktop and Scaffold manifests, checksums, and version pointers. Local production
update readback returned HTTP 401; production verification is from the authenticated
publication workflow. The installed/running desktop was not replaced or restarted.

[Edge production rollout](https://github.com/Ashler-AI/comet/actions/runs/34422649146)
passed remote typechecking, tests, and real local Edge/Rust collaboration smoke.
Its byte-identical candidate
`8c159f33d9619b4dc6d3d1841c1379ae437c6f85ae3021c6360b561604b7b6a7`
deployed to staging Worker `06163544-c132-4452-bfb2-d63fd80f49ff` and production
Worker `fa60a9ce-6e1d-48a8-9ac7-4df449a9e4e0`. Both live health endpoints returned
`ok: true` with the expected environment. Local typechecks were intentionally
skipped to preserve workstation resources.

Mobile staging **1.0 (16)** and production **1.0 (10)** passed all ten CI scenarios
and were accepted by Apple for internal TestFlight processing. Distribution
signatures, bundle/build identities, and production APNs entitlements verified.
Authenticated App Store Connect readback confirmed both builds fully processed
and available in the existing **Ashler Internal** groups; see the
[mobile release evidence](apps/ios/README.md#crew-0188-upload-evidence).
Live cold-Scaffold first-send acceptance and Lois's affected-account recovery
remain manual checks, not established by fixture or local collaboration tests.

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

OMP journal lookup scans the complete configured session store without a directory-count
cutoff. Large stores must not block resume, takeover, or fork merely because accumulated
session artifacts exceed 10,000 directories. Filesystem scan failures still prevent an
incomplete lookup from being treated as exhaustive.

Download the macOS DMG from the same release feed. For OMP transport details, see [the harness architecture](ARCHITECTURE.md#5-engine-plan). The installer bootstraps any missing agent CLI (OMP, Claude Code, Codex) after the comet install — failures there never abort the install, and `COMET_SKIP_AGENT_BOOTSTRAP=1` skips the phase for managed environments. An existing `omp` is never silently replaced; to bootstrap or validate it explicitly:

```bash
# Run the same private install.sh downloaded above:
sh install.sh --install-omp
```

This installs the official [oh-my-pi v17.2.9](https://github.com/can1357/oh-my-pi/releases/tag/v17.2.9) artifact to `~/.local/bin/omp` after SHA-256 verification against the per-platform pins in `install.sh` (darwin arm64/x64, linux glibc and musl arm64/x64). App updates can be started at any time from **Settings → Crew update**. The engine also tracks agent CLI versions on its release-check cadence; **Settings → Agents** offers per-agent updates through each CLI's own self-updater (`omp update`, `claude update`, `codex update`), and by default the first boot of a new Crew version refreshes installed agents automatically (**Settings** toggle or `COMET_UPDATE_HARNESSES=0` to opt out).

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

Handoff chats publish running, waiting, and terminal status under their chat ID,
including follow-up turns. Mobile follow-ups use a desktop controller, not the
ephemeral sandbox host: attachment resumes a paused sandbox and confirms its
current host authority before admitting the message. A reachable desktop
controller is required.
On the next command after an epoch change, the sandbox host transfers prior
session ownership only with a live, edge-verified grant for the same sandbox,
room, and principal. Queue, steer, and peer-message continuations do not
require another Start command to restore status publication.

Native transfer reads the sandbox checkout's exact HEAD before capture. When it
is a known source ancestor and the only bundle boundary, the archive contains only
the Git delta after that commit; matching HEADs transfer no Git objects. Dirty and untracked files remain
a separate verified overlay. The 256 MiB archive limit is unchanged.
Archive uploads have a five-minute total request deadline, separate from the
30-second control-request deadline. Cancellation still stops an in-flight upload.
The worktree verifier runs from a temporary sandbox file rather than exceeding
the runtime's 16 KiB exec-argument limit with inline program text.
The reconstructed checkout has a separate 1 GiB byte budget; compressed Git
objects and archive bytes are checked independently before checkout publication.

Scaffold reconstructs the exact source HEAD at `/workspace/crew-handoff`, preserving
a nested source cwd. Delta checkouts borrow the provisioned platform repository's
object store, so those base objects must remain available; its checkout, index,
and refs are left untouched. Without a usable shared ancestor, transfer includes
a self-contained shallow snapshot of the exact source HEAD, not its history.
Source shallow/partial clones are supported; missing selected promisor objects
are hydrated into private temporary storage through the source's existing Git
configuration, without copying it or growing the source object store. Fetches have
a hard per-file write limit and observed aggregate-storage checks. Linux applies
a per-process address-space limit; macOS samples process-group RSS and cancels on
overflow (between-sample overshoot is possible). Every fetched pack is accounted
against expanded-object limits before further hydration or bundling.
OMP context is rebased to the imported cwd. Capture/import fail closed on archive,
expanded-object, checkout-size, path, or symlink safety violations; Git submodules
are not reconstructed. Oversized deltas still fail rather than pushing a branch
automatically. Provisioning order is unchanged; a capture failure can still leave
the newly provisioned remote session awaiting recovery.

Starting with **0.1.86**, native handoff prepares only the captured **prior
conversation** for attachment replay. Recognized image, file/document, and audio
blocks keep inline base64 content; local content-addressed blobs are hydrated
only after regular-file, size, stable-read, and SHA-256 verification. Missing,
unreadable, corrupt, nonregular, or over-budget attachment blobs become explicit
historical-unavailable text markers. Path/URL attachment references that cannot
be carried as bytes also become markers; Crew does not fetch arbitrary URLs.
Ordinary text links, tool arguments, and unrelated session metadata are unchanged.
The original journal remains intact, and the prepared snapshot is rehashed for
the existing archive verification. The separately queued **current prompt** does
not pass through this fallback: current attachment/input errors remain errors.
This is a handoff preparation policy, not a global OMP provider retry policy.

**0.1.87** extends that boundary to actual OMP compaction archives at
`preserveData.snapcompact.frames`: available frames retain their metadata and
receive verified inline bytes; unavailable frames are removed with an explicit
warning prepended to the compaction summary. **0.1.86 did not cover this shape**,
and its installed-app retest still failed. A local smoke using the original
incident journal prepared all **15 frames across three archives**, with each
decoded SHA-256 matching its source blob and no unresolved frame/data blob
references remaining. This is local preparation proof, not remote execution proof.

The subsequent **installed 0.1.87** retest completed successfully in remote Crew
chat `c2a70e4f-bd3a-4244-8968-7c5b0619e06f`, sandbox
`rcs_a656652b49f3e396e7294643` (staging, local database), from the original
image-containing conversation. The remote model recovered native context and
verified source/platform repository isolation. Independent inspection of the
remote journal decoded all **15 original frames**, including the latest five,
and matched every SHA-256 to the saved local blob baseline; no replacement
markers substituted for those images. Desktop staging and production published
the same verified candidate from `4491b119c0de7ee8de3613979d34f971b5c767b8`:
[build/staging](https://github.com/Ashler-AI/comet/actions/runs/34398401078),
[production promotion](https://github.com/Ashler-AI/comet/actions/runs/34399706707).

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

## macOS computer-use permissions

Open **Settings → Permissions** in Crew. The page checks this desktop process
without prompting; its executable path and PID identify the app being checked.
Use **Request Accessibility** for inspecting and controlling other apps, and
**Request Screen Recording** for screen capture. Approve each request yourself
in macOS; agent tool approval does not grant operating-system access.

If a prompt does not appear, use the corresponding **Open System Settings**
button, enable the current app under Privacy & Security, then return and choose
**Refresh status**. “Not granted” can mean never requested, declined, or restricted.
macOS may require quitting and reopening Crew, especially for Screen Recording.
Remote sessions and separately launched headless daemons are not covered by the
desktop status. A terminal-launched process can have a different permission owner.

If the switches are already enabled but access still fails after an update, the
saved grant may belong to an older ad-hoc-signed binary. Install the final
Developer ID-signed release first, then remove and re-add that app in the affected
Privacy & Security lists and relaunch it. Moving from ad-hoc signing can require
one last approval; normal signed updates retain the same identity. Do not replace
the app with another build between approving access and retesting.

The independently packaged **Crew Staging.app** has its own permission identity;
it is not a launcher for the production app. See [macOS packaging](dist/README.md#macos)
for signing prerequisites, staging isolation, and migration from ad-hoc installs.
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

Crew **0.1.101** ships the expanded 1,536-name AEC worktree pool and exhaustive
allocation from merged source `64913c5019ee28e98f4abd411f9990bbee863676`.
[Build and staging publication](https://github.com/Ashler-AI/comet/actions/runs/34877909631)
passed the desktop/runtime tests and produced notarized Crew and Crew Staging apps.
[Production promotion](https://github.com/Ashler-AI/comet/actions/runs/34880371590)
reused the exact candidate and verified published desktop and Scaffold channels.
Both downloaded macOS distributions passed checksum, strict signature, stapler,
and Gatekeeper checks. Mobile staging **1.0 (22)** and production **1.0 (16)** are
available through their existing internal TestFlight groups; see
[mobile release evidence](apps/ios/README.md#crew-01101-upload-evidence).

Manual releases choose an explicit surface:

- `desktop` builds both signed macOS app variants. Staging advances `desktop-*` for the production-identity candidate and `desktop-staging-*` for Crew Staging; production promotion publishes only `comet-<version>-macos-arm64*` and advances only `desktop-*`. Both apps support in-app download and restart without changing bundle identity.
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
`body.error` envelope) keeps one native Attach operation waiting while startup
remains valid and cancellable, without a fixed healthy-start deadline. Each wait
rechecks the sandbox, owner, room, and lifecycle
epoch; unrelated 404s and terminal states still fail. Manual retry uses the same
accepted sandbox. A failed launch without an accepted remote target discards its
pending draft; confirmed deletion discards an accepted pending draft. Deleting a
persisted chat keeps the ordinary chat-deletion behavior.
Attachment alone does not complete startup: Crew retains the accepted draft until
the first command is admitted, so a checkout or upload failure can retry against
the same sandbox. Navigating away does not cancel admission; failed prompts return
to their originating session rather than replacing another session's draft.
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
