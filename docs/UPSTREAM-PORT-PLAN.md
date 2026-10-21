# Crew on current upstream: port and update plan

## Current delivery: staging native cutover 0.2.116

The staging cutover replaces the maintained legacy implementation with this
upstream-based source. Production deployment, release channels and profiles remain
unchanged. The earlier Canary-only constraints below describe historical testing,
not the current staging authorization.

Publish an immutable `staging-candidate` first; deploy compatible staging services,
establish authoritative old-writer fences and verify retained-history migration,
then activate the native runtime and promote the exact candidate. Follow
[`skills/comet-work/SKILL.md`](../skills/comet-work/SKILL.md); candidate publication
alone is not activation or a successful migration.

The earlier [0.2.115 candidate](https://github.com/Ashler-AI/comet/actions/runs/39643038860)
was published immutably from `2aa451d1` without moving channels. These follow-on
repairs use 0.2.116; they must not replace the already-published 0.2.115 objects.

Integration with native main `79ce43e3` passed 48 release/packaging/privacy/signature
checks, nine native web contracts, and the real Linux tarball, release-install and
integrity/rollback smoke. After corrections, the five previously failing targets
passed 63 focused checks: filesystem watching (10), device routing (11), local-first
lifecycle (9), queued attachments (3), backup (9), and import (21). These are combined
results from integrated runs and focused reruns, not one all-pass global invocation.
The first attachment timeout remains unexplained; single-case, concurrent diagnostic,
and final uninstrumented checks passed without a speculative product change.

The rebuilt `scripts/dev-demo.sh --slow` instance accepted a prompt through the
native composer and rendered the completed streamed mock reply. This used fresh,
isolated data; installed profiles were not replaced. Historical-import and
authentication-redirect review cleared, but none of this is live cutover acceptance.

The backup destination fixture now uses a host-native absolute temporary path.
Sandbox materializer process checks run on Unix, matching the sandbox's POSIX
Python contract; portable archive and handoff checks still run on Windows.
The SQLite contention check covers deferred reclamation, retained data and restored
write timeouts, not an aggregate open/reclaim wall-clock deadline. The macOS CI
deadline failure did not establish a production busy-timeout defect.

Follow-on fixtures await completed file indexing and distinguish terminated Linux
catalog descendants from unreaped zombies, while still rejecting live children.
Native watch reconciliation keeps its notification assertions; its completion
bound allows measured 12–14 s FSEvents RPC work under parallel test load. The
macOS job allows a cold build beyond the observed 45-minute cutoff without
extending headed-fixture timeouts. Handoff bootstrap retirement now reports its
blocked state on failure; the earlier timeout has no established runtime fix.
The workerd suite bounds concurrent Miniflare/SQLite instances to two. Its
300-wake test retains the 64-inflight acknowledgment boundary and durable
retirement assertions; passive timing observed completion at 6.444 s, beyond
the old five-second fixture deadline. All 41 workerd checks passed afterward
without diagnostics, skipped cases or reduced in-test concurrency.

Prior authenticated Apple metadata identified staging app `6808397041` with
1.0 (36) Testing. Build 1.0 (37) is an unallocated candidate, not a freshly verified
next upload: the 2026-10-21 metadata attempt redirected to login with
`authResult=FAILED`. Revalidate current build/upload metadata before allocation.
No new staging build was uploaded; the staging LiveActivity distribution-profile
requirement also remains unresolved.

The integration owner's arm64, ad-hoc-signed iOS Debug `build-for-testing` passed,
including Rust core compilation, UniFFI regeneration and Swift linking. The
generic simulator caller initially requested unsupported x86_64 as well; explicit
`ARCHS=arm64 ONLY_ACTIVE_ARCH=YES` corrected it without global architecture changes.
A fresh iPhone 17 Pro/iOS 26.5 simulator passed seven `CrewIsolationTests` and
`SessionFlowTests/testSendEchoesAndStreamsReply` (eight tests, zero failures).
Evidence: `/tmp/Crew-Staging-Native-Smoke.tT9JjF/OfflineSmoke.xcresult` and sibling
`Attachments/`. Both inspected screenshots show completed reply rendering, not
mid-stream visuals. No existing simulator profile was modified or credential
imported. This is offline mobile build/runtime proof, not authenticated same-bundle
migration, physical-device acceptance, TestFlight delivery or a global native-suite
pass. The reproducible isolated commands are in [`apps/ios/README.md`](../apps/ios/README.md).

Activation remains gated on complete migration and actual runtime verification.
The running legacy engine kills its supervised OMP processes during shutdown;
durable-history recovery must not be described as uninterrupted live-job transfer.
Do not activate an incompletely reconciled profile or claim a live copy is frozen.

The canonical native staging profile is `~/.crew-staging`; its owner controller
must be independently authenticated for `scaffold-staging.internal.ashler.com`,
`comet-staging.internal.ashler.com`, and project/deployment `ashler-staging`.
No such local controller record is currently present. Retirement source `320a9ef2`
globally fences legacy connections; it is not a harmless `/auth/session` preflight
deployment. Prepare the candidate and compatible services first, then establish a
real cross-store writer barrier, capture the legacy originals, and activate native
Edge at the canonical origin. Only then use login-only to authenticate the native
controller and verify the offline import before starting its session engine.
Do not manufacture identity fields or mark live writers stopped.

## Historical verification: isolated upstream canary

The port is under implementation on `crew/upstream-canary`, based on upstream
`3b2f78027a2c31622c39c35716da4d6e1af8de15` (v0.2.113). The legacy Crew source
revision is `297717c55beab9323c53095f3f5f683f00676a5e`; its working tree, installed
Crew Staging app and live profiles remain untouched. Platform changes use the
separate same-named branch based on `c48442b7f433cb0c9e8c03885d7020c3cc5b1807`.
Platform's initial source is committed and pushed as
`4d877f1bd87feef03ec6f114a20c46f4d7882f2c`; its
[remote Worker/provider/Agent Auth CI](https://github.com/Ashler-AI/ashler-platform/actions/runs/38847118006)
passed, including remote typechecks and runtime contract suites.
Canary sign-in's missing IAP audience was corrected in Platform `77341592d7`
and deployed as canary Helm revision 2. The actual browser receives verified
`auth: iap`; anonymous authorization redirects to Google and forged identity
headers remain HTTP 401. The user completed desktop sign-in; IPC confirms the
same canary identity and preserves it across application restarts.
Crew's initial integration is `eccc3324`; verified relay cleanup and migration
checks are committed and pushed as `c5eda252`. The initial
[private artifact and Edge CI run](https://github.com/Ashler-AI/comet/actions/runs/38848735417)
passed on macOS, Linux x64 and Linux ARM64, including remote Edge typechecks.
Native runtime/browser integration is committed and pushed as `045be1b9`, with
the private continuation scheme corrected in `225cc0df` and device-resume auth
corrected in `69f631b6`.
[The exact-source artifact build](https://github.com/Ashler-AI/comet/actions/runs/38975530294)
passed on macOS/Linux x64/Linux ARM64, including remote Edge typechecks and native
browser contracts. Both Linux checksums passed and the running local app selects
the new executable. The rebuilt desktop retained all 17 conversation IDs and its
signed-in scope. No shared release channel was published.
This is a testing lane, not production or a completed cutover.
The current delivery target is separate desktop/Scaffold canary deployments and
a distinct Crew Canary TestFlight app for daily-workflow validation. The user
waived the standalone independent Google OAuth acceptance check for this dogfood
delivery; authentication and scope enforcement remain unchanged. Existing Crew
release channels and live profiles must not be replaced or migrated.

The device dropdown now offers **Scaffold (Local DB)**, **Scaffold (Staging DB)**
and **Scaffold (Production DB)** using the same row styling as devices, replacing
the separate **New / use sandbox** entry. Selecting a destination stays in the
new-chat composer and does not provision anything. Submit uses its existing model,
reasoning/options and attachments, then durably creates and attaches the sandbox
before sending. Unknown outcomes retain the command ID and draft; checking them
does not replay Create or Attach. The attached chat persists the selected settings.
Canary snapshot provisioning remains governed by the local-only configuration.
The sandbox settings page omits source-branch/region textboxes and reuses the
searchable model picker with full labels. The native UI verified destination-only
selection leaves the chat count unchanged, model search, and the simplified form.
Transcript headers/sidebar details distinguish Scaffold from local execution,
including lifecycle/offline state. Both live composer checks passed: the retained
image prompt returned its exact generated text once, and a fresh sandbox using
the `fade0064` runtime returned `CREW_COMPOSER_MEDIUM_OK`. The selected `medium`
reasoning level persisted in chat `8676e377-ea1e-4c20-9be7-20e1ba2a0a83` and was
read back through the independent dogfood controller.

The account page now uses the existing settings cards and usage meters, shows
reset deadlines, and opens the existing shared Agent Auth sign-in flow for each
provider. Refresh no longer depends on sandbox discovery; native Refresh and
both provider sign-in dialogs were exercised without granting new credentials.
Sandbox maintenance and diagnostic fields sit behind disclosure controls;
reconciliation precedes new operations. Desktop and mobile artwork now uses the
green center and four white pieces on black, generated from `dist/zeron.svg`.

The intermittent history failure came from device-wide RPCs reaching a
session-scoped sandbox link. Admission now occurs in the shared forwarding path,
before acquiring that link. Four affected histories loaded in the rebuilt
controller; an unsupported `WatchHarnessUpdates` request was rejected locally
and the same sandbox history remained readable. The four focused Rust checks
(shared login import recovery, scoped admission, persisted-identity validation,
and usage reset deadlines) passed. Five signed simulator isolation/error checks
also passed. The final desktop build timed out locally after 15 minutes rebuilding
dependencies; the exact-source remote build below passed and supplied the tested
installed desktop. Local typechecks were intentionally skipped.

[Initial branded artifact CI](https://github.com/Ashler-AI/comet/actions/runs/39334305241)
passed for exact code revision `9c2268c58871eaaf1342dee15033b15905df9174` on macOS,
Linux x64 and Linux ARM64, including remote Edge typechecks and browser contracts.
Archive checksums, executable checksum and strict app signature passed. The
verified macOS bundle was installed and activated at `/Applications/Crew Canary.app`;
all 26 conversation IDs, the original desktop device ID and signed-in scope were
retained. All four previously failing sandbox histories retained their exact
message IDs. Native maintenance disclosure and account controls were exercised
in this CI-built app. Delivery stays private/manual; no shared release channel
or existing Crew Staging installation was changed.
Isolated Edge `d6780161-12a7-4da3-a606-31984ae095fc` is deployed; its live health
response confirms Scaffold auth and the `crew-upstream-canary` environment.

The separate iOS `ai.ashler.crew.upstream-canary` app from source `9c2268c5`
is uploaded as **1.0 (3)** to **Crew Canary by Ashler** (App Store Connect app
`6824662308`). Apple processing completed; the
[TestFlight group](https://appstoreconnect.apple.com/teams/678e64ca-9800-489a-9076-4e4195f35838/apps/6824662308/testflight/groups/169c6912-7fa0-4c1c-934d-50c906fd48f1/builds)
shows **Testing**, one internal tester and two builds. `Canary dogfood` contains
only the authorized account, with automatic distribution disabled. Cloud-managed
signing preserved the production push entitlement, verified in the exported IPA.
Apple's processed build icon shows the new green/white mark on black. The upload
retains the non-blocking missing WebRTC dSYM warning; framework crash symbolication
is incomplete. Apple previously confirmed **1.0 (2)** installed on an iPhone 18 Pro.
After being asked to unlock the test iPhone and verify **1.0 (3)**, the user
confirmed "unlocked and verified". This closes the physical-device hands-on gate
for Canary dogfooding; it is user-reported acceptance, not an automated device run.
No existing App Store app identity or shared Crew release channel was changed.

Signed simulator checks exercised the promoted sandbox entry before the device
inventory, online-controller choice, provider-qualified model selection, owned
sandbox selection, and retained `CREW_COMPOSER_MEDIUM_OK` history. The maintenance
menu still opens a scoped Stop confirmation; it was dismissed without executing.
Source/region inputs are removed and optional diagnostic fields start collapsed.
The desktop created then resolved one retained annotation on its dedicated smoke
conversation; the independently authenticated controller received the same ID,
exact text anchor and resolved state. No existing user conversation was edited.

The final fresh-runtime smoke used sandbox `rcs_93416ae0420298468f4a4aef` and
session `720d4a20-e8da-4521-bb09-4142c702d0b1`. Attachment enforces the pinned
Linux executable checksum before launch; the remote device reported `0.2.113`.
The native turn and two actual simulator continuations produced
`CREW_CANARY_9C_RUNTIME_OK`, `CREW_CANARY_MOBILE_OK` and `CREW_CANARY_STATUS_OK.`.
All six complete message IDs were retained and visible in the installed desktop
and simulator. Physical-device hands-on acceptance was subsequently confirmed by
the user as recorded above.

This smoke exposed a scoped Edge schema mismatch: `lastCompletedTurn` was typed
as a number, but native completion IDs are strings. Whole-batch rejection left
the session showing `working` after completed inference. `3bdfc486` corrects the
field type without expanding host authority; five focused workerd checks passed,
including owner-visible completion and transactional rejection of invalid types
and foreign identities. The live watcher observed `working` to `idle` at
`2026-10-19T23:15:48.935Z`, with the final assistant ID retained as the completion
ID. Authoritative Edge and controller reads subsequently agreed. The initial
90-second observer timed out before convergence; no restart, re-attachment,
status override or prompt replay was used, and the delay's cause is unproven.
The same smoke sandbox was then confirmed stopped (epoch 2); local native history
and the installed desktop retained all six exact message IDs. No other sandbox
was stopped and the conversation was not archived or deleted.
[Exact-source CI](https://github.com/Ashler-AI/comet/actions/runs/39339937493)
passed for the Edge correction, including remote typechecks, all Edge tests,
native-browser contracts and macOS/Linux x64/Linux ARM64 packaging.

Stop acceptance also exposed MCP `read_chat` and `get_chat` forcing requests to
the stopped host despite complete synchronized history. `b9b36ab5` routes both
read-only consumers through the existing native `transcript` path; live send,
input-response and turn-wait reads remain explicitly host-targeted. The actual
CI-built MCP executable returned all six retained complete IDs and
`get_chat.messageCount:6` with the final assistant marker, without a target-device
override or fallback. No `openingTail` preview is requested by this path.
[Final native artifact CI](https://github.com/Ashler-AI/comet/actions/runs/39341325253)
passed for `b9b36ab5dff291871d6977dcfff8d9128b48fbae` on macOS and both Linux
architectures, with remote Edge typechecks/tests and native-browser contracts.
Archive checksums, Linux executable checksum and strict macOS signature passed.
The final bundle is installed and activated at `/Applications/Crew Canary.app`;
its executable SHA-256 is
`cde30d3764fb1cb13fd85aca67a2c4599075f409833934c06c7e64c582cbad06`.
The restart retained all 27 conversations, both synchronized macOS device IDs,
all three pre-restart working-session host identities and their message IDs,
and the six-entry stopped smoke history. Generic app activation did not expose
Canary IPC; the documented explicit profile/port launch restored the correct
controller. Keep those coordinates pinned when launching from another Crew
agent. No existing sandbox was paused or restarted. TestFlight remains 1.0 (3):
the final caller correction is desktop/CLI-only, not an iOS change.

The October 20 interruption investigation found Crew forcing OMP's native turn
recovery off. The reported stream-idle failure coincided with host clamshell
sleep/dark wake; this is not evidence that Agent Auth rejected the request.
Crew now leaves retry limits and replay-safety decisions to OMP rather than
adding a second HTTP or turn-replay loop. Terminal RPC failures carry their
visible error only on `Done`, after reaping the writer, avoiding two error cards
for one failure. Previously saved transcript entries are not rewritten.

The real OMP 18.11.1 smoke passed through a loopback Responses SSE endpoint:
one native file read completed, the next request stalled, and native recovery
returned `RECOVERED` with the completed tool result retained once. No provider
credentials or live sessions were used. All 12 non-ignored native OMP regression
checks passed, including final-error rendering and writer release. The smoke
keeps HOME separate from its workspace because OMP intentionally leaves HOME
on startup; full tool output is checked in native events/history, not the
deliberately output-free synchronized tool chip.

One-shot CLI commands now resolve explicit/inherited IPC routing without saving
it into `crew-runtime.json`; intentional application, headless, auth and daemon
startup still persist their launch settings. This prevents a Canary CLI invoked
from Crew Staging from changing the next Canary launch's port. Private artifact
CI includes native recovery and actual CLI subprocess regressions.

[Recovery artifact CI](https://github.com/Ashler-AI/comet/actions/runs/39367786890)
passed for `52552efcf598837c528b5d9c64c310aac72d78ff` on macOS and both Linux
architectures, including 12 native OMP and two CLI regressions, remote Edge
typechecks/tests and browser contracts. Archive/executable checksums and strict
macOS signature verification passed. The packaged CLI read a real retained
transcript through inherited IPC 27954 while its private profile named a different
test-owned port; both that profile and the live Canary settings remained
byte-identical.

The verified app is installed and activated at `/Applications/Crew Canary.app`
with executable SHA-256
`cb7816c09053350d1a5fd216628ccbb6a6596e00b6f4ba362318f5067a85900d`.
Restart verification retained all 28 chat IDs, the same synced controller identity,
all 26 entries across six sampled transcripts, and the three previously working
remote session/host pairs. The affected native OMP history ID is unchanged.
Fresh native screenshot/AX showed the signed-in controller and retained transcript;
workspace and focused-chat sync are connected/synced with no pending pushes or
durable jobs. The new Linux x64 artifact is pinned for future Canary host launches,
SHA-256 `2f0d063a1bd23b90a5bb6afaa2b5b83b051b1d023fe113a60d8df6e1d37daef3`.
Existing sandbox hosts, Crew Staging, Canary Dev, TestFlight and shared deployment
channels were not restarted or changed. Local typechecks were intentionally skipped.

Scheduled coordination resumed in Scaffold session
`95e5f344-d332-445d-b91e-dd2e691153f6` after its earlier Agent Auth 503. The peer
owns Platform `/private/tmp/harness-pr` and the harness deployment; this session
owns Crew. Its contract review identified shared loopback, incomplete filesystem
and privilege isolation, missing native DATA checkpoints and missing same-epoch
native-process supervision. These are source findings, not a live exploit test.
Distinct IPC ports are not a security boundary. Live harness testing remains
gated on those fixes and an isolated `crew-upstream-canary`/chat2 lane; the existing
Canary profile must not be retargeted to legacy shared staging Edge.
At 2026-10-20 09:46 UTC, the peer stopped on an Anthropic safety refusal before
finishing that prerequisite handback. No retry or provider switch was attempted;
explicit interactive approval is required before resuming that session.

Crew integration source [`fc12483c`](https://github.com/Ashler-AI/comet/commit/fc12483c7fa0aed93b84e8aaa03e755c8ff78704)
separates logical sandbox storage from the
device/epoch-bound authentication fingerprint. A validated storage binding keeps
current-authority legacy roots and absolute attachment paths in place; it must be
saved with the data. Earlier-epoch legacy roots without a prior verified binding
still require explicit recovery, never foreign-store scanning or credential reuse.
Explicit Resume uses native attachment and confirms only live, current-epoch host
authority; observation of provider readiness alone cannot confirm it. Unknown
mutations are not replayed, and stopped/failed resumes retire.

All 47 focused Rust checks passed (four managed-auth/storage checks, 42 Scaffold
checks and the resume-readiness check); the native application build also passed.
An actual three-process headless smoke upgraded an isolated legacy store, then
changed its lifecycle epoch while retaining the open SQLite store, processed and
pending command records, and an absolute-path attachment. It exercised one saved
authority refresh, two fresh enrollment exchanges and three graceful shutdowns.
Authentication used a loopback fixture, not real Scaffold or provider credentials;
this is not cloud cold-restore or end-to-end harness evidence. The initial observer
missed one-shot RPC `ok` replies; correcting that fixture produced the passing run.
Existing compiler warnings remain; local typechecks were intentionally skipped.
These changes have not replaced the installed Canary runtime or a shared lane.
Private artifact-only [CI](https://github.com/Ashler-AI/comet/actions/runs/39390433796)
passed for that exact source on macOS ARM64, Linux x64 and Linux ARM64, including
remote Edge checks. Downloaded archive checksums, extracted Linux executable
checksums and the macOS app signature verified. These private artifacts are not
an installed-runtime or cloud-rollout claim.

Launcher source [`82eedae4`](https://github.com/Ashler-AI/comet/commit/82eedae49a738a7e53ab6016eafbe1598c2bc084)
passes its verified control origin, project and
deployment through existing CLI flags instead of relying on provider environment
variables. Without them, the actual CI executable exited before enrollment with
`device bootstrap requires a configured Scaffold authority`; with them it returned
the expected `ScaffoldHostAuthority` and shut down gracefully. This used isolated
loopback authentication. All 42 Scaffold checks passed after the launcher change.

Its private artifact-only [CI](https://github.com/Ashler-AI/comet/actions/runs/39400100955)
passed on macOS ARM64 and Linux x64/ARM64, including remote Edge and native-browser
checks. All downloaded archive and Linux executable checksums verified; the macOS
app signature verified. The exact macOS CI executable passed the isolated native
authority smoke with one enrollment exchange and graceful shutdown. These are
loopback-fixture results, not live harness integration or deployment evidence.
Verified Linux executable SHA-256 values (not active pins):
- x64: `2a6128c7e421a69f353fc5d03236a35a84bc5922c6bfd313ac43b4fbe0db097a`
- ARM64: `4cd868e36a97ce30480c8e91c60bff4c86bdd176076cf710787d0b41475f945b`

Fresh installed-Canary readback retained all 28 baseline chats and the same
controller, with connected/synced workspace and zero pending durable writes.
No installed runtime, existing host, shared release channel or deployment changed.
The independent review tool failed on provider HTTP 402; the bounded launcher
producer/consumer review was completed inline. Local typechecks remained disabled.

The October 20 local-Canary recovery found the installed host stopped and Canary
Dev retrying 15 directory jobs with no local transcript snapshots. Dev logged 188
stale-source conflicts in one minute; both clients shared a credential/rate-limit
bucket and encountered control HTTP 429 followed by Edge HTTP 503. The worker now
keeps server versions only as in-memory prerequisites: it leaves conflicted jobs
durable and performs no further lookup until that chat's actually observed source
dominates the prerequisite. Remote counters never become projection evidence, and
transient GET/upsert failures remain retryable.

All four directory regressions passed, including loopback HTTP request counts,
unrelated wakeups, snapshot catch-up, fresh-worker recovery and transient failures.
The isolated headed demo built and rendered completed mock turns; its initial
300-second readiness wait expired during build/seed, then it became ready normally.
Recovery retained all 29 chat IDs, controller/model/native-history identities and
resumed both interrupted local runs. Launches from a Crew agent must explicitly
set Canary IPC 27954 (Dev 27955) rather than inherit Staging's `COMET_IPC_PORT`;
the accidental misattachment during recovery was closed and saved routing restored.
No shared engine, remote host, deployment lane or authentication guard changed.
The rebuilt, signature-verified Dev observer and recovered installed host then ran
together for a measured 120 seconds: both retained all 29 chat IDs and device
identities, stayed connected/synced, and logged zero repeated stale-source retries,
control 429s or upstream 503s. Pending directory jobs remained durable (16 on Dev,
26 on the installed host); they were not discarded to make the check pass. Native
screenshots showed the recovered local run and its latest transcript in Dev.

Directory-retry source [`49ef1539`](https://github.com/Ashler-AI/comet/commit/49ef1539b20b6aa8afb1ff3bbe2fff046604906a)
passed private artifact [CI](https://github.com/Ashler-AI/comet/actions/runs/39451135985)
on macOS ARM64 and Linux x64/ARM64, including the directory regression, native
recovery/storage checks, remote Edge typechecks and browser contracts. The verified
macOS artifact is installed and running at `/Applications/Crew Canary.app`, executable
SHA-256 `96a9880d145efa6314333062099fbf48001b539bb237cf1a8de79fb49ae892a1`.
Canary Dev runs a signed local build of the same source. Post-installation native
UI and IPC verification retained all 29 chats and their model/native-history
identities. Both clients stayed connected/synced for another 120 seconds with zero
stale-source retries, 429s or 503s; the host had no pending durable work and Dev
retained 15 intentionally parked directory jobs. The other Platform session kept
its original active run. Runtime host pins and shared release lanes are unchanged;
local typechecks were intentionally skipped to preserve workstation resources.

The subsequent connection-warning investigation found redundant verification on
every user-bearer read: 24 concurrent account reads through the installed binary
made 24 control and 24 Edge verification requests against an isolated authority.
The shared desktop/mobile auth helper now coalesces verification and reuses a
server-verified identity for at most 60 seconds. Both monotonic and wall-clock
age bound freshness across suspend; cold starts still verify. Cancellation
fences queued/in-flight results, and credential, subject, resource, project and
deployment boundaries remain checked. Protected requests still authorize on the
server; a fresh local identity cannot override a denied operation.

Transient failures share one cooldown, honoring numeric/date `Retry-After`
without an upper cap. A one-second local minimum prevents zero/past-date retry
storms; absent/invalid headers use bounded 1–5-second backoff. Expired success is
never reused during an outage, and definitive rejection cannot revive it.
The 11 shared-auth regressions passed. In the actual rebuilt headed app, the same
24 concurrent account reads took 25 ms and added zero verification requests;
a protected HTTP 403 still failed. After expiry, 48 consumers shared a 429
cooldown without extra verification or sign-out, then recovered through one
verification pair without restarting. This used a private loopback fixture,
not a live backend outage. The desktop helper was closed, so native macOS window
capture supplied the headed screenshot; no permissions were changed.

The complete targeted run passed 25 checks: shared auth 11, engine auth 6 and
client auth 8. The first command exhausted its deadline during compilation;
the remaining suites completed after resuming. The engine regression initially
counted an independent cold-auth instance in its warm-read expectation; it now
measures the warm-read delta instead of pinning that incidental cumulative count.
The signed local Canary Dev build retained all 46 baseline chat IDs, its device
and native-history identities, and signed-in connected/synced state after restart.
Independent native-provider review found no blocking correctness/security issues;
the specialized review provider was unavailable with HTTP 402. Local typechecks
and unrelated lint were not run.

Auth-only source `0e481e40e82050af1b6d0253b8abdbd2df493720` passed the private
[artifact workflow](https://github.com/Ashler-AI/comet/actions/runs/39516859297)
on macOS ARM64 and Linux x64/ARM64, including remote Edge and browser checks.
All archive and Linux executable checksums and the strict macOS signature passed.
The macOS executable SHA-256 is
`53cadd856d06f017faaf610d9557b7b1e4ee221de192b20e21109baa5fca5078`.
This auth-only candidate was installed and activated at `/Applications/Crew Canary.app`.
Both controllers retained all 48 baseline chat IDs and their device/native-history
identities. The sole locally working session at activation resumed on the same
native history and produced new tool calls; its original user entry stayed singular.
No user prompt was manually resent and no durable work was discarded.

From `2026-10-21T01:15:12Z` through `01:17:12Z`, both controllers stayed
connected/synced with zero additional workspace disconnects or verification
429/503 entries. The host's existing durable backlog decreased from 5,852 to
5,593 batches; Dev retained its parked jobs. Generic backend chat/registry HTTP
503s recurred after `01:17:55Z`, outside that observation window. This fix removes
redundant verification; it does not hide or claim to repair backend outages.
That candidate did not include the separate Unicode selection correction below.

The October 20 `23:50:10Z` installed-app crash was a separate markdown-selection
panic: byte 33 split an em dash at bytes 31–34, then the non-unwinding native
callback aborted the app. Relaunch automatically resumed all three interrupted
native sessions; the reported `b818292b` conversation retained its native history,
model and worktree and subsequently completed its deployment-monitoring report.
Selection offsets retained across frames were only length-clamped when paired
with newer text. Span producers now snap both endpoints to UTF-8 boundaries
using the existing left-affinity policy; fully selected virtualized overlaps
retain whole-element coverage after text changes. Copy still slices strictly.

An isolated check against the prior source reproduced the exact byte-33 panic;
all 12 updated selection regressions and the application build passed. The
headed native fixture copied ` café — 中文 🙂 after` exactly, then copied
`Doc command — the composer queues a durable run entry` across styled spans.
Fresh screenshots showed selection highlighting without a crash; focus and the
original clipboard were restored. Private artifact CI now runs this stdlib-only
selection module on macOS and Linux, without adding a UI test framework.

Unicode correction `adb989e8a2ab7d6fd752f9287f73b1892535f646` passed private
[artifact CI](https://github.com/Ashler-AI/comet/actions/runs/39528473449) on macOS
ARM64 and Linux x64/ARM64, including the 12 selection regressions and remote
Edge/browser checks. All archive/Linux executable checksums and the strict macOS
signature verified. The corrected app is installed at `/Applications/Crew Canary.app`,
with executable SHA-256
`8fc47da3aa2a8bbaf14805e3fedc4cbbae4b579918fb609e95e13f6b24019b92`.
Canary Dev runs the signed local build of the same source. Both profiles retained
all 51 baseline chat IDs, device/native-history identities, model/config and working
directories; fresh native screenshots showed populated chat surfaces after activation.
Shared release channels, Crew Staging, existing sandboxes and their runtime pins
were unchanged. No durable queues were manually cleared.

The installed-app restart cleanly interrupted the sole working local run
`fc016733`: its four original entries and native identity remain, but the last
assistant entry became aborted without automatic continuation. The native journal
recorded `Interrupted by user` and `session_exit: sigterm`; Crew's journal ended
in `Done(interrupted)`, which intentionally excludes it from crash recovery.
Transport and control verification HTTP 429s plus backend 503s are separate from
that recovery decision. The live `02:24:32Z`–`02:26:33Z` continuity observation
failed its connected-state assertion; synchronized metadata at startup is not
evidence of a stable connection. No clean post-activation sync window is claimed.

Recovery used the existing composer `QueueCommand` path with one explicit
continuation limited to already-authorized work, not a replay of the original
request. Its first admission failed before native launch on Agent Auth HTTP 429.
One retry five minutes later used a fresh command ID and the same message ID,
preserving the single continuation entry and the visible first-attempt error.
At `02:35:01Z`, the same native session `01a15b69-07ec-754c-b4d3-d9a345be5f2b`
was producing fresh output; its original four entries, model/config and worktree
were intact. Both profiles still held all 51 chat IDs. Dev had also reconciled one
other sandbox chat's host from epoch 6 to epoch 8; no host assignment was edited.
Both controllers were connected/synced at the final snapshot, but the failed
observation window and remaining backlog still prevent a stable-sync claim.

Native chat2 supervision is deployed only to the isolated canary. The missing
native `session.sync.write` bootstrap capability is fixed in Platform
`30fff0f0f0d`, which passed [remote Worker/provider/vault CI](https://github.com/Ashler-AI/ashler-platform/actions/runs/38973661026),
including typechecks and supervised-runtime suites. Eight local bootstrap checks
passed; legacy grants retain their previous permissions. The final authority and
live-epoch fixes in Platform `880a8d5bb0d` passed
[remote Worker/provider/vault CI](https://github.com/Ashler-AI/ashler-platform/actions/runs/38984328096),
including remote typechecks and supervised-runtime suites. Verified canary Worker
version is `9868c317-f624-4dba-9f22-6ff8125ba7b5`. The guarded isolated launcher
reported no pending D1 migrations; these fixes were not deployed to a shared lane.

[Cloud Build](https://console.cloud.google.com/cloud-build/builds/ae0b6339-6d2a-44a9-9e5a-2ff398a7051b?project=54440053364)
published provider digest `e43f40cfce5594d98cbc420073d18a656d233833396dd69d1a818baba1103143`
and runtime overlay digest `24070ec6cee010a7c57e34425b700cd07bf3b2879ed3135754106dfa239f398a`.
The overlay retains the complete prior session image and replaces only the
reviewed supervisor/agent bridge. E2B template `crew-upstream-native-06dfeec5c75`
(ID `nj0xn9i97lgeefdpf06s`, build `ed882f0f-51ce-473d-ac6d-eb4c1e3cd571`) built
successfully. Helm revision 7 selects it using its own release-owned pointer,
`crew-upstream-canary-template-06dfeec5c75`; shared template pointers are unchanged.
Deployment pins are committed as Platform `0ee2c1daa7f`.

The host waits for the checksum-verified executable and atomically installed
embedded browser assets. Startup authority is independent of browser readiness:
native context must arrive before the browser can report healthy. The deployed
native browser, managed inference and lifecycle sequence passed the live smoke
below. Legacy/shared lanes retain their existing protocol selection.

The owned native sandbox `rcs_5ecdcc8ec00c8f8fe2a3f056` created successfully,
paused at epoch 2, and accepted a same-ID resume at epoch 3. Resume exposed a
separate auth bug: a saved revoked credential took precedence over the fresh
bootstrap. Crew `69f631b6` fixes new-grant selection while retaining refresh for
an already-consumed grant. All three managed-auth regression checks passed.
An actual isolated CLI smoke with a loopback test authority completed initial
epoch-8 enrollment, epoch-9 re-enrollment, and restart using the consumed bootstrap;
observed requests were exchange, exchange, refresh. This is not live E2B evidence.
The rebuilt desktop retained all 17 conversation IDs and its signed-in scope.
The failed resume sandbox was stopped at epoch 4 without replacement; its rejected
second pause did not change the observed epoch. The corrected artifact installed
into `rcs_45db1f808bb3cb58aa18ec6b`; attachment confirmed all seven native host
capabilities. This sandbox reached `ready`, and its authenticated browser showed
the assigned native conversation and managed model. The first submitted turn
retained its prompt but was rejected because the authority response dropped
`authorityId`. Platform `da0ff59bed2` preserves the issuer metadata; an incomplete
test fixture initially failed remote CI and was corrected in `8bae3838235`.
The authenticated browser then completed `CREW_NATIVE_CANARY_OK` through Agent
Auth, and the desktop's native transcript subscription received the same completed
assistant entry.

That same sandbox paused at epoch 2 and resumed at epoch 3. Resume initially
returned an unknown outcome; reconciliation of the original command confirmed
`ready` without repeating Resume or creating a replacement. The resumed browser
retained its earlier history. Its next turn exposed a scope check against the
creation-time profile device ID rather than the live lifecycle. Platform
`880a8d5bb0d` uses the existing canonical device-ID helper with the current epoch.
The regression failed before the fix; all 19 device/broker/authority checks passed
afterward, including stale-grant rejection. After isolated deployment, the browser
completed `CREW_NATIVE_RESUME_OK`, and the desktop received that completed entry.
All six entries, including the rejected attempts, remained readable through a
fresh desktop subscription after Stop confirmed epoch 4. The scoped environment
list contained only the same stopped sandbox; the browser showed Reconnecting
while retaining its history. This is real managed inference and live E2B evidence.

Local verification: native command admission/recovery 4/4, Scaffold 41/41,
coordinator/RPC 2/2 and native installer 2/2; browser HTTP contracts 9/9,
supervision/agent contracts 23/23, provider boundaries 14/14 and controller
contracts 56/56 passed. An actual isolated engine run installed the embedded
assets twice, completed a mock-inference turn, admitted a repeated command only
once, returned its durable outcome without payload, rejected conflicting reuse,
and rejected a stale interrupt without admission. Mock inference is not evidence
of sandbox-hosted Agent Auth execution. The rebuilt local app retained all 15
chats and its signed-in controller scope. Real attachment reconciliation against
the previously running native host returned confirmed while application startup
was pending, published the correct workspace/host binding, and a subsequent Stop
confirmed that same owned sandbox stopped without replacement.
The initial default-profile Rust build timed out; the established low-memory
build profile completed. No local typechecks were run.

| Area | Exercised evidence | Remaining acceptance |
| --- | --- | --- |
| Desktop | Final CI-built app retained all 27 conversations and three working-session identities/histories; shared-account Refresh, usage resets, both sign-in dialogs, sandbox maintenance disclosure and annotation create/resolve exercised | Real provider credential grant remains user-interactive |
| Isolation | Distinct canary bundles/profiles; approved simulator session import retained; TestFlight 1.0 (3) is Testing for the sole authorized tester, who confirmed the requested physical-device verification | Standalone Google OAuth check waived for dogfooding |
| Sync | Two controllers retained sandbox histories and synchronized a real annotation; authenticated iOS warm/cold links, socket-interruption recovery and two actual simulator continuations retained history; live completion reached idle with its native turn ID | Physical-network recovery beyond the user-confirmed app verification |
| Edge | Live `https://crew-upstream-canary.cgoggins.workers.dev`; version `d6780161-12a7-4da3-a606-31984ae095fc` is healthy and accepts scoped string completion IDs; five focused checks and full remote Edge CI passed; prior forged-identity checks returned 401 and public installer returned 404 | Live client revocation |
| Scaffold | Fresh `9c2268c5` runtime completed native/mobile inference and same-ID Stop with six retained entries; earlier `69f631b6` pause/resume preserved its sandbox through epochs 1–4; final `b9b36ab5` runtime pin is checksum-verified and its real MCP stopped-history reads passed | Physical-device lifecycle scenarios were not separately reported |
| Protocol checks | Device/broker/authority checks 19/19, with failing-before/passing-after resume regression and stale-grant rejection; final remote Worker/provider/vault CI passed including typechecks | Additional interactive client/mobile revocation scenarios |
| Rust checks | Native command recovery 4/4, Scaffold 41/41, coordinator/RPC 2/2 and installer 2/2; actual native managed browser turns and live pause/resume/Stop passed | Additional client/mobile revocation scenarios |
| Mobile | Signed simulator exercised controller/model/sandbox selectors and scoped Stop confirmation, and completed two real continuations; five native isolation/error checks passed; user confirmed physical-device app verification | Physical-network recovery beyond the reported hands-on verification |
| Clean upstream | Unmodified `3b2f7802` executable built in a detached worktree; separate workerd and two clean engines completed remote queued Run, transcript and working-to-idle sync | Mock inference only; no WorkOS credentials were required or borrowed |
| Update rehearsal | Crew patch replayed onto `9609f994`, then actually merged `3b2f7802`; merge `06ac4276703a68db1ae8be50fbce0b568df135ba` is tree-identical to `eccc3324`; merged executable passed real two-engine/workerd queued Run, transcript and working-to-idle sync, headed desktop checks, shared mobile-core/Swift/native iOS builds and simulator cold launch | Final cutover remains separate from this isolated Canary acceptance |

The initial empty canary vault caused the reported `agent_auth_v2_account_unavailable`.
Read-only metadata verified the existing Crew Staging pool has 5 connected OpenAI
and 6 connected Anthropic accounts. Its canonical owner ID differs from the
isolated canary's owner; pointing at the same database would not fix ownership.
No provider credentials have been copied, imported, or connected.

The user-approved shared Agent Auth broker first merged to `master` through
[Platform PR #9429](https://github.com/Ashler-AI/ashler-platform/pull/9429), but
staging candidates compile their selected source without merging `master`.
[PR #9490](https://github.com/Ashler-AI/ashler-platform/pull/9490) now backports it
onto the actual `scaffold-staging` lineage at `b9add0b5ffb7fe0e1dee5f0adfa17821e16db4fb`,
preserving the deployed `f5d9ced3` stable per-pod Services changes. Future staging
candidates must descend from this backport, not the obsolete `ed64ee02` rollout.
All six local security checks and
[remote Worker/provider/vault CI](https://github.com/Ashler-AI/ashler-platform/actions/runs/39287090285)
passed, including remote typechecks. Clean preflight permitted deployment without
D1 or Durable Object migrations; existing secrets were inherited. Verified staging
Worker version is `31695f4e-e8eb-4cd6-8790-802bb0522e96`. All 60 binding fingerprints,
compatibility settings and schedules match the original live baseline.
When using the shared operator secret path, put explicit staging overrides after
`infisical run -- env`: its primary service-origin defaults otherwise override
the caller environment. Verification caught and corrected all three affected URLs.
The running canary returned 11 accounts (6 Anthropic, 5 OpenAI); the private broker
still returned HTTP 403 without its trust key. No provider credentials were copied.

Actual canary API requests returned HTTP 200 and the expected literal replies
from `gpt-6.1-sol` and `claude-opus-5-5`. Account refresh returned all 11 shared
accounts with usage. This proves live shared-pool inference, not sandbox-hosted
execution. Account enrollment/import/revoke remain in Crew Staging; no provider
credentials were copied. Authorities retain the canary-prefixed scope, private
v2 gate and sandbox lifecycle validation.
Live public probes without the broker trust key, and with forged actor headers
but no canary bearer, both returned HTTP 403.

Canary Worker `e3aaaff6-9b3e-43ee-ab13-6249f1674d9d` enables Cloudflare's native
[`global_fetch_strictly_public`](https://developers.cloudflare.com/workers/runtime-apis/fetch/)
flag: Worker-to-Worker owner checks had returned origin HTTP 404 despite direct
authenticated Edge checks succeeding. The original smoke projection now has a
confirmed directory publisher state. Chat2 bootstrap relies on verified grant
ownership instead of opening the obsolete legacy session WebSocket.

The fresh lifecycle smoke reached E2B VM creation, then failed in
`wait_command_channel`: `[permission_denied] HTTP 403` for 120 seconds. Its
native attachment outcome was reconciled to rejected without replay. No E2B
access-policy change or replacement sandbox was attempted after that failure.
The provider's own non-resuming SDK lookup subsequently returned
`SandboxNotFoundError` for that owned VM, confirming failed-start cleanup.
The canary consumer's earlier full
[remote Worker/provider/vault CI](https://github.com/Ashler-AI/ashler-platform/actions/runs/38933796743)
also passed.
Final controller source `9a12de49fc4` passed
[remote Worker/provider/vault CI](https://github.com/Ashler-AI/ashler-platform/actions/runs/38946679029),
including all remote typechecks.

The isolated provider image was remotely built from the current provider's exact
source plus its two-line canary link-scheme fix, then deployed only to the canary
release as digest `sha256:01f6d0e611c4fd94414b5c1a0fe9512d30c6fcc406e2af1d67c3bdfc09e5f5cc`.
[Cloud Build](https://console.cloud.google.com/cloud-build/builds/5aa53535-4093-42f4-83bf-68f686135f95?project=54440053364)
passed; the old provider rejected `crew-upstream-canary` before every directory
operation. Three HTTP directory tests passed; disposable PostgreSQL integration
was skipped because no test database was configured. Shared provider deployments
and database permissions were not changed.

The user approved provisioning and installing the isolated canary on the paired
iPhone 18 Pro (iOS 27.0.1). Xcode 26.6 (`17F113`) cannot mount its current
developer image: `0xe800010f`, missing hardware variant (board 10, chip 33120).
Apple's documented `xcodebuild -runFirstLaunch -checkForNewerComponents` returned
`No new updates for 17F113`. A compatible developer image/toolchain remains a
prerequisite for on-device launch; no pairing or device security settings changed.
The device build also failed automatic provisioning: Apple reported no registered
team devices and no development profiles for the canary app/widget. Its produced
app is unsigned and was not installed. Simulator success is a separate result.

Canary continuation links use the exact configured project/deployment and can
navigate ready, paused and stopped sessions without starting their host. Browser
link generation and redirects were exercised with a provider transport that
fails on execution. Existing legacy deployments retain their link policy.

The desktop's native warm URL dispatch and cold launch with the synthetic
directory link preserved the exact signed-out intent; IPC `WatchChats` and
`WatchSessions` remained empty. Launch Services preserved installed production
and staging handlers; the canary did not register upstream's `zeron:` scheme.
On 2026-10-17, two independently authenticated desktop clients returned the same
18 conversation IDs and resolved sandbox `rcs_45db1f808bb3cb58aa18ec6b` to
conversation `6ac457b2-f8d3-4cf2-921e-404d14c83dc2`. Clicking the real Chrome
`Open in Crew` link and accepting its one-time app-open prompt displayed that
conversation and retained transcript in Crew Canary. The same sandbox remained
stopped at epoch 4; no resume, replacement or command was admitted.
Both clients read the annotation collection successfully (empty). The user
approved adding/removing a test note, but the API supports only create/edit/resolve;
no note was created. A retained/resolved-note check still requires that approval.

iOS Sign In initially failed because Scaffold accepted only HTTP loopback OAuth
callbacks. Its shared register/authorize/token validator now also accepts exactly
the deployment-configured `crew-upstream-canary://callback`; other schemes and
callback suffixes remain rejected. Isolated Worker
`c198a3fc-321b-4063-8b72-dde6dc8fe53d` returned live HTTP 201 for the native
callback and HTTP 400 for staging, query-suffixed and external HTTPS callbacks.
Nine local OAuth/auth-session checks passed. The user approved native app/website
consent, but Google's required cross-device passkey flow did not complete in Simulator.
The native consent prompt exposed upstream's internal product name; setting
`CFBundleName` to `Crew Canary` fixes that without renaming the executable/module.
The isolated simulator app rebuilt and reinstalled successfully, and fresh native
UI evidence shows `Crew Canary` requesting the exact canary authority. Physical
installation remains blocked above.
Platform fix `c45b67e6c5a` is pushed; its
[remote Worker/provider/vault checks](https://github.com/Ashler-AI/ashler-platform/actions/runs/39049389627)
passed, including remote typechecks and supervised-runtime suites. No shared
deployment or production release was changed.

The user explicitly approved importing only the existing canary **Scaffold**
session into the isolated simulator. The Debug-simulator-only
`-import-canary-session` hook consumes a mode-0600, bounded regular file before
calling the existing verified restoration path. The original desktop credential
remained byte-identical; transfer files were deleted, and no Google/provider
credentials were copied or logged. Both clients share the credential's revocation;
this is authenticated mobile testing, not proof of independent Google sign-in.

The first real import exposed the unsigned simulator build's missing Keychain
identity (securityd OSStatus `-34018`). Xcode's local simulator signing supplies
the application identifier; no plaintext fallback, device provisioning or account
permission changes were made. All three `CrewIsolationTests` passed. The signed
app displayed `czhen@ashler.ai · crew-upstream-canary` and both desktop hosts.
Wrong-project import failed closed and consumed its file; normal cold launch then
restored the previously saved account from Keychain. Warm directory and cold
Scaffold links, plus background/foreground recovery, displayed all six retained
entries including `CREW_NATIVE_CANARY_OK` and `CREW_NATIVE_RESUME_OK`. The existing
sandbox remained stopped at epoch 4; no replacement or execution was admitted.

Signed-in mobile controller reads exposed a real upstream-port regression: the
Edge allowlist rejected upstream's empty `echo` liveness frame and closed every
device link before RPC readiness. The relay now admits only the empty, canonical
probe, retaining authentication, session-target validation and RPC restrictions.
A real workerd regression failed before the fix and passed afterward; five
relay/nudge checks and fourteen authority/peer checks passed. Isolated Edge
`9e476e0a-393f-4021-afd3-c54f55b0f563` is deployed and healthy. The simulator then
listed canary sandboxes through the desktop controller and selected the original
stopped sandbox/session IDs without executing a lifecycle command.
After the scoped reconnect, the rebuilt simulator also completed a fresh read
through the other desktop controller. A successful list cleared its prior
selection error and preserved the canonical sandbox/session IDs.

The rebuilt simulator presents native error messages without reflected
`Zeron.CoreError` names. All four `CrewIsolationTests` passed. An integrated
build initially failed compiling host-side `ring` because Xcode's PATH selected
raw clang without an SDK; prioritizing Apple's `/usr/bin` compiler shims fixed
the build and regenerated the bindings through the normal build phase.

Recovery was exercised without changing host-wide networking: the debugger
called the existing native reachability API and interrupted 35 TCP sockets in
only the canary simulator process. The app displayed “Offline — sends are saved”
while retaining both completion markers. Restoring reachability removed the
banner and preserved the transcript; the debugger was detached. This is a
process-scoped reachability/socket-interruption simulation, not a physical
airplane-mode test or a claim that all subsequent network traffic was blocked.
No message, annotation or sandbox lifecycle operation was submitted.

The configured Linux x64 pin is now the `b9b36ab5dff291871d6977dcfff8d9128b48fbae`
private artifact under `~/.crew-upstream-canary/runtime/`, with verified executable
SHA-256 `f48ed03ae72f23f6301879309dccff7f33d89bb726c3ed6d2690641a3c3bce26`.
The fresh native/mobile inference and final Stop smoke used `9c2268c5`; the
final `b9b36ab5` delta changes only read-only MCP caller routing, exercised on
the same stopped history without creating another sandbox.
The earlier live native-browser and same-ID pause/resume inference evidence used
`69f631b6c72c55c171f2e74d76121a0814990830`; Stop retained that synchronized history.

Launches from another Crew agent must pin Canary's IPC/data coordinates as shown
in `dist/README.md`; otherwise the parent's `COMET_IPC_PORT` can select its engine.
The local instance listens on 27954 and reports the user's verified `signedIn`
canary profile. Existing sessions survive restart; no credentials were borrowed
from the parent. The demo uses a fresh owned temp directory and seeds each
project once, fixing its observed `no such space` failure.

Managed model pickers use existing bundled OMP and upstream Claude/Codex
catalogs without invoking local CLIs or reading provider credentials. Agent Auth
still validates actual account/model access at run admission. Managed title
inference uses the controller's title route; realtime voice and unsupported
harnesses remain explicitly unavailable, never local-provider fallbacks.

Pre-runtime admission failures (including Agent Auth authority errors) retain an
assistant error entry and an errored session status, while the command remains
rejected. A saved user message alone does not imply inference started. After the
underlying service is restored, explicitly recover a confirmed rejected command
with a fresh command ID and the original Run payload/message ID; the user entry
is deduplicated. Do not automatically retry, replace the message ID, or use
RetryDelivery for an already-saved message (it only reissues undelivered entries).
Crash recovery reads pending commands and in-flight claims under the same
existing execution mutex, released before asynchronous dispatch. A concurrent
control drain must not overwrite a terminal outcome or its admission reason
using a stale pending snapshot after the executing claim retires.
The admission/recovery and existing startup-failure regressions passed. The actual
offline headed demo displayed the durable error and **Run failed** state. Reported
chat `379bf65b-4542-4630-be24-a36051dcb913` recovered through an explicit new command:
its original user entry remains singular, the old command remains rejected, and
real Agent Auth inference produced assistant text and tool calls.
That response later hit `ECONNRESET`: GKE logs confirmed a 30-second backend
deadline truncating HTTP 200 streams. Platform `ecab2c2e286` enables the existing
streaming-only backend for Canary's three inference endpoints (3600 seconds),
leaving ordinary API deadlines and authentication unchanged. A real 1200-line
managed response then completed with its terminal marker and no errors over
111 seconds from its first assistant entry. The earlier partial user response
was retained, not automatically replayed; the user can explicitly continue it.

Native startup now rejects failed sandbox bootstrap before creating a local
profile, device identity or IPC listener. Previously, a delayed binary install
could exhaust the initial grant and silently start a local-profile host; that
host then prevented the fresh Attach grant from taking effect. Native handoff
attaches before waiting for host readiness. The actual built CLI rejected an
invalid bootstrap with exit 1 and created neither a local profile nor identity.
The reported session `834ecf25-ee4b-42fe-8584-5f7e42179df0` was recovered in place,
without creating a replacement sandbox or replaying its prompt.

Scoped peer readiness uses `LocalDevice`, not the host-wide `ListHarnesses`
probe. Attachment chunks/commit carry the destination chat ID and require its
existing `session.files` capability at Edge and the native host. Missing or
mismatched session IDs remain denied; this does not grant host-wide file access.
Queued attachment dispatch resolves the current session host on each attempt,
so pending delivery follows a resumed host without creating another Run.
The integrated desktop build, 28 Rust transport/attachment/client checks, three
composer-recovery checks, 13 Edge authority checks and nine browser checks passed.
The transport suite initially failed because its relay fixtures had no explicit
dev authority; fixtures now opt in locally, while production remains fail-closed.
One existing transport check is ignored. Local typechecks were not run.
The first image sandbox auto-paused while CI ran. Inspect confirmed that state;
same-ID Resume reached epoch 2. Its original command was rejected because bytes
never arrived, so one explicit RetryDelivery recovered the original message with
a fresh command. The transcript contains one user entry and one exact image-text
response; no replacement sandbox or duplicate prompt was created.

The copy-only importer passed committed-legacy schema fixtures through the real
target SQLite/Loro readers, including transcript parts, peer provenance,
annotation revisions, native OMP bytes, attachments and durable command outcomes.
Unsupported shared/granted/native records and unreconciled work explicitly block
cutover rather than being silently dropped. Existing web/legacy clients stay on
their old isolated lane.
No live user profile has been imported.

The rehearsal's three conflicts were the Cursor launch boundary and the macOS
development plist/launcher. Resolution keeps both the managed-route guard and
upstream's Plus model defaults, and keeps Crew's isolated environment names.
Parents are `7142b2c533bfc1e960abbd901238ff8845f15c24` (Crew on prior upstream)
and `3b2f78027a2c31622c39c35716da4d6e1af8de15`; resulting tree is
`fe83c31e96e6e325b9580d44146b7f24d6962b00`. This is not an empty merge.

The isolated Canary delivery checklist is complete, including retained annotation
write/sync and the user's physical-iOS app verification. This is not full release
qualification: the remaining-acceptance matrix and minimum release evidence below
still apply. The standalone independent Google OAuth check is waived only for
isolated dogfooding. No production promotion or live-profile migration occurred.
Local typechecks are intentionally not run to preserve workstation resources.
Remaining gates below are requirements, not claims of completed testing.


### Scoped sidebar links and staged source delivery (0.2.114)

The selected Scaffold session exposes **Web** and **Tilt** in session details
from its canonical environment links. Owner, project, deployment and session
must match the chat; each missing or unsafe URL hides independently. Activation
rechecks the current selection. Local/Devbox rows do not expose these actions.
An owned headed fixture exercised both native actions against loopback pages;
a separately initialized local-session window hid both. The focused scope/URL
regression and native browser contracts passed; no sandbox was allocated.

Release adapters preserve canonical signed artifacts and immutable GCS publication.
Build-only reviewed source cannot publish; GCS candidates require merged-main
ancestry. Shared Edge deployment is manual-only, and immutable candidate
publication does not move channels or activate a controller. Fresh staging uses
its own `~/.crew-staging` profile and IPC 27957, independently authenticated.
Legacy mixed/unscoped deployments, pending work, unsupported consumer access and
unqualified real-profile import memory remain cutover blockers, not permission
to relabel profiles or retire other owners' records. Production remains gated
on the captain's installed-app restart and manual acceptance.

Copied-database inspection now completes without dataset-wide history retention.
The actual public API inspected 3,037 snapshots (715,402,552 encoded bytes) in
95.83 s at 254.94 MiB lifetime peak footprint and preserved the database hash.
The same shallow workspace that exceeded 1 GiB now completes at 41.61 MiB;
full-corpus timing used a 120 s watchdog, not the earlier 15 s diagnostic window.
Six native codec, 23 migration-library, 32 backup/import and one portable runtime
fixture checks passed locally. Shallow history still yields no inert-bootstrap
proof; complete authenticated backup/import and current-source remote CI remain
separate cutover gates.

The vendored memory benchmark parser uses comma-delimited number groups rather
than ambiguous nested repetition. Its two CodeQL backtracking findings are fixed;
the actual Deno parser rejected both 100,000-digit malformed log cases in under
2 ms, with valid numeric results unchanged (benchmark subprocess stubbed).

The earlier 0.2.114 integration source is merged; these follow-on repairs require
current-source CI. Canonical staging activation follows the gate below.
No production cutover is authorized.

### Canonical staging native runtime (0.2.115)

Archive compatibility `scaffold.comet-runtime.v2` packages the native binary with `crew-web` and the
`crew.native.web.v1` contract. Platform installs and verifies the immutable archive;
the controller no longer uploads or reinstalls a caller-selected runtime. Native
DATA is `.scaffold/crew-data`, web assets are `.scaffold/crew/web`, and IPC is 39400.
This archive version is not the authority wire schema: create, inspect and target
proof retain Platform's independently versioned `scaffold.comet-runtime.v1` envelope.
Attachment reuses matching live authority or makes one supervised launch, then
confirms owner, scope, sandbox, device, epoch and capabilities. Every outcome after
bootstrap upload is attempted gets one independent, bounded cleanup attempt;
transport failure can prevent removal, without replacing the original result.

Canonical staging retains the exact deployed ae2 legacy classes, WASM, raw bindings
and alarm implementation. New native classes use separate namespaces and the
`native-v2/` blob prefix. A newer verified native admission fences older credentials
and connected readers/writers; it is not a separate stop/pause revocation feed or
retroactive cancellation of already-authorized operations.

Candidate and forward-retained rollback artifacts bind the complete staging config
and exact legacy bytes. Rollback is a forward deployment retaining both generations,
not a Cloudflare version rollback across new Durable Object classes. Publish only a
merged-main `release.yml` `desktop-and-scaffold` / `staging-candidate` tuple first;
this does not move release channels. A fresh independently signed-in controller uses
its own DATA and IPC 27957. Hold the single `deploy.yml` staging switch for matched
Platform fresh-image readiness and the verified rollback pair. No profile import,
old-data migration, production deployment or independent channel movement is allowed.

## Decision

Build the next Crew integration on current `zeronsh/zeron`, retaining upstream's engine, sync, persistence, and UI architecture. Port the Ashler-specific behavior at its boundaries instead of replaying the entire divergent Crew history.

The deliverable is not just a working port: it must also survive an actual upstream-update rehearsal before cutover. Keep the existing Crew branch, installations, active sessions, and data intact throughout development.

### Pinned starting points

- Upstream: [`3b2f78027a2c31622c39c35716da4d6e1af8de15`](https://github.com/zeronsh/zeron/commit/3b2f78027a2c31622c39c35716da4d6e1af8de15), version `0.2.113`. The remote `HEAD` and `main` matched this commit when checked for this plan. Recheck at implementation start; do not build against an unrecorded moving branch.
- Crew: `297717c55beab9323c53095f3f5f683f00676a5e`, version `0.1.142`, plus separately reviewed working-tree changes. The current checkout has uncommitted changes; they are not disposable or automatically part of the committed port source.
- Use `https://github.com/zeronsh/zeron.git` as the upstream source. The existing Crew upstream remote still names the older `zeronsh/comet` URL.

## Scope and boundaries

| Area | Proposed treatment | Boundary to keep small |
| --- | --- | --- |
| Crew/Scaffold login | Port resource-bound OAuth, scoped device grants, revocation, and profile isolation | Engine auth/profile construction and Edge authorization |
| Agent Auth | Port the existing authority client and streaming relay; retain server-side account selection | Host-owned run context, harness launch configuration, account settings |
| Scaffold sandboxes | Port list/inspect/create/pause/resume/stop plus authenticated attach and recovery | Scaffold client/runtime, typed RPC operations, environment controls |
| Comments and settings | Reuse upstream equivalents; port only missing behavior | Existing settings pages, comment controls, small state/RPC additions |
| OMP | Port the native harness and ownership/session behavior required by current Scaffold execution | Existing `Harness` interface and native session/launch services |
| Sync and persistence | Adopt upstream registry/chat transport, budgets, async persistence, and rendering | No transplant of Crew's old workspace synchronization stack |
| Mobile | Adopt upstream's current iOS app, shared Rust client, and UniFFI bridge; port Crew-specific behavior onto them | Shared client auth/scope, native URL handling, scoped host operations, iOS packaging |
| Branding and distribution | Keep Crew identity, signing, staging/production channels, deep links, and required runtime contracts | Packaging and user-visible strings; retain upstream internal crate/path names |

Delivery includes desktop, the headless Scaffold host, and upstream-based iOS. Desktop/headless integration can be developed first, but mobile is an explicit workstream, not merely a compatibility check or an optional follow-up. Web compatibility remains a cutover gate. This does not authorize dropping existing sessions, annotations, mobile access, or web access: migrate retained consumers and data, keeping any temporary legacy lane isolated until coordinated cutover.

Namespace integration and unrelated custom features are not dependencies of the four requested features. Do not automatically copy them. Any retirement of existing behavior must be an explicit release-scope decision, not an accidental consequence of the port.

### Cross-client session and deep-link requirements

- The same authenticated user must see their synchronized sessions across Crew desktop and iOS clients, including sessions hosted on another desktop or in Scaffold. The registry is per-user, not per-device. Preserve project/deployment isolation and explicit grants; this does not make other users' private sessions visible.
- Creation/import/attachment must publish a stable owner-visible session reference through the authorized controller/control plane. Do not rely on a restricted sandbox token gaining access to the whole user registry. Reconcile owned session references so discovery does not depend on which client originally opened the session.
- Distinguish visibility from execution availability. A disconnected host must not remove its sessions from the index; already-synchronized history remains browseable subject to authorization. Continuing execution requires the relevant host/sandbox to be reachable. Signed-out/local-only sessions require explicit import before other clients can discover them.
- Existing **Continue in Crew / Open in Crew** buttons must open the same session in the installed desktop or iOS client, including cold launch and a signed-out launch that completes login before navigation. Preserve existing production/staging URL schemes, session-directory and Scaffold routes, IDs, and scope; links contain no credentials and grant no access by themselves.
- Opening a link is not a transfer of execution or a new-session request. Preserve the current distinction between mobile browse-only opening and an explicit send/attach/resume action; any change to that behavior must be deliberate.

Acceptance: sign the same user into two desktops and an iPhone; create sessions on each execution host and in Scaffold; verify matching stable session references and history after sync/reconnect. Open both existing link forms on cold/warm desktop and iOS, before/after login, and verify the same session with no duplicate creation or unintended execution. Check offline-host display, account/project isolation, denied access, and staging/production separation.

## 1. Crew/Scaffold login

### Reuse and port

Upstream already has browser/loopback and pasted-code login UI, auth state handling, and profile-scoped stores. Its implementation is WorkOS-specific, including organization selection, refresh tokens, and JWT aging. Crew's Scaffold login has a different resource/token contract; replacing URLs is insufficient.

- Keep upstream's auth/UI flow boundaries and immutable profile selection. Add an explicit Scaffold auth implementation/selection at initialization, rather than replacing the whole upstream auth module or introducing a generic authentication plugin framework.
- Port Crew's OAuth discovery checks, same-origin endpoint validation, PKCE S256, resource binding, callback state/expiry checks, headless completion, and persisted session handling.
- Preserve the distinction between the current user-bearer contract and expiring device grants. Do not apply WorkOS refresh-token semantics to Scaffold credentials.
- Bind profiles to verified identity plus project/deployment scope. Do not equate a client-supplied project string with a verified WorkOS organization. Switching identity/scope opens the correct profile rather than retargeting an already-open database.
- Port scoped grant issuance/rotation and the LocalController versus ScaffoldHost restrictions. A sandbox host must not gain account import, arbitrary local-device execution, or recursive sandbox creation permissions.

### Edge is part of this feature

Upstream's registry is per-user, and its chat rooms authorize a user owner. Crew additionally has project/deployment/session/device/capability grants. Preserve these distinctions at every entry point: WebSocket, HTTP append/checkpoint, relay RPC, uploads/downloads, and attach.

- Verify Scaffold authority server-side and pass only trusted identity/scope metadata to rooms. Strip caller-supplied authority headers.
- A grant for one sandbox/session must not become unrestricted authority for the user's registry, other chats, or device filesystem RPCs.
- Keep upstream's opaque chat transport. Opaque blobs cannot enforce semantic field-level permissions: restricted callers must use authorized command operations, not receive arbitrary document-write access.
- Keep explicit grants separate from the personal registry. Supporting a granted session does not require reviving a project-global shared workspace CRDT.
- Reject expired/revoked/wrong-scope authority, including on established connections. Distinguish authentication failure from control-plane unavailability; a transient outage must not erase valid local history or credentials.

Acceptance: browser and headless login; restart persistence; sign-out and identity/scope switch; callback replay rejection; grant refresh/revocation; negative cross-project, cross-session, and restricted-device access checks over both HTTP and WebSocket.

## 2. Route inference through Agent Auth

“API requests” here means model/inference traffic, including auxiliary model calls. Scaffold lifecycle and sync requests retain their own control-plane authentication; this is not a proxy for all application network traffic.

### Concrete port

- Reuse the existing Agent Auth v2 authority exchange, `/responses` and `/messages` streaming relay, account list/import/revoke operations, and route receipt handling.
- Add Crew's host-only `RunContext`/`InferenceRoute` to upstream `RunControls`. Do not put authority secrets in serialized `RunRequest`, chat documents, prompts, logs, or synced settings.
- Preserve upstream's newer execution lease, turn control, cancellation, and runtime-reconfiguration behavior when extending `RunControls`; do not copy the older struct over it.
- Adapt the existing Claude and Codex launch overrides and native OMP provider integration. OMP is a separate harness, not an executable rename of upstream's Pi driver.
- Maintain session/owner/provider/lifecycle-epoch binding, token redaction, inherited-credential removal, streaming cancellation, and header sanitization. The child receives an opaque local token; real shared provider credentials remain in Agent Auth.
- Reuse Agent Auth's account selection and usage routing. Do not add a second client-side account scheduler or rotation policy.
- Present shared accounts and opaque account choices through upstream's account settings. Preserve explicit account import/revoke consent and errors; do not silently switch local CLI credentials.

### Coverage policy

OMP, Claude, and Codex are the initial managed routes to port from current Crew. Inventory every other enabled harness and auxiliary inference path: titles, reviews/subagents, model probes that perform inference, and voice/realtime features.

In Crew-managed execution, each enabled path must use Agent Auth or be visibly unavailable until it has a compatible adapter. No silent fallback to inherited provider credentials. An explicitly selected user-owned/local-provider mode remains separate and labeled; signing out of Crew must not silently select it. Existing sessions using unsupported routes need an explicit migration disposition.

Managed OMP pins `PI_SMOL_MODEL`, `PI_SLOW_MODEL`, and `PI_PLAN_MODEL` to the parent’s qualified Crew model for that process only; saved preferences and explicit agent overrides are unchanged. The inherited provider hook still rejects unmanaged inference. Crew’s OpenAI models declare `supportsStrictMode: true` so OMP preserves the task tool’s explicit `strict: false`; otherwise Responses can require optional seance-only fields and reject ordinary scouts before launch.

Native OMP receives only its run-scoped `OMP_AUTH_GATEWAY_URL`/`OMP_AUTH_GATEWAY_TOKEN`; the Codex/legacy `COMET_INFERENCE_TOKEN` alias is removed after common context setup. This prevents an installed legacy global loader from contacting its localhost fallback before OMP becomes ready. An isolated run through the actual harness and installed OMP completed in 2.354 s with that loader present, one catalog request, one inference request, and no request outside the assigned loopback gateway. No live Agent Auth or installed-profile activation is implied.

Run `node scripts/omp-gateway-smoke.mjs /path/to/omp` for the installed-runtime regression: an isolated profile and loopback inference exercise the actual advertised schema, direct scout launch, managed role selection, and completed child result. This is not a live Agent Auth receipt check. Failed tool cards retain the existing 160-character diagnostic summary plus a truncation marker when needed; successful output and full diagnostics remain journal-only, with no historical backfill.

Acceptance: real streamed runs for both provider protocols; observed Agent Auth route/account receipts; tool continuation and cancellation; authority refresh; expired or wrong-owner route rejection; no duplicate run after reconnect; no provider-secret persistence; auxiliary calls cannot bypass the managed route. Do not replay an inference POST after an ambiguous disconnect unless the server's idempotency contract explicitly permits it.

## 3. Scaffold sandbox lifecycle

Port the existing `ScaffoldClient` and the lifecycle subset of `ScaffoldRuntime`, not the entire current sessions/composer implementation. Keep Platform authoritative for sandbox state and identifiers; the desktop caches observations and submits operations.

| User action | Required behavior |
| --- | --- |
| Start new sandbox | Explicit create with project/deployment, source ref, region, database mode, runtime version, and model/account route; persist the returned sandbox/session binding |
| Pause sandbox | Pause that sandbox, retaining identity and reporting the observed result; do not describe this as stopping a single agent turn |
| Resume sandbox | Resume the same paused sandbox, validate unchanged identity/scope, refresh authority/attachment, and reconnect |
| Stop sandbox | Stop that sandbox, retire its pending execution/recovery work, and surface its terminal state; never auto-create a replacement |
| Attach/reconnect | Inspect and attach to the preserved ID; current attach-to-paused behavior resumes the same sandbox and must be clearly conveyed in the UI |
| Stop agent turn | Use upstream's turn control without pretending the sandbox was paused/stopped or killing unrelated background work |

Expose these as typed engine operations used by both UI and CLI. UI buttons display pending, confirmed, failed, or outcome-unknown state; an accepted HTTP request is not proof that the sandbox is ready. Reconcile via authoritative inspect/list after an operation, reconnect, or engine restart; retain current event-driven refresh rather than adding an arbitrary polling framework.

### Reliability behavior to preserve

- Stable sandbox/session IDs and immutable caller command IDs. Lost acknowledgements must not turn retries into new commands or new sandboxes.
- Persist enough operation identity/outcome state to recover after restart. Reuse upstream's durable command/store mechanisms where equivalent; add only missing scoped outcome records, not a parallel sync engine.
- Keep accepted/delivery-pending, rejected, and unknown distinct. Terminal outcomes are never replayed. Snapshot replacement cannot silently erase an outstanding semantic command or its conflict.
- Verify Platform's actual create-idempotency and lifecycle contracts before enabling automatic retries. If creation outcome is unknown, inspect/reconcile; do not issue another create on the assumption that the first failed.
- Preserve lifecycle epochs, native OMP session/history/cwd, writer ownership, and exact child-process identity. Reject stale recovery after Stop. A restarted controller must attach to the existing execution, not start a duplicate writer.
- Retain the required native CLI/environment interface, including `COMET_EXECUTABLE`, `COMET_SESSION_ID`, and `COMET_IPC_PORT`. Preserve the checked-in native handoff contract where consumed; do not introduce a second handoff protocol as part of this port.
- `comet session supervise` uses the host-minted `COMET_SUPERVISION_TOKEN` with `COMET_SESSION_ID` over local IPC. Its single JSON receipt identifies the retained exact run. Keep the child attached; terminate/reap it to release. The checked subscription blocks idle/update retirement only: cancellation, stream death and engine shutdown still end it, and the last release restores the original idle deadline. Old tokens or old subscription drops cannot retain/release a replacement run. Tokens stay out of serialized documents and debug output.
- Legacy cutover recovery is **interrupted-request continuation, not live-process adoption**. The importer retains and revalidates the exact `PendingRun`/`.recovery` bytes, source identity, native history and final journal evidence. Only an unblocked pending request with a later known `Done: interrupted` outcome can become a recovery intent; completed, errored, missing/unknown outcomes and mismatched authority remain gated. A `writerStopped` field is not OS liveness proof: the operator/local stop boundary or cloud source fence must establish that the old writer really stopped before export and activation.
- After attachment aliases, repositories and turn hooks are wired, the engine binds each imported intent to the freshly verified controller owner/project/deployment and destination device, checks the original native ID/cwd/model/account and user entry, then admits its exact request through the existing immutable native command ID. Original attachments, approval settings and user-message identity are retained; account selection maps to the existing `crewAccountId` route without selecting a default. Archived native bytes stay immutable; execution uses the existing `native-resumes` working copy. Existing commands and unknown processed claims are never re-admitted, and these chats never use generic last-user-prompt reconstruction. A new execution receives a new supervision token; the old subscription does not survive engine exit.
- Server-proven extinct original epochs retain `crew.originalEpochHistory` and their original native ID as provenance, not execution authority. Runtime admission loads the importer's hash-verified history map; editing or dropping index metadata cannot unlock it. Run, steering, idle startup, native/side-chat fork, handoff retries and imported recovery share the execution fence. An explicit verified Start may release only its exact new sandbox/epoch/device through the immutable `original-epoch-start:<chat>:<originalEpoch>:<newEpoch>` receipt and reset native association to the empty tombstone. Generic refresh, a larger client-projected epoch, unknown command outcomes and the original native ID never grant execution. Preserved transcript reads remain available; original pending requests are inert, not replayed as new-epoch work.
- Focused admission regression (not run during implementation): `cargo test --locked -p zeron-engine original_epoch_requires_trusted_start_and_never_reuses_old_native_identity`. It rejects original/raw-native resume and unverified/unknown starts, accepts the exact trusted new-epoch association, and retains the source message ID and receipt.
- A fresh device host fetches `/api/code-sandboxes/:sandbox/original-epoch-history` with its current verified `cs1` grant before engine/document-sync construction, under the profile lock. The authenticated response carries exact original proof JSON bytes and the complete original command-status map. Shared validation preserves the raw receipt hash; the host marks every original command processed and commits `original-epoch-bootstrap:<chat>:<originalEpoch>` before admission. No whole-profile restore, manufactured native journal, or client-projected marker is an authority substitute. A missing/incomplete proof fails closed; an authenticated explicit null applies only to sessions without original terminal migration.
- Terminal original-epoch transcript rows retain legacy `queued` and `steered` delivery statuses through snapshot/JSON round trips. These are historical message metadata, not live queue entries, streaming state, execution claims or recovery instructions. Desktop/mobile render them as non-streaming history; importing them never creates a command or queued message. The importer’s terminal-scope restrictions and runtime original-epoch execution fence remain authoritative.

Acceptance: start → stream → pause → resume same ID → continue → stop; repeated actions; lost HTTP response; controller restart during creation and attachment; expired grants; terminal sandbox reconnect; stale lifecycle completion after Stop; native history continuity and absence of duplicate execution.

## 4. UI portability inventory

| Feature | What exists upstream | Port decision |
| --- | --- | --- |
| File/diff comments | `comments.rs` and `comment_ui.rs` already implement editable review comments folded into the next prompt | Reuse them; no second comment system for equivalent behavior |
| Comment on selected transcript text/message | Current Crew can add selected context plus a note to the next prompt | Port the missing selection/action behavior through upstream's composer/comment conventions after checking the actual surface |
| Persistent shared annotations | Crew adds durable create/edit/resolve, semantic anchors, and re-anchored/orphaned states | Separate data/authorization work, not a cosmetic UI port. Inventory existing use and migrate these records; implement retained behavior over scoped commands without importing the project workspace stack |
| Crew login and shared accounts | Upstream has login/account settings but different authorities | Reuse the pages with the new auth/account operations and accurate shared-versus-local labels |
| Scaffold controls | Upstream device/session UI is not Scaffold lifecycle management | Add environment status, source/model/account selection, start/pause/resume/stop, and actionable recovery state using the engine operations above |
| Appearance, composer, notifications, shortcuts, files, worktrees | Upstream already has dedicated settings | Keep upstream behavior; port only a demonstrated missing Crew preference and migrate its stored value |
| macOS permissions | Upstream Appshots already exposes permission status/actions | Reuse that UI; evaluate Crew's process/executable-specific diagnostics as an incremental addition, not another permissions page |
| Crew updates and agent updates | Both implementations contain update-related UI | Preserve signed Crew channels, manual check/restart, and any genuinely missing preference; retain upstream downloader improvements |
| Session links/IDs | Crew links carry deployment-specific session-directory or Scaffold attachment semantics | Preserve existing desktop/iOS URL routes, authenticated navigation, and exact session identity; never substitute session creation |

Use side-by-side feature acceptance, not pixel parity. A ported control must be exercised in the actual headed app, including focus/keyboard behavior and error states. Do not copy whole `shell.rs`, `composer.rs`, or settings modules to recover a handful of controls.

Full teammate/project collaboration is a separate scope decision from personal cross-device sync and scoped sandbox access. If existing shared annotations or access depend on it, that is a cutover blocker until the supported semantics are implemented or explicitly retired with a data-preservation path. Upstream review comments are not a migration substitute for those records.

### Mobile workstream: adopt upstream iOS and shared Rust client

The initial plan's mobile compatibility gate alone would not have delivered upstream mobile fixes. The target is upstream `apps/ios` plus `crates/client` and `crates/mobile`, not keeping Crew's separate Swift sync implementation and patching only the desktop.

- Retain upstream's shared registry/chat2 transports, reconnect/backoff wakeups, network-path and foreground recovery, background persistence, and incremental transcript/layout implementation. This inherits the implementation changes; performance and reliability still require verification in the Crew build.
- Port Scaffold authentication and verified scope into the shared client and UniFFI boundary, not only `crates/engine/src/auth.rs`. Upstream mobile currently assumes WorkOS credentials and organization IDs; it also has product-specific endpoints and callback identity that must not remain in a Crew release.
- Port Crew's URL handlers, login continuation, keychain/profile isolation, native bundle/channel identity, and required session/Scaffold controls onto the upstream shell. Share protocol/auth rules with desktop where practical rather than duplicating a new sync stack in Swift.
- Keep iOS an engine-free client: commands go to the authorized execution host, whose inference uses Agent Auth. Do not place shared provider credentials or a local agent runtime on the phone. Preserve and verify the existing desktop-controller dependency where required; removing it is a separate capability change, not an implied benefit of the UI rewrite.
- Include iOS cache/settings/credential migration and preserved session membership in the data plan. Do not delete the old app's local state before pending work and cached-only records are reconciled.

Mobile acceptance: the cross-client/link scenario above, signed-in cold start, foreground/background and network recovery, long streaming transcripts, cancellation, send after reconnect, expired/revoked authority, and offline-host behavior. Exercise the actual iOS app on simulator and a supported physical device before release. Future upstream-update rehearsals must include the mobile client and native shell, not just desktop/headless builds.

## 5. Implementation sequence and gates

Each phase should be a reviewable logical change, splitting further only where independently runnable. Preserve the upstream layout; use concrete integration modules in the existing crates. No generic plugin SDK, new gateway service, or parallel persistence layer.

Keep upstream ancestry intact. The eventual reviewed cutover merge must retain both existing Crew history and the upstream-based integration history, with its result deliberately resolved to the validated port tree. Do not squash away upstream ancestry, reset/force-push `main`, or let an automatic merge resurrect retired Crew implementations. Verify the resulting merge tree before building the release candidate.

| Phase | Deliverable | Exit gate |
| --- | --- | --- |
| 0. Baseline and contract inventory | New isolated integration branch/worktree from the pinned upstream SHA; source/consumer inventory; fresh Crew test profile; patch ownership map | Clean upstream executable exercised; current Crew source/data untouched; inventory records all required callers and retained/deferred features |
| 1. Identity and packaging | Crew product/channel identity, Scaffold login in engine and shared client, profile isolation, Edge verifier and restricted grants | Desktop/headless/iOS auth works; cross-scope tests deny access; isolated Edge namespace/profile prevents old/new clients sharing incompatible stores |
| 2. Inference and OMP | Host-only route context, Agent Auth client/relay, OMP adapter, Claude/Codex overrides, shared account UI | Real local managed turns route through Agent Auth; cancellation, token lifecycle, and process ownership checks pass |
| 3. Scaffold vertical slice | Remote runtime packaging, list/create/attach/pause/resume/stop, durable operation recovery, native CLI consumers | Full lifecycle exercised on staging with preserved identity and native history; no duplicate create/execution under fault injection |
| 4. UI, mobile, and migration | Upstream-based iOS, existing deep links, cross-client session discovery, missing portable controls/preferences, retained annotations, data importer, web compatibility | Actual desktop/iOS surfaces verified; cross-client/link and mobile lifecycle checks pass; copied profiles migrate without loss |
| 5. Upstream-update rehearsal | Merge a newer selected upstream commit into the integration branch and rerun desktop, headless, and mobile gates | Core Crew slices and iOS behavior still work; conflict locations are recorded and unnecessary upstream edits removed before release |
| 6. Release and cutover | Reviewed release, coordinated runtime/Edge/client rollout, staged candidate and documented recovery | Staging candidate manually accepted after restart; production uses the exact approved candidate through the established release path |

Phases 2 and the control-plane portion of 3 can proceed independently after phase 1 contracts are fixed. Finish the working login → managed inference → remote lifecycle path before polishing optional UI.

## 6. Data and protocol migration

This is not an in-place binary upgrade over the old store. Current Crew's project workspace/session schema and upstream's per-user registry/chat2 topology differ.

1. Inventory local SQLite/Loro state, chat/native session IDs, attachments, worktrees, model/account selections, sandbox bindings, grants, pending commands/outcomes, settings, and annotations. Include the existing desktop, headless, mobile, web, and handoff consumers.
2. Develop against a fresh, separate profile. Build a one-time versioned import from a consistent backup into the new schema; never point the new runtime at the live old database as an experiment.
3. Preserve stable IDs where required by native sessions and Scaffold. Explicitly map project/deployment/principal ownership into the new profile/registry, deduplicating shared references without changing authorization. Reauthenticate or rotate credentials when their original trust binding cannot be preserved safely.
4. Copy attachments/native histories and retain worktree associations. Reconcile pending commands with their durable outcomes; do not replay unknown operations automatically or flatten semantic annotations into prompt text and call that parity.
5. Use versioned Edge bindings/routes for the new topology. No old/new dual writers to the same logical session. Keep old clients on an isolated legacy lane until compatible clients or an approved retirement path exist.
6. Dry-run representative copied profiles, including shared/granted sessions and interrupted work. Produce a reconciliation report with IDs, content integrity, unsupported records, and outcomes; unexplained loss blocks cutover.
7. At authorized cutover, establish one active writer per migrated session and take a consistent backup. Rollback before new writes can restore the old binary plus old store. After new writes, stopping and reconciling/exporting them is required; restoring a stale backup is not a lossless rollback.

### Staging cloud migration operator

`scripts/migrate-staging-cloud.mjs` is staging-only. It reads the existing user auth
file with `--auth-file` (JSON `accessToken`/`access_token` or a raw token file), or the
existing `SCAFFOLD_ACCESS_TOKEN` environment variable. It never prints credentials or
passes them in process arguments, follows no redirects, and cannot select a production
origin. Run it from the Crew repository:

```sh
node scripts/migrate-staging-cloud.mjs --dry-run --via-staging-bridge --auth-file /private/path/existing-staging-auth.json --report /private/path/cloud-plan.json
node scripts/migrate-staging-cloud.mjs --apply /private/path/cloud-plan.json --prepare-only --via-staging-bridge --auth-file /private/path/existing-staging-auth.json --report /private/path/cloud-prepare.json --platform-checkout /path/to/reviewed-platform --compatibility-receipt /private/path/compatible-services.json
node scripts/migrate-staging-cloud.mjs --recover /private/path/cloud-plan.json --via-staging-bridge --destination-device-id OWNER_CONTROLLER_ID --auth-file /private/path/existing-staging-auth.json --report /private/path/cloud-recovery.json --platform-checkout /path/to/reviewed-platform --compatibility-receipt /private/path/compatible-services.json
```

Keep an operator-owned staging port-forward open in another terminal:
`kubectl --context "$SCAFFOLD_KUBE_CONTEXT" -n scaffold-staging port-forward --address 127.0.0.1 deployment/scaffold-control-plane 18081:8080`.
The existing bridge injects its service credential inside the pod; never extract it.
`--via-staging-bridge` checks the established bridge health contract, then verifies the
same owner and exact staging resource through its upstream owner-auth route. Generic
`/api/sessions` requires that existing service boundary; a valid owner bearer alone
does not replace it. HTTP diagnostics name the safe phase without credentials or URLs.

Use `--prepare-only` while the one-time legacy source exporter is serving. It returns
`prepared:true, complete:false` after source preparation; for a terminal original epoch,
that is frozen source plus server eligibility, **not** native import or publication.
It never calls generation CAS or native activation. Keep those server-owned frozen
source artifacts and claims, switch to the reviewed native Edge deployment only at the
authorized operator gate, then use `--recover` to commit/activate. Repeated recovery
uses the retained immutable source, not a now-unavailable legacy route.

Every report path must be new; files are mode `0600`. Dry-run is the default and uses
only existing routes: `GET /api/code-sandboxes/auth/session` verifies the current owner,
then all pages of `GET /api/sessions?showStopped=true&user=<verified-owner>`, followed by
the explicit `inventoryRecord` projection from `GET /api/sessions/:id/crew-migration`,
and a repeated complete ID listing to detect drift. This phase-one GET reads raw
stored authority without lifecycle normalization, but exposes only selected safe
fields: no arbitrary session properties, credentials, auth routes or prompt content.
Exact stored profile-version/runtime-profile values remain diagnostic, not trust defaults.
The plan names that authenticated owner and includes status counts plus every retained
ID and blocker; unknown or mismatched ownership is not silently filtered.
It does not request live reconciliation, probe a remote runtime, resume a sandbox,
stop a writer, import files, or change generation. The deterministic version-1 plan
includes every retained record (including stopped and explicitly non-Crew records),
original owner/project/deployment/logical-session/sandbox/device/epoch, the separate
`coordinatorLifecycleEpoch`, generation, record/preservation SHA-256, and blockers. Unknown owner, scope, generation, duplicate
logical identity, incomplete pagination, and unclassified records are hard gates.
An absent generation uses the existing server's **legacy `session`** contract, never
`chat2`. A dry-run candidate means “requires fenced import,” not verified migration.
Operator record/preservation digests cover this safe public projection; they are not
full private-record digests. The server independently compares the complete stored
source record for CAS. Missing `inventoryRecord` fails closed without falling back
to the normalizing session-detail route.

Phase one deploys the compatible Worker with creation frozen, then immediately
captures the pure-read inventory baseline **before** provider changes or migration.
This is a post-Worker, pre-migration baseline—not a reconstruction of old raw DO
records. The compatibility verifier must validate that retained artifact and its
unclaimed/uncommitted state, rejecting unexpected native generations, before the
operator can dispatch a claim. Unknown source metadata remains explicit. No
provider or migration action may fill a missing baseline by normalizing old records.
A missing, failed or truncated baseline is unknown, not evidence that no migration
occurred: retain creation freeze and do not restore the old Worker. Application
rollback must pass its guard before replacing services, and Worker restoration
requires verified health of the restored prior Application. These gates are not a
claim that compatibility deployment or live migration is ready.
The operator's receipt check is defense in depth, not server authority. A per-deployment
DO-backed migration admission barrier defaults closed; only the trusted workflow
opens it after baseline and readiness verification. Direct prepare requests hold an
admission ticket through durable claim persistence. Recovery closes admission and
requires no outstanding tickets before the complete inventory and any old-service
or Worker restoration. Unknown tickets remain blocking; no timeout opens admission.
The owner operator neither opens the barrier nor receives broader permissions.

Apply first re-inventories and requires an exact plan match. It prepares every legacy
candidate before committing any generation. Each mutation carries
`expected: {scope, coordinatorLifecycleEpoch, sourceGeneration: "session"}` to the owner-authenticated
`/api/sessions/:id/crew-migration/{prepare,recover,commit}` APIs. A plan or caller receipt
is not import proof. The coordinator persists a private migration claim before any
provider action; ordinary lifecycle and provider ingress are fenced. The provider uses
the existing authenticated SDK command transport, not the legacy supervisor's exec
channel, to preserve the same sandbox and epoch. The native runtime is installed from
the pinned staging candidate without replacing the old profile. A stopped-writer copy
feeds the existing backup converter and native importer; original SQLite/Loro evidence,
native journals, attachments, configuration, worktree/model bindings and durable
command outcomes remain inventoried. The provider-local remote-context file binds
these files to the server-derived identity; it is **not** authentication or proof that
a writer stopped.

`scope` preserves the exact known-v1 profile's original `targetDeviceId` and
`lifecycleEpoch`; neither is synthesized from the coordinator record. The retained
device must match the same sandbox's proven producer format, `comet-scaffold-{sandboxId}`
or `comet-scaffold-{sandboxId}-e{originalEpoch}`; an arbitrary device is not accepted. A stopped
writer at E1 with a coordinator stop at E2 therefore retains scope E1 and carries
`coordinatorLifecycleEpoch: 2`; an active E1 writer carries 1/1. Claims, recovery,
CAS and ingress fencing check the coordinator epoch. Source capture and native
readback check the original scope. The separate coordinator field is mandatory
through terminal authority, native proof and history marker; missing/stale fields
fail closed, with no old-receipt fallback. This identity split does not admit an
observed-revocation terminal path or relax the existing terminal evidence gates.

Provider `prepare`/`verify` are service-only and accept the exact private coordinator
claim, independently checked against readiness. Durable ingress leases cover all
replicas, sockets, and detached SDK operations. A missing release is unknown and never
expires into a fictitious stopped-writer proof. Drain leases before copying; stop
legacy watchdog/supervisor execution and its writer descendants. SQLite source capture
uses a read-only backup, not a checkpoint or rewrite of the original. Compatibility
rollout must also drain old, pre-lease provider revisions before preparing any source.

Verification reads actual destination storage and retained source bytes, compares
semantic native snapshots, file/native-journal hashes, counts, and durable outcomes.
The coordinator fetches this proof through its private provider path and compares the
unchanged source record before generation CAS. Pending/unknown execution stays intact
and cannot activate or replay. Metadata replication intents may be retained or
compacted only when the authoritative snapshot proves the same effect. New generation
does not authorize a second writer; keep admission fenced through same-epoch native
activation and retain the committed proof. Legacy Durable Object data remains offline
backup, not an active parallel runtime.
Inactive records retain their original status and epoch: do not start a writer merely
to finish migration. Their imported bytes must be persisted in the original retained
sandbox/snapshot used by a later explicit resume; an unbound copy is not sufficient.
The guest-copy path rejects paused/stopped records before SDK connect: auto-waking
the VM could resume unknown legacy work before its writer is fenced. Resumable paused
sources require a preserving offline filesystem or pre-resume writer-fence path; they
remain explicit inventory blockers, not successful imports. Proven extinct original
epochs use the separate owner-profile preparation below, never a replacement guest.
The pinned E2B SDK is `2.50.0`. Its file/download operations use the running guest;
the official [persistence contract](https://docs.e2b.dev/sandbox/persistence.md) says
`connect` resumes paused processes and killed sandboxes cannot resume. The
[snapshot API](https://docs.e2b.dev/sandbox/snapshots.md) captures a running source and
spawns new sandbox IDs, not an offline writable view of the original. A
[reboot resume](https://docs.e2b.dev/sandbox/resume-without-memory.md) explicitly loses
unflushed writes. None is a safe substitute for a provider-supported offline
filesystem/memory-preserving export-and-writeback or pre-resume process barrier;
recoverable killed-source data still requires its retained original checkpoint.
Active records use the existing native launcher and freshly verified, exact-scope
authentication at the same epoch, not imported credentials or expanded permissions.

An original-epoch terminal **candidate** requires persisted stopped/failed, kill action,
termination timestamp and reason. Those metadata fields do not prove completion:
the server must independently attest the exact registered physical VM's genuine 404,
exhaust all retained physical snapshots, and capture the frozen authoritative source.
Unknown timestamps, surviving work, inaccessible extant data, and duplicate physical
records for one logical ID stay blocked. The report retains every duplicate and its
exact tuple; it never chooses the newest record or silently deduplicates history.

The retained staging inventory and bounded read-only investigation on 2026-10-21
do not establish that legacy histories are corrupt or safely discardable. The
811-record report retains 669 blocked records, 141 explicit non-Crew records and
one fenced-import candidate; this is not a fresh post-compatibility inventory.
Fourteen exact raw profile-version values remain unavailable from retained DTOs:
the ordinary read-only list omits the runtime-profile object, while detail reads
normalize stored lifecycle state. Twelve of the 27 investigated records still
lack historical physical identity. For the other 15, exact-ID reads returned 404
and complete snapshot inventories were empty in both documented provider realms;
these are current, realm/credential-scoped observations, not invented kill times
or completed migration receipts. The two duplicate logical groups still lack
authoritative current/superseding workspace bindings; no history is selected by age.
Offline source capture and the actual source-history backup remain incomplete.
The parent journal's trusted native-ID/cwd association is present, but its pending
nudge and active writers must be reconciled before taking the final consistent copy.
These unresolved inputs remain gates; the epoch separation above does not bypass them.

Desktop bulk capture reuses that same source rather than refreezing overlapping rooms:

```sh
python3 scripts/capture-staging-desktop.py --database /ABS/COPY/docs.sqlite3 --auth-file /PRIVATE/auth.json --output /PRIVATE/capture --migration-id EXPLICIT_BATCH --cloud-plan /PRIVATE/cloud-plan.json --via-staging-bridge
```

This defaults to authenticated read-only planning. Add `--apply` for authorized capture
or `--resume` for explicit recovery using persisted IDs. Every s3/s4/workspace scope is
inventoried. Matching prepared cloud scopes use authenticated coordinator receipts and
raw-source GETs, not a second POST; ambiguous physical groups require an exact retained
source receipt and still cannot invent terminal authority. Bodies are streamed with
identity encoding, exact Content-Length and SHA-256; no whole-response JSON buffering.
`complete.json` is written only after all required source/terminal checks succeed.

For scope-less bootstrap candidates, add `--native-cli /ABS/COMET`
`--source-backup-root /ABS/COPY --writers-stopped` and repeat
`--native-history-root /ABS/COPY/OMP_HOME` for every disjoint copied native root.
The collector invokes the native read-only `inspect-backup` classifier itself,
binding its report to the exact SQLite hash and each streamed snapshot hash.
Only that native-proven inert disposition requires no remote room; arbitrary
classification JSON, an empty-looking row, or a special-cased ID is not proof.
Original IDs, bytes and hashes remain in the plan and unchanged SQLite copy;
unknown, nonempty or referenced candidates remain blocked.

The output `raw/` and `context.json` feed the existing offline export command:

```sh
comet export-backup --source-profile-root /ABS/ORIGINAL --source-backup-root /ABS/COPY --database-relative docs.sqlite3 --destination /PRIVATE/import-backup --writers-stopped --identity-session /PRIVATE/EXISTING_OWNER_SESSION.json --source-context /PRIVATE/capture/context.json --source-storage-directory /PRIVATE/capture/raw --native-history-root /ABS/COPY/OMP_HOME
```

Repeat `--native-history-root` for every copied native root. If attachment blobs
are required, use `--inventory-attachments-only` first, retain the exact requested
blobs, and pass their directory with `--attachment-root` to the final export.
`sourceWitnesses` is required for every non-inert document and contains its
actual `{migrationId, sha256}`; a batch ID is not per-room authority. Add
`--terminal-authority-root /PRIVATE/capture/terminal-authority` when the capture includes
server-attested original epochs. The existing exporter derives their native identity
and original command map from real frozen source plus retained owner replica/config,
retains all evidence, and exempts a missing journal only for that verified old epoch.
It never creates a replacement journal or converts unknown outcomes into success.

Import into a fresh, authenticated owner-controller profile with the existing importer,
verify it, and start that same isolated native controller. Publish selected histories:
`comet --data-dir "$OWNER_DATA" --ipc-port "$PORT" session publish-imported "$UUID1" "$UUID2"`.
This owner-only RPC posts actual stored snapshots and awaits registry acknowledgments;
it launches no agent. Terminal commit selects `--destination-device-id` only as an
address: the coordinator fetches `VerifyImportedCrewSession` from that authenticated
controller, hashes its exact raw proof, and independently reads native Edge checkpoints
and registry rows. The verifier's device is distinct from the original absent guest.
Only verified readback permits CAS, preserving the original record's status and epoch.
Old pending/unknown commands remain visible and inert. A later explicit `starting`
creates a genuinely new epoch strictly greater than the retained coordinator epoch;
its verified bootstrap carries the original proof and
inert command map before sync, and establishes a fresh native association without
replaying or silently resuming the old one.

A failed or lost response stops the operator without retries or automatic rollback;
the report persists an `unknown` intent **before** dispatch. Keep all source/destination
copies and the server claim. Explicit recovery reads the existing claim and reruns only
the same fenced preparation/readback. An untouched, authoritatively `unprepared` record
uses serialized `prepare`; a delayed original request reuses that same claim rather
than creating a second token. Unknown claim state, changed identity, unresolved lease,
incomplete import, or changed preserved state remains blocked. Abort is not permission
to clear a fence: it needs independently verified source restoration. After native
writes, restoring a stale legacy backup is not lossless recovery.
If generation CAS succeeded but native activation did not, recovery continues the
fenced native activation; it does not re-import, clear the claim, or revive legacy
writers. The committed server proof remains available for the later activation gate.

Deployment order is immutable candidate publication, compatible staging Worker/provider
with the legacy template retained and old provider ingress drained, full-inventory
migration, then the native-template activation gate. That gate must fetch current
authority and durable server import proofs; a self-hashed report or operator boolean
is insufficient. No production redeploy or production state mutation is part of this
operator. Focused operator regressions are runnable with
`node --test scripts/migrate-staging-cloud.test.mjs`; this implementation task does not
execute that check or any live apply.
Before every apply/recovery mutation, the operator executes the reviewed Platform
`internal/scaffold/scripts/deploy-staging-compatible-services.mjs --verify-receipt`
read-only subcommand. It re-fetches actual provider pods/deployment and Cloudflare
Worker versions, requiring the recorded immutable controller commit and original
drain identity. It uses the existing kube context and
`CLOUDFLARE_API_TOKEN_SCAFFOLD`/`CLOUDFLARE_ACCOUNT_ID`; credentials are not reported.
A stale receipt or failed live verification blocks dispatch, not merely final promotion.

Shared records are inventoried, not discarded or presumed migratable. For each actual
record, revalidate any reachable existing live authority. A copied publication alone
cannot establish a contributor's current grant, expiry/revocation, original
sandbox/device/epoch, or native-journal association: legacy grant envelopes were
socket-local rather than persisted in Loro. Where those facts are absent from both
retained records and existing authenticated authority, report the exact missing
contributor/binding and keep bytes offline; do not invent a historical-membership
predicate or issue a broad replacement grant.

The legacy source also requires a real Edge write barrier plus its authoritative raw
Loro snapshot, merged with the copied host snapshot while retaining both originals.
Host shutdown alone does not capture later mobile commands or remote annotations.
The existing legacy routes have no all-writer freeze: workspace `reset-log` is
destructive, and grant revocation does not close non-grant Scaffold sockets. An HTTP
route switch or equal repeated snapshot hashes is not a fence for existing sockets.
Without a trusted source barrier producer covering HTTP and existing WebSockets,
provider preparation and generation activation fail closed with
`legacy_edge_writer_barrier_required`; no caller receipt can satisfy that prerequisite.
The authorized one-time source adapter is built separately from the observed legacy
staging revision in a disposable migration worktree; it is not a second maintained
runtime. It retires all legacy writes, closes existing sockets, and exports raw
`legacy-session-sql-v1` storage under the original exact owner/read/control authority.
The coordinator fetches the persisted bytes itself, validates their digest and room
scope, and binds them to its original sandbox/device/epoch claim. The native offline
decoder reconstructs the snapshot plus ordered update log, verifies causal
completeness, and preserves the original raw envelope. Only that real frozen-source
proof can satisfy the barrier gate; deployment acknowledgment alone cannot.
Cloudflare documents [Durable Object global uniqueness and eventual code updates](https://developers.cloudflare.com/durable-objects/platform/known-issues/):
uniqueness is enforced on events and storage access, so a replaced instance's later
storage access throws. Therefore require a successful persisted freeze/export receipt
from the replacement class for each room, not a deployment timestamp. Use one migration
version at 100%, no gradual split, and never roll back to source writer code after capture.

## 7. Keeping the fork updateable

### Ownership rules

- Upstream owns core sync, chat persistence, registry, rendering, generic harness lifecycle, and ordinary settings. Crew owns its auth/control-plane adapters, managed inference context, native OMP integration, scoped lifecycle operations, and distribution contract.
- Keep upstream internal crate names and file locations. Brand the customer-facing product as Crew, but avoid a mechanical repository-wide identifier rename on every import.
- Keep logical commits by integration area. Do not cherry-pick hundreds of old commits or replace hot upstream files with old Crew versions. Port behavior onto current APIs and retain only regression tests that prove consumer-visible invariants.
- Submit genuinely generic fixes or small extension points upstream when appropriate; upstream acceptance is not a prerequisite for the port. Do not build hypothetical extension machinery in advance.
- Record the accepted upstream SHA, Crew revision, runtime-contract version, release tuple, integration seams, and verification evidence in the repository/release metadata. Use existing version/runtime files where suitable, not a custom patch-management tool.

### Routine update procedure

1. Assign an integration owner; review upstream weekly, with earlier updates for relevant security/reliability fixes. Select and record a specific upstream main/tag commit after reviewing its CI and migrations.
2. Fetch from the corrected upstream URL and merge the selected commit on a dedicated upgrade branch/PR. Merge, rather than rebase, the published Crew branch; do not rewrite deployed history. Initial unpublished port branches can be rebased while establishing the baseline.
3. Review every changed integration seam, even if Git reports no conflict: auth/profile, Edge authorization, `RunControls`/harness launch, session ownership, RPC/schema, updater, and packaging. Resolve conflicts toward upstream structure and reapply only the needed Crew behavior.
4. Run remote compile/typecheck and focused behavioral checks for the supported desktop/headless targets, then exercise the core headed/staging scenario and migration compatibility. Keep the upstream/default path working where retained; do not require live WorkOS access just to test Crew.
5. Record the conflict/fix list and evidence in the upgrade PR. A repeated conflict is a reason to move or shrink the integration seam, not to accumulate another parallel implementation.
6. Release through the same staging/captain/production gates. Advance the recorded upstream baseline only with the tested merge. Keep source, runtime, Edge, and client compatibility explicit; “Git merged cleanly” is not a release gate.

If there is no newer upstream commit by phase 5, rehearse the integration across two recorded upstream revisions ending at the target, rather than declaring updateability from an empty merge.

## 8. Verification and rollout requirements

Reuse the existing behavioral fault cases for lost acknowledgements, restart recovery, snapshot replacement, and stale commands, adapting them to the new transport. Do not import tests that pin old wiring or source text.

Minimum release evidence:

- Auth and authorization checks described above, including grant invalidation on connected clients.
- Real Agent Auth streaming for the supported providers, route receipts, cancellation, and managed-route coverage.
- Real Scaffold create/pause/resume/stop/reattach with controller restarts and ambiguous outcomes.
- Upstream sync under backpressure/reconnect and many open chats, including UI responsiveness and bounded resource use. Ensure Crew additions do not bypass upstream budgets or reintroduce blocking persistence on the async runtime.
- Copied-profile migration, pending-command reconciliation, native-history continuity, attachment integrity, and a demonstrated recovery procedure.
- Actual headed desktop/iOS proof for retained comment/settings behavior, same-user cross-client visibility, and existing deep links. Exercise iOS foreground/background, network recovery, and streaming on simulator and device; smoke web/CLI consumers against the matching runtime.
- An upstream-update rehearsal with the same acceptance checks, not just a successful compile.

No local typechecks: keep `ASHLER_INCREMENTAL_TSC_CHECKS=false`, including Git operations. Run typechecks in authorized remote CI. Executed implementation evidence and outstanding gates are recorded above; do not infer completion from the planned checks.

The isolated canary uses the private manual delivery documented in [distribution guidance](../dist/README.md), not the shared updater. Eventual cutover must preserve the [existing Crew release gates](https://github.com/Ashler-AI/comet/blob/297717c55beab9323c53095f3f5f683f00676a5e/skills/comet-work/SKILL.md), including the [platform-first runtime sequence](https://github.com/Ashler-AI/comet/blob/297717c55beab9323c53095f3f5f683f00676a5e/README.md). Preserve an immutable staging candidate, wait for the captain's explicit manual verification of the restarted build, and promote that exact candidate only with production authorization. Coordinate both Scaffold image lanes and Edge/client compatibility. The documented production pin-operation gap must be resolved before rollout, not guessed during release.

## Source anchors

Legacy Crew links below are pinned to its committed source, not the new integration tree. The original working-tree changes were kept untouched.

- Login/grants: [`crates/engine/src/auth.rs`](https://github.com/Ashler-AI/comet/blob/297717c55beab9323c53095f3f5f683f00676a5e/crates/engine/src/auth.rs), [`edge/src/auth.ts`](https://github.com/Ashler-AI/comet/blob/297717c55beab9323c53095f3f5f683f00676a5e/edge/src/auth.ts).
- Agent Auth and lifecycle: [`crates/engine/src/inference_relay.rs`](https://github.com/Ashler-AI/comet/blob/297717c55beab9323c53095f3f5f683f00676a5e/crates/engine/src/inference_relay.rs), [`crates/engine/src/scaffold.rs`](https://github.com/Ashler-AI/comet/blob/297717c55beab9323c53095f3f5f683f00676a5e/crates/engine/src/scaffold.rs).
- Host-only context/OMP: [`crates/harness/src/lib.rs`](https://github.com/Ashler-AI/comet/blob/297717c55beab9323c53095f3f5f683f00676a5e/crates/harness/src/lib.rs), [`crates/harness/src/omp/mod.rs`](https://github.com/Ashler-AI/comet/blob/297717c55beab9323c53095f3f5f683f00676a5e/crates/harness/src/omp/mod.rs).
- Current runtime/security invariants: [`docs/ASHLER-SCAFFOLD-END-STATE.md`](https://github.com/Ashler-AI/comet/blob/297717c55beab9323c53095f3f5f683f00676a5e/docs/ASHLER-SCAFFOLD-END-STATE.md).
- Current annotation/settings behavior: [`crates/ui/src/shell.rs`](https://github.com/Ashler-AI/comet/blob/297717c55beab9323c53095f3f5f683f00676a5e/crates/ui/src/shell.rs), [`crates/ui/src/multiplayer.rs`](https://github.com/Ashler-AI/comet/blob/297717c55beab9323c53095f3f5f683f00676a5e/crates/ui/src/multiplayer.rs), [`crates/ui/src/settings/accounts.rs`](https://github.com/Ashler-AI/comet/blob/297717c55beab9323c53095f3f5f683f00676a5e/crates/ui/src/settings/accounts.rs), [`crates/ui/src/settings/permissions.rs`](https://github.com/Ashler-AI/comet/blob/297717c55beab9323c53095f3f5f683f00676a5e/crates/ui/src/settings/permissions.rs).
- Upstream [auth](https://github.com/zeronsh/zeron/blob/3b2f78027a2c31622c39c35716da4d6e1af8de15/crates/engine/src/auth.rs), [profiles](https://github.com/zeronsh/zeron/blob/3b2f78027a2c31622c39c35716da4d6e1af8de15/crates/engine/src/profile.rs), [harness controls](https://github.com/zeronsh/zeron/blob/3b2f78027a2c31622c39c35716da4d6e1af8de15/crates/harness/src/lib.rs), and [Edge chat authorization](https://github.com/zeronsh/zeron/blob/3b2f78027a2c31622c39c35716da4d6e1af8de15/edge/src/chat-room.ts).
- Upstream [comments](https://github.com/zeronsh/zeron/blob/3b2f78027a2c31622c39c35716da4d6e1af8de15/crates/ui/src/comments.rs), [shared comment controls](https://github.com/zeronsh/zeron/blob/3b2f78027a2c31622c39c35716da4d6e1af8de15/crates/ui/src/comment_ui.rs), and [settings](https://github.com/zeronsh/zeron/tree/3b2f78027a2c31622c39c35716da4d6e1af8de15/crates/ui/src/settings).
- Upstream mobile [architecture](https://github.com/zeronsh/zeron/blob/3b2f78027a2c31622c39c35716da4d6e1af8de15/docs/mobile-rewrite.md), [shared live sync](https://github.com/zeronsh/zeron/blob/3b2f78027a2c31622c39c35716da4d6e1af8de15/crates/client/src/live/mod.rs), [foreground/background handling](https://github.com/zeronsh/zeron/blob/3b2f78027a2c31622c39c35716da4d6e1af8de15/crates/client/src/client.rs), and [UniFFI bridge](https://github.com/zeronsh/zeron/blob/3b2f78027a2c31622c39c35716da4d6e1af8de15/crates/mobile/src/client_ffi/mod.rs).
- Current Crew [desktop URL handling](https://github.com/Ashler-AI/comet/blob/297717c55beab9323c53095f3f5f683f00676a5e/crates/ui/src/state.rs) and [iOS URL handling](https://github.com/Ashler-AI/comet/blob/297717c55beab9323c53095f3f5f683f00676a5e/apps/ios/Comet/App/AppModel.swift).
