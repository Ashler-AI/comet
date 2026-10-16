# Crew on current upstream: port and update plan

## Implementation status: isolated upstream canary

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
Smoke and URL-isolation amendments are committed and pushed as `516ad3a2`;
the pooled-account/sandbox UI and control fixes are `eb938af2`.
[Its private artifact CI](https://github.com/Ashler-AI/comet/actions/runs/38934010957)
passed on macOS, Linux x64, Linux ARM64 and remote Edge contracts/typechecks.
The downloaded Linux x64 archive and executable checksums were verified locally;
nothing is published to a shared release channel.
This is a testing lane, not production or a completed cutover.

| Area | Exercised evidence | Remaining acceptance |
| --- | --- | --- |
| Desktop | Local app rebuilt and launched with separate Accounts/Sandboxes/Annotations pages, provider-grouped shared-account usage, bounded session details, sidebar/new-session sandbox entries and managed-model selection; actual UI creation confirmation and successful link navigation exercised | Live shared-pool rows and managed turns await broker deployment |
| Isolation | Bundle `ai.ashler.crew.upstream-canary`, IPC 27954, data `~/.crew-upstream-canary`, only `crew-upstream-canary` registered as its URL scheme | Authenticated profile restart and cross-client verification |
| Sync | Real workerd plus two real headless engines: session discovery, queued remote Run, transcript and working-to-idle status roundtrip passed | This used mock inference; reconnect/backpressure and real managed turns remain |
| Edge | Live `https://crew-upstream-canary.cgoggins.workers.dev`; health returned 200, Scaffold auth and the exact canary environment; version `9277a59d-5e0a-40f6-844c-187494aed4ff`; unauthenticated identity/registry/chat reads returned 401 even with forged identity headers; public installer returned 404 | Live signed-in desktop/mobile authority and revocation |
| Scaffold | Canary Helm revision 3 deployed the exact-baseline provider directory fix; authenticated directory GET now succeeds; existing failed attachment reconciles to rejected without replay; new sandbox form uses deployment source/default region and explicit managed-model selection | Shared inference admission and successful create/attach/pause/resume/stop |
| Protocol checks | Shared-auth consumer/binding/OMP contracts 33/33, exact-live-staging broker security 6/6, canary config 2/2; existing Edge workerd 38/38 and wake/backup alarms 8/8 | Live shared-broker path awaits explicit deployment/key approval |
| Rust checks | Scaffold library 39/39 and resolved-link navigation regression 1/1 passed; desktop build/package succeeded; existing client/document/engine/protocol/sync/harness checks recorded above | Actual shared-provider and sandbox runtime scenarios |
| Mobile | Shared Rust core, generated Swift bindings and native app built; iOS 26.5 simulator displayed Crew Canary; warm directory link and cold Scaffold link stayed signed out; exact cold-link intent persisted across launch | Authenticated cross-client scenarios, lifecycle/network recovery and physical-device gates |
| Clean upstream | Unmodified `3b2f7802` executable built in a detached worktree; separate workerd and two clean engines completed remote queued Run, transcript and working-to-idle sync | Mock inference only; no WorkOS credentials were required or borrowed |
| Update rehearsal | Crew patch replayed onto `9609f994`, then actually merged `3b2f7802`; merge `06ac4276703a68db1ae8be50fbce0b568df135ba` is tree-identical to `eccc3324`; merged executable passed real two-engine/workerd queued Run, transcript and working-to-idle sync, headed desktop checks, shared mobile-core/Swift/native iOS builds and simulator cold launch | Authenticated cross-client/provider/lifecycle gates |

The initial empty canary vault caused the reported `agent_auth_v2_account_unavailable`.
Read-only metadata verified the existing Crew Staging pool has 5 connected OpenAI
and 6 connected Anthropic accounts. Its canonical owner ID differs from the
isolated canary's owner; pointing at the same database would not fix ownership.
No provider credentials have been copied, imported, or connected.

Platform `0c1de2aa850` prepares the canary consumer of a narrow staging broker.
The shared-service patch is separately based on the **exact live staging source**
`94dc2c9cade2b89b4ba4b3fc80bee777695edbc4`, not the divergent canary fork.
Commits `2b09634cc57` (broker) and `abe9f0d93e3` (directory compatibility) passed
[remote Worker/vault/provider CI](https://github.com/Ashler-AI/ashler-platform/actions/runs/38932761302),
including typechecks. Shared-controller deployment and a new dedicated server
trust key await explicit user approval. This is not a live shared-pool claim.
Account enrollment, import and revoke remain in Crew Staging; Canary displays and
uses only the authorized pool. All delegated authority scopes are canary-prefixed
and retain the shared v2 private-auth gate and sandbox lifecycle validation.
The canary consumer also passed its own full
[remote Worker/vault/provider CI](https://github.com/Ashler-AI/ashler-platform/actions/runs/38933796743).

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
directory link both preserved the exact signed-out intent; IPC `WatchChats` and
`WatchSessions` remained empty. The inherited `zeron:` startup registration is
removed; after cold launch, Launch Services still selected the installed Crew
production/staging handlers and had no canary-owned `zeron:` override. Safari's
browser-to-app prompt remains unverified: its private window locked, and the
foreground-control request was not granted. Native dispatch is not browser proof.

The exact `516ad3a2` Linux x64 runtime is configured under
`~/.crew-upstream-canary/runtime/516ad3a2c42f478dddd23ef9fb7bfff3087e0cd7/comet`,
SHA-256 `03866b003aed7389241b34dd924081c0b33a3b83f3705c69f4b428da225d2966`.
The archive digest is `7ceba3a16d9a8c882b2946cad11a1a11d3e6f8b4b0cf7ce2238fe12bf4d42774`;
provenance records `scaffold.comet-runtime.v1`. This is not a sandbox-run claim.
The new `eb938af2180996f6a5afa9643c7c5733aff48a85` Linux runtime is also downloaded
and checksum-verified at the same runtime-directory layout, SHA-256
`aa386f9941202d01cf05b53abd4318ce5271c0a26295f5af533ee4a8ab1c9c36`.
It has not replaced the running host's artifact pin or any sandbox binary;
activation belongs to the approved shared-auth/runtime smoke, not a silent restart.

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

Authenticated cross-client/provider/lifecycle checks and physical-iOS verification
remain open; there is no production promotion. Local typechecks are intentionally
not run to preserve workstation resources. Remaining gates below are requirements,
not claims of completed testing.


Status: implementation and verification in progress; no production cutover.

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
