# Crew

Crew is Ashler's internal, multi-device controller for coding-agent sessions. The repository, binary, protocols, and service identifiers retain the `Comet` name for compatibility.

## Crew 0.1.149: concurrent read-marker recovery

The installed 0.1.148 client still rejected new-session creation when retained
`lastSeenAt` observations advanced independently on two clients. Shared workspace
reconciliation now keeps the latest valid, nondecreasing read observation,
including the cached value. Explicit unread/deletion, backwards transitions,
ownership changes and genuine competing edits keep their existing safeguards.
The metadata warning and creation failure share this cause; neither is suppressed.

Private replay against a fresh read-only server capture recovered all **156**
retained records, preserving every original before/value, and passed new-chat
creation, principal membership and cold reopen. All **116** document tests passed,
including concurrent reads and explicit unread conflicts. This is isolated
verification, not installed-client or phone acceptance. No live cache was reset.

The same capture passed actual `WorkspaceHost` creation, principal membership
and cold reopen, with only the normal local device boot clock/version restamp.
The isolated headed client displayed the new session; typed Local start completed
with mock-agent output and no metadata error. **315** engine and **57** sync tests
also passed. The real native/Edge 24-turn smoke preserved **1,600** history rows
through outages, crashes and lost acknowledgements, catching up in **2,494 ms**
with **89,632 KiB** peak observed RSS. Local typechecks were intentionally skipped.

A later fresh capture exposed a coalesced archive intent against an independently
created host row. Recovery now distinguishes the host's initial `archived=false`
from a deliberate restore: retained causal history must prove both the row and
flag were first insertions. Real restores, row recreations and unavailable proof
remain blocked. Empty chats no longer require a message clock to reconcile their
initial metadata. All **176** retained intents then replayed unchanged, preserving
the archive choice and passing new-chat creation, membership and cold reopen.

Mobile build 32's source-matched simulator and live native/Edge checks passed in
[run 37954096697](https://github.com/Ashler-AI/comet/actions/runs/37954096697), including
actual command admission and streamed output. The final native archive correction
also passed the local **116 document / 315 engine / 57 sync** suite and headed demo.

Desktop and Devbox engines require independent native updates. The first 0.1.149
release was blocked before publication by a separate live mobile creation failure:
iOS duplicated the host's accepted row while its publication arrived, with different
nil/null config encoding and creation clocks. Mobile staging build **32** removes
that second writer for Local and Scaffold creation, waits for principal-scoped
catalog readiness, and retains the creation ID across retries. Older equivalent
creation intents recover without discarding real config edits or ownership guards.
Build **31** remains the latest published TestFlight release until build 32 is
separately signed and uploaded. Production and both Scaffold image pins remain
gated on captain verification of the restarted staging desktop. Physical-phone
convergence is not yet verified.

## Crew 0.1.148: retained catalog convergence

Native workspace recovery now selects `lastMessageAt` and its preview together,
including a newer cached observation, instead of rejecting the preview after
merging its clock. Independently seeded chat and membership rows retain missing
metadata only when it was never deleted; owner/scope, conflicting user edits,
explicit unread markers and membership removals remain guarded.

Recreated rows use the insertion's own causal ancestry to prove that an earlier
tombstone was observed. Unrelated room traffic no longer blocks a legitimate
recreation; a later cache import cannot authorize a concurrent resurrection.

Isolated replay of the captured server snapshot and two accepted updates recovered
all **142** retained records, passed repeated adoption and cold reopen, and kept
every original intent's before/value. The capture contained **1,749** local chat
rows versus **1,738** on the server, plus **1,630** versus **1,619** memberships.
This verifies recovery on private copies, not publication of that backlog or the
installed phone's convergence. Mobile still requires its own principal's membership;
the repair does not expose other project users' sessions or bypass cleanup safeguards.

New local sessions persist their catalog row before checkout and agent admission,
so the same retained recovery error also blocked first send. The shared repair
restores that path without bypassing the commit guard. A private `WorkspaceHost`
replay retained all 142 originals and recovered both a partial-creation retry and
a fresh chat plus principal membership across cold reopen. A separate isolated
native instance passed fresh creation, typed Local start, and mock-agent response.
The real Edge/native 24-turn smoke preserved 1,600 history rows across outages,
lost receipts and restarts, with 4,001 ms catch-up and 80,336 KiB peak observed RSS.

## Crew 0.1.147: lossless workspace recovery

Loro's immutable causal-vector merges used mutable lookups for unchanged
counters, copying shared tree nodes. Rebuilding the same shallow-root vector
amplified that cost. A tiny accepted update could exhaust the shared Worker's
128 MiB limit, disconnecting catalog readers and rejecting subsequent writes.
Both native and Edge now preserve that structural sharing. Snapshot folds also
reuse the encoded history at the existing retained boundary instead of decoding
and copying every operation; no history boundary or permission is changed.

Exact-byte replay of the captured snapshot and 27 accepted records, lossless
folding, cold reopen and a returning offline edit used **80.94 MiB** of WASM,
versus **174.13 MiB** before. All **299,330 operations**, complete state, version
vector and retained frontier matched. The synthetic 450-peer concurrent-history
regression fails the old runtime at **240.06 MiB** and passes at **11.75 MiB**.
Run it with `node edge/scripts/workspace-replay-memory-smoke.mjs`; private
baseline/delta paths remain supported and are never committed.

Snapshot admission also avoids merging two hot workspace replicas at the same
retained boundary: it imports only the candidate's new operations into accepted
state. The captured native recovery snapshot preserved **309,686 operations**
and cold-restart state at **80.63 MiB**, down from **147.44 MiB**. The permanent
memory smoke now exercises actual Durable Object admission, duplicate delivery
and durable cold replay, not only the Loro library.

Local verification passed **217 Edge**, **111 document**, **315 engine**,
**21 RPC**, **57 sync** and **578 UI** tests, plus subprocess cancellation and
startup-negotiation regressions. The 24-turn real two-device/Worker smoke retained
**1,600** history rows through crash/reconnect and converged in **4.13 seconds**.
The isolated headed demo rendered the populated model picker and completed mock
response. These checks do not establish installed-client or hosted-rollout status.

Native recovery includes guarded complete-cache adoption and clears the cold
repair latch only after the matching authenticated repair ACK. Unknown ACKs,
foreign histories, genuine competing edits and resurrections remain blocked.
The original intents are preserved; recovery does not reset a live workspace.

Directory projection streams native and legacy tool text in UTF-8-safe 64 KiB
chunks instead of rejecting large transcript rows. Snapshot adoption atomically
requeues the canonical directory job, including arrivals after missing-source
failures; it preserves deletion fences, retry backoff and causal checks. Truly
absent snapshots remain pending rather than publishing invented history.

Same-device chat switches retain harness/model catalogs. Discovery is cancellable
and bounded at the UI, engine and subprocess; startup negotiation fails explicitly
after 15 seconds without publishing readiness. Credentials, device, connection,
harness updates and explicit retry/wake invalidate the relevant catalog.
Cancellation owns the unreaped subprocess group even after its leader exits;
macOS returns `ESRCH` from `getpgid` in that state despite surviving descendants.

A stalled RPC subscription no longer blocks the shared reader. Its bounded
ordered prefix is followed by an explicit overflow error and upstream cancellation;
transcript/snapshot watches resubscribe, while terminal replay retains its sequence
cursor. Forwarded streams preserve errors; durable command admission is unchanged.
At most 256 active or queued-for-cancellation subscriptions are admitted per
connection, plus one cancellation being sent.

The private Edge runtime is built from Loro
`45708d059d8620fb53066c9f86cefa1601e0e1c6` with
`edge/vendor/loro-1.16.4-crew.1.patch`. Apply that patch to the pinned checkout,
then build `loro-wasm` for `wasm32-unknown-unknown` through
`python3 scripts/local-cargo.py build --locked --release --manifest-path
UPSTREAM/Cargo.toml -p loro-wasm --target wasm32-unknown-unknown`.
Use `edge/scripts/build-loro-runtime.mjs WASM_PATH WASM_BINDGEN_PATH OUTPUT_TGZ
UPSTREAM_SOURCE` with wasm-bindgen **0.2.100**; it verifies the published wrapper
archive's SHA-512 and packages only fresh Node/web bindings. It never runs a
local TypeScript check. Local typechecks remain intentionally disabled.

## Crew 0.1.146: published Scaffold source by default

New Scaffold drafts select **Published source**, omitting a source override so
the sandbox keeps its published checkout. **Latest master** explicitly requests
`master`; branches and commit SHAs remain explicit choices. Select **Published
source** again to clear an override. Local checkout and worktree bases never
choose Scaffold's source, and attached sessions retain the remote source details.

Native OMP handoff also omits its former implicit `master` override. Explicit
handoff preparation refs are preserved, and the transferred worktree and nested
working directory remain the execution location. Database selection and isolation
are unchanged. The runtime contract is unchanged.

## Crew 0.1.145: mobile catalog convergence

The shared workspace could not materialize its accepted journal: batching the
captured 76 updates grew Loro WASM memory to 134 MiB before JavaScript overhead,
exceeding Cloudflare's total 128 MiB Worker limit. Desktop retained a cached
session list while mobile could not receive the same catalog. Fresh device
heartbeats did not establish durable workspace convergence.

Workspace replay now streams every accepted record in sequence, retaining the
final causal-dependency checks and membership removals. Exact-byte workerd replay
preserved all 1,666 raw chats, 1,547 memberships, complete state and version vector;
WASM used 99.8125 MiB after import and 106.6875 MiB after materialization, versus
134/140.6875 MiB before. Hosted staging subsequently served all 1,666 chat rows
and 1,547 memberships to an authenticated cold reader without clearing history.

Mobile build 31 defers local uploads during authoritative recovery and rejects
queued updates from a replaced document. Retained intents survive adoption;
the existing mobile regression covers recovery edits, live catalog updates and
membership revocation. All 217 Edge tests passed. Local typechecks were
intentionally not run to preserve workstation resources; mobile verification
and native/Worker type gates run in remote CI.

Native recovery also preserves the imported histories' version vectors before
normalizing legacy self-session aliases. Normalization creates local operations;
counting them as unseen remote edits falsely conflicted with retained user intent
and prevented the workspace from joining. Identity, route and genuine concurrent
edit checks remain enforced ([PR #86](https://github.com/Ashler-AI/comet/pull/86)).

The signed, notarized **0.1.145** candidate from merged source
`c0445cc0a4511f05236e4443eb8039a814f91648`
([run 37820291549](https://github.com/Ashler-AI/comet/actions/runs/37820291549)) passed
remote native/mobile gates, including all **110 document** and **55 sync** tests.
An isolated copy of the actual failed **0.1.144** cache reproduced the conflict;
the candidate recovered **4,784 retained intents**, connected, and converged
**1,601 memberships**, including the previously missing active session, through
the real local Edge Worker. No live database writes, provider inference, local
compilation or local typechecks were used. Captured user data was not committed.

## Crew 0.1.144: sync and restart reliability

Crew repairs journal-only workspace catch-up, bidirectional room backpressure,
lost upload acknowledgements, and concurrent Loro/presence membership updates.
Accepted durable updates still reach authorized readers when their publisher
disconnects; a stalled reader's authority lookup no longer blocks other readers.
Engine relay replacement ends the old RPC subscriptions so clients reconnect
instead of retaining streams the new engine does not own.

Online startup leaves completed histories cold and recovers outstanding work.
Planned exits refresh interrupted-request eligibility without spending the crash
retry budget; teardown errors do not retire resumable work. Explicit Stop and
successful completion remain terminal. Presence probes start immediately and
publish healthy peers without waiting for dead ones.

Ordinary UUID imports observe canonical same-owner room activity before their
workspace chat row arrives, without claiming host placement. Desktop imported
rows display that activity. Mobile publishes coherent completed transcript
projections during continuous streaming and performs a trailing catch-up pass;
ambiguous unary relay failures are not automatically replayed. Explicit Scaffold
attachment obtains verified authority before reopening legacy scoped caches.
Desktop status strips, sidebar indicators and composer activity now consume newer
same-owner room activity ahead of an older workspace/watch row. Child activity
uses the engine's shared owner/scope selection; genuine stale heartbeats and
newer terminal outcomes retain their precedence.

Project/principal isolation, removed memberships, scoped-cache conflicts and
single-writer protections remain enforced. Roll out Edge before native clients;
existing remote engines require their own update/restart. Conflicting local caches
are retained, not silently rewritten.

Local verification exercised 1,600 retained workspace rows and 24 turns through
real native engines and a local Worker, including offline edits, crashes, lost
admission replies, reconnect and revocation. That 24-turn run's workspace catch-up
took 4.006 seconds; agent inference and Scaffold authority use isolated test fixtures.
All 575 desktop tests passed, including a failing-before/passing-after stale-index
regression. A rebuilt headed demo showed Working with a two-minute-old workspace
row and a fresh owner publication. The earlier demo rendered its completed response
with one original user entry and authoritative `applied` command readback.
A subsequent two-device run kept owner activity fresh beyond the 45-second lease,
then passed reconnect, crash recovery and revocation checks with 1,600 history rows.
Initial local mobile verification was syntax-only parsing. Exact-source remote
native, Edge and mobile gates are required before staging publication. Local
typechecks were intentionally skipped to preserve workstation resources.

## Unreleased: cross-device workspace recovery

Legacy workspace rows and retained journals normalize repeated self-session
aliases together, preserving canonical ownership, tombstones and completed
outcomes. Covered bootstrap observations can coalesce; conflicting user edits
or unknown ancestry retain their original evidence and remain blocked.

An authenticated workspace checkpoint that is already covered by the complete
local cache cannot roll back newer accepted edits. Recovery keeps that isolated
cache only when it still matches every retained intent, preserving archived
sessions and field tombstones without blocking new-session metadata commits.
Unseen competing edits and remote resurrections still retain their original
evidence and fail closed; no automatic reset or cache deletion is performed.

Same-owner session memberships can learn their first environment route. Recovery
preserves that route across restarts and refuses changes to known scopes or
owners.

Durable command admission is independent of metadata and notification failures.
Desktop and mobile retain the original command identity across retries and
restart, read the owner's outcome, and distinguish accepted-pending delivery
from unknown admission or authoritative rejection. Completed sends are not queued
again; peer outcome reads retain the same exact authenticated scope as admission.
AppState events refresh unresolved outcomes without re-admission. Authoritative
terminal proof retires the original journal record and its associated delivery
notice; metadata-recovery and unrelated warnings are not cleared by that proof.
Mobile retries recognize the original materialized message despite a lost
admission reply, retire its retained draft, and do not admit it again. Terminal
rejection remains authoritative.

Interrupted transcript checkpoints keep richer acknowledged output and monotonic
task progress while retaining one interruption marker; a conflicting acknowledged
marker remains blocked, and acknowledged terminal outcomes still win.
Recovery-blocked admission cannot append new commands, and
a failed commit retains authority only for an already-materialized original intent.
Command execution waits for document recovery. Desktop commands whose delivery
is notified do not show a delivery-pending warning while awaiting execution;
actual delivery failures and metadata-recovery warnings remain visible.

The retained staging transcript recovered against its authenticated remote snapshot
with all 53 acknowledged parts plus the interruption marker. All 742 local checks
passed, and an isolated rebuilt native engine completed an instruction and its
same-identity retry without duplication. The paid-provider image test stayed
ignored; foreground UI smoke lacked desktop-control confirmation. Installed
clients were not replaced, and local typechecks were intentionally skipped.

Composer submissions claim their draft and snapshot the authorized target and
configuration before asynchronous recovery. Later choices cannot retarget them.
Stop and input retries match their execution target and request; terminal controls retire
from the hot journal without blocking genuinely new controls.

Local cold recovery of the preserved Mac workspace retained 1,662 public chats
and 4,540 pending records through two opens, with no private aliases or recovery
error. The 24-turn real two-device smoke preserved 1,600 history rows through
offline edits, crashes and lost acknowledgements, including original-host outcome
readback, raw/foreign read denial and bounded RSS. Quiet-owner freshness beyond
45 seconds, all 212 Edge tests and 574 desktop tests passed. The actual headed
demo admitted duplicate Enter once, rendered its mock reply, and retired the
pending notice after authoritative applied readback without re-admission.
Local typechecks are intentionally skipped. Exact-source remote mobile gates,
live writer-gate cutover, staging publication and physical-device acceptance
remain with the integration owner.
The required native verification workflow checks generated Worker declarations
and TypeScript remotely before running the Edge/native convergence probes.
Worker fragment timers use concrete platform handle types; authorization fixtures
retain narrower capability cases without mutating generated literal bindings.

## Unreleased: workspace snapshot admission

Newer client snapshots cannot discard the workspace's retained causal history.
Returning offline edits and session memberships remain mergeable after a cold
restart; transcript-room retention is unchanged. Previously discarded dependencies
still require an explicit, backed-up recovery rather than an automatic reset.

Rejected staging recovery seeds report the received byte count, SHA-256,
validation stage, and a bounded error stack to the authenticated controller.
Invalid seeds still leave retained state unchanged; production responses remain
generic and neither environment returns seed contents or credentials.
Recovery-seed decoding uses the existing WASM exhaustion/recycle policy; rejected
requests never replace retained workspace history.
HTTP and WebSocket imports use that same policy instead of masking exhausted
WASM as an ordinary invalid update; staging logs retain a bounded error stack.
Creation recovery uses winning field edits, not a peer's unrelated later counter.
Disjoint incoming fields and explicit deletions survive repeated shallow recovery;
identity and owner checks precede causal shortcuts. Authority clocks are captured
before session-only schema migration, never by stamping a workspace as a session.
Unprovable provenance remains blocked with original intents retained.

Bounded owner registers validate immutable anchors, not changing heartbeat event
IDs. Coalesced creation activity retains one timestamp/preview publication across
independent seeds when all user-owned fields and row ownership match.

Latest native follow-up passed 152 document/sync tests and the rebuilt 24-turn
real collaboration smoke with all 1,600 history rows. Three awake quiet-owner
runs passed lease freshness and cold recovery; the headed demo admitted a new
mock turn and rendered its completed reply. Candidate `de43e527` passed Linux,
macOS and complete mobile gates in [run 37420927501](https://github.com/Ashler-AI/comet/actions/runs/37420927501),
including real simulator/native/Edge transport and the staging 1.0 (30) archive.
All nine downloaded checksums, exact bundle/build and production APNs entitlement
verified. The archive is ad-hoc, not Apple distribution or TestFlight availability;
no merge, channel publication or live-engine cutover occurred.

Public room identities are canonical UUIDs. Principal, project and deployment
boundaries remain exact; private execution keys never become discovery rows.
Legacy output migration checks provenance before publishing recovered messages.
Cold execution aliases accept authenticated OMP, Codex and Claude Code contexts.

The edge pins Loro 1.16.4 for lower-memory concurrent workspace imports; no
history boundary is advanced. A returning native writer's edit over the recovered
workspace used 104 MiB of WASM linear memory versus 143 MiB on 1.13.9.
Replay the same budget check in a fresh process with private corpus files:
`node edge/scripts/workspace-replay-memory-smoke.mjs BASELINE.loro DELTA.loro`.
The Worker loads Loro as a native compiled WebAssembly module rather than an
embedded base64 payload, avoiding its startup string and decoding buffers.
The immutable candidate includes the WASM module; no-bundle deployment attaches
the same bytes alongside the JavaScript entry point.

Mobile browse links retain their exact scope without attaching or resuming a
sandbox. Conflicting links fail before navigation or sends. Shared attachment
identities use consistent outbox accounting; terminal controls retain durable
outcomes without consuming pending-command capacity. Offline workspace goals
project before a network join. A trusted room reset retires only the previous
frontier, not the pending journal.

Writable sync and relay clients must declare `syncProtocol=durable-records-v1`.
Crew 0.1.135 is retired for writes: legacy room clients retain authenticated
read-only backfill, while host/control registration and durable writes require
an update. The declaration negotiates compatibility, never authentication.
Owner heartbeats use the bounded `agentSessions` register; immutable phase
anchors remain. Release gates cover real legacy-write rejection, current-client
crash recovery, quiet-owner freshness and simulator/native/Edge transport.

Current cutover checks: 468 native/RPC library tests, 26 native integration tests
and eight relay integration tests passed, with existing external/paid-provider
tests explicitly ignored. All 206 Edge tests passed. The unchanged 24-turn,
300 KiB progressive-tool probe passed at 52.7 MiB RSS growth against 128 MiB.
SQLite layout migration avoids full-blob temporaries; standard LZ4 frame sizing
bounds compression scratch. The rebuilt 0.1.141 headless binary completed the
full 24-turn convergence scenario, preserving 1,600 history rows through crash
and independent epoch replacement, with 9.6 MiB RSS growth. Accepted Stop also
retires queued recovery so no resumed run can start after its acknowledgment.
The rebuilt headed demo accepted a new mock turn and rendered its complete reply. Remote memory
gates passed on Linux and macOS; the simulator passed all 27 recovery fixtures.
Real simulator/native/Edge transport and the complete staging simulator/archive
gate passed in [mobile run 37373585650](https://github.com/Ashler-AI/comet/actions/runs/37373585650)
from `70e90dc1`. Archive signing is ad-hoc, not Apple distribution; no new
TestFlight upload or staging/production release is claimed for that earlier
candidate. Updated-client rollout is authorized, but existing-engine work has
resumed; merging, writer-gate activation and restarts wait for a fresh coordinated
checkpoint. Candidate verification proceeds independently without an execution hold.
Local typechecks are intentionally skipped to preserve workstation resources.

## Crew Staging mobile 1.0 (29)

Crew Staging **1.0 (29)** is available in the existing **Ashler Internal** TestFlight
group from merged source `a074ebdbf28111504ec211a9c61ee5bd805d036a`.
[Main CI 37140082063](https://github.com/Ashler-AI/comet/actions/runs/37140082063)
passed all 21 recovery markers and archived the device build; settled screenshots
verify persistent blocked-recovery feedback/manual retry and the Unreachable strip.
The exact uploaded IPA passed strict distribution-signature and APNs checks, and
authenticated Apple readback showed Testing. See
[mobile release evidence](apps/ios/README.md#crew-staging-10-29-durable-recovery-release-evidence).
No production upload, tester/account changes or public App Store submission occurred.
Physical-phone acceptance remains manual; export/upload did not compile locally.

## Crew 0.1.136 release candidate: canonical recovered rooms

Recovered execution journals keep their private execution keys, but resolve to
the owned canonical session UUID before selecting a transcript writer or room.
Lost in-memory aliases no longer create `UUID::session::UUID` documents or dial
permanent-404 room addresses. Completed private output is reconciled by stable
message ID; canonical metadata and terminal outcomes are retained, private
snapshots remain backed up, and pending commands are not copied or executed.

An existing running writer is not retargeted by Take over: that operation can
retry an interrupted request. Finish or checkpoint current work before a
controlled host upgrade; do not use takeover to repair a live transcript view.
Divergent private/canonical message identity or content fails visibly without
discarding either copy.

Completion activity updates only timestamp fields, so a concurrent user rename
is not erased by a stale full-row write. Replies to verified same-device sessions
stay local when unscoped, rather than depending on an available Edge relay.

All 345 document/engine unit tests passed. An isolated actual headless runtime
recovered stranded output after restart, retained the private backup, omitted
its pending command, and completed a distinct public turn without replaying the
original request. The headed demo rendered the recovered history and completed
one new turn; its original user message and follow-up each appeared exactly once.
The peer-message, two-engine convergence, and restart suites passed 18 tests;
one authenticated paid-provider test remains intentionally ignored. The actual
native CLI smoke verified immutable retries, correlated replies, and restart
recovery with an offline Edge endpoint.
No installed engine was replaced; local typechecks were intentionally skipped.
## Unreleased: durable scoped recovery

Application edits and command outcomes are journaled before acknowledgement,
independently of replaceable Loro caches. Causal checkpoint restoration does not
replay edits already present in newer history; genuinely conflicting fields keep
their original records. Cold native journals reconcile with authoritative room
identities before uploading reconstructed list entries.

Public room identities are canonical UUIDs. Principal, project and deployment
boundaries remain exact; private execution keys never become discovery rows.
Legacy output migration checks provenance before publishing recovered messages.
Cold execution aliases accept authenticated OMP, Codex and Claude Code contexts.

Mobile browse links retain their exact scope without attaching or resuming a
sandbox. Conflicting links fail before navigation or sends. Shared attachment
identities use consistent outbox accounting; terminal controls retain durable
outcomes without consuming pending-command capacity. Offline workspace goals
project before a network join. A trusted room reset retires only the previous
frontier, not the pending journal.

The release gates cover paced large-tool memory, failure recovery, both supported
mixed-native directions and a real simulator/native/Edge transport path. Native
publication heartbeats remain while the pre-register baseline is supported;
that compatibility history can be removed when the baseline retires.

Local checks: 433 native library tests and 194 Edge tests passed. The unchanged
24-turn, 300 KiB progressive-tool probe passed at 60.2 MiB RSS growth against
128 MiB. The full native convergence scenario and new mobile source still await
their final integrated gates; this source is not a staging or production release.
Local typechecks are intentionally skipped to preserve workstation resources.

## Crew 0.1.135 release candidate: network catch-up

Crew waits for a room's snapshot and update journal to finish catching up before
reporting it connected. Valid intermediate backfill frames no longer request
another full resync, which could keep an active room in a reconnect loop after
a brief internet outage. Corrupt imports and gaps discovered on synchronized
connections retain bounded full-backfill recovery; local writes are preserved.

The new regression failed before the correction; all 42 sync tests passed after it.
An isolated live WebSocket smoke recovered from connection loss, consumed a streamed
snapshot and journal, and converged an offline draft with zero full resyncs. The
headed demo loaded its mock transcript; installed clients were not replaced with
development binaries. Local typechecks were intentionally skipped.

## Crew 0.1.134 release candidate

Workspace persistence and edge folding retain available causal history rather
than independently replacing each replica with a state-only snapshot. Desktop
and Devbox edits made while disconnected can therefore merge after both hosts
save and restart. Session-document retention is unchanged.

This keeps more workspace history in exchange for preserving offline edits.
Previously discarded dependencies cannot be recreated by reconnecting or by
this upgrade; divergent existing replicas require a backed-up, explicit recovery.
Deploy the matching edge before upgrading controllers and hosts.

The candidate also publishes ordinary remote-session status through existing
session rooms, so Devbox activity indicators do not depend on workspace backfill.
Canonical controls preserve an existing bare-chat writer's live execution key.

The local headed 0.1.134 demo admitted a mock prompt, displayed the working strip
and Stop affordance, and returned to idle. Focused native regressions and the
38-test edge room authorization suite passed. The required opposite-provider
review attempt could not start: this repository lacks
`skills/local-code-review/scripts/opencodereview.mjs` (`MODULE_NOT_FOUND`). Local
typechecks were intentionally skipped; remote CI retains its required checks.
Production promotion remains gated on captain verification of restarted staging.

## Crew 0.1.133 release candidate

The 0.1.133 candidate introduced native state-only persistence before joining
and edge workspace shallow compaction under the session-document retention
policy. It preserves visible rows, but independent offline frontiers can lose
the dependencies required to merge. The 0.1.134 correction above removes
that unsafe workspace compaction; transcripts remain independently durable in
their session documents.

The retained staging workspace shrinks from 15,359,379 bytes to 2,272,109 bytes
while preserving all 1,577 chat rows and 1,465 session rows. The Scaffold runtime
contract stays `scaffold.comet-runtime.v1`.
The local headed demo built 0.1.133, admitted a mock workspace-compaction
smoke prompt, and rendered the streamed response.
Opposite-provider review could not start because this repository has no checked-in
review launcher. Local typechecks were intentionally skipped.

## Crew 0.1.132 release candidate

Archived remote sessions no longer retain background room observers, so active
Devbox transcripts and status updates are not starved by old session history.
Crew-owned Namespace forwards run in an isolated process group; their 30-minute
lease now stops helper subprocesses as well as the top-level `devbox` command.

This candidate includes the mobile shallow-cache recovery in
[PR #56](https://github.com/Ashler-AI/comet/pull/56) and the Devbox streaming/forward
fixes in [PR #58](https://github.com/Ashler-AI/comet/pull/58). Mobile candidates are
Crew Staging **1.0 (28)** and Crew **1.0 (21)**. The Scaffold runtime contract stays
`scaffold.comet-runtime.v1`; both Scaffold image lanes must pin the verified release's
version, private bucket, and Linux x86_64 digest together.

The local headed demo built 0.1.132, admitted a mock release-smoke prompt, and rendered
its streamed response. Desktop production promotion and both Scaffold pin changes
remain gated on captain verification of the restarted staging desktop app. Inference
review could not start because this repository lacks the checked-in opposite-provider
launcher; local typechecks were intentionally skipped.

Release verification dropped a redundant callback-RPC-count test that assumed a
background tunnel request ran before message admission. The failed-tunnel and
hung-tunnel regressions still verify that forwarding does not block sends.

Crew **0.1.132** staging published from `4346d8a3612de0e988998a66e7eec25bff83c2c6`
in [release run 36765542374](https://github.com/Ashler-AI/comet/actions/runs/36765542374).
The run verified the signed macOS candidate and read back the desktop,
desktop-staging, and Scaffold staging channels. Production must reuse this exact
`candidate_run_id` after captain approval; no production publication or Scaffold
image-pin changes have been performed. Crew Staging mobile **1.0 (28)** is available
in the existing Ashler Internal TestFlight group; production mobile **1.0 (21)**
is verified and prepared but not uploaded. See
[mobile publication evidence](apps/ios/README.md#crew-01132-mobile-release-evidence).
Downloaded macOS distributions also passed checksums, strict same-team signatures,
stapler and Gatekeeper checks, with matching DMG/updater bundles. Candidate SHA-256:
`952d02d9ac401398dd439d56ad12340fa038d3af5a8a2bc1ee972133e0bbdcfc`.

## Crew 0.1.119: Namespace Devboxes

Namespace hosts advertise a **Devbox** environment separately from their Linux OS
and execution authority. Session rows, transcripts, folders, and device settings
show Devbox rather than Local; ordinary personal hosts and Scaffold keep their labels.

Crew protects active turns from Namespace auto-stop with owned files in
`/.namespace/tasks`. Completion, cancellation, failed starts, and orderly engine
shutdown remove those markers. Parked idle harnesses do not keep the machine awake.
Other engines' or independent jobs' markers are never cleared. After SIGKILL or a
machine crash, restart Crew to clean confirmed stale Crew markers; never delete all
task markers indiscriminately. Independent background jobs that outlive a turn need
their own marker and cleanup trap.

Use the [Namespace Devbox setup skill](skills/namespace-devbox/SKILL.md) for an
owner-named 16-CPU/64-GiB machine, Chromium by default, the Ashler kind/Tilt stack,
and one supervised callback forward for all initial browser logins. After setup,
Crew wakes the machine and runs coding sessions through its device relay; no local
coordinating agent or credential copying is required.

## Crew 0.1.117: ordinary remote device control

Desktop clients can start, steer, answer, and stop sessions on an ordinary
`comet headless` device signed into the same account and project. These sessions
were labelled **Local** in 0.1.117, including before the first agent publication;
the Namespace metadata in 0.1.119 distinguishes Devboxes. Only explicit
Scaffold sessions use the Scaffold label.

Remote commands use fresh, one-shot authenticated device-relay admission scoped
to the host and chat. The host records immutable command provenance; copied or
forged commands in the shared document cannot confer execution authority.
Exact retries retain their receipt through the original command lifetime without
renewing authority. Offline hosts fail admission explicitly.

Deploy the matching edge before updating clients and hosts, and reconnect
ordinary hosts after deployment. Scaffold lifecycle and grant checks are unchanged.
The two-engine collaboration smoke covers ordinary remote legacy and typed Local
start/response/steer/stop, foreign-principal denial, and existing Scaffold revocation.

## Unreleased: ordinary remote activity

Ordinary remote hosts, including Namespace Devboxes, publish owned status and
heartbeats through their existing session rooms. Controllers merge that activity
into `WatchSessions` independently of workspace backfill, keeping sidebar and
composer indicators aligned when workspace synchronization is stalled. Local
engine status still wins for locally hosted sessions. Membership and owner checks
fence remote activity; static streaming snapshots do not count as fresh heartbeats.

Canonical session controls keep an existing bare-chat writer's live execution key,
so publishing its room record does not strand Stop, steering, or input answers.
Upgrade both controller and host; already-running older binaries do not acquire
this behavior from a desktop-only update. This change does not reset or repair
previously divergent workspace histories.

## Unreleased: shared session discovery

`comet session search --query "upload retries"` searches generated and display
titles. `--source-url` accepts a Slack message/thread or Notion page link;
`--owner-id`, `--limit` (1–50), and `--cursor` narrow or page the same search.
Results contain title, owner, matched sources, and a credential-free Crew link,
never transcript contents or an assertion that the owner is currently working.
Opening a result preserves its project and deployment scope and requires sign-in.

Completed turns queue discovery metadata updates. Automatic title generation uses
fixed-model `gpt-5.6-luna` through Agent Auth, independently of the conversation
harness, only when the transcript projection counts one cleanly completed agent
turn. Later turns normally refresh metadata without regenerating the title; the
accepted transcript-order and older-session limitations are documented at the
gate and projection boundary in [titles.rs](crates/engine/src/titles.rs).
Manual and legacy titles survive refresh; generated titles update sidebar
metadata, not existing branch names.
The first prompt still names a new managed branch once. The durable local outbox
coalesces snapshot metadata to a 30-second maximum scheduling delay; completed
turns get priority, but model calls are limited to one per session per minute.
One background worker bounds concurrent inference; retry deadlines survive restart.
Completed snapshots persist their priority atomically. Each claim consumes that
priority so failed jobs cannot repeatedly jump the backlog; retry deadlines still
apply. Jobs queued before this upgrade retain ordinary scheduling until their next
completed turn.
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
it with `node scripts/sync-omp-model-catalog.mjs <platform commit SHA>`; unknown
source formats fail regeneration. Released `claude-opus-5-5` uses the confirmed
`low`, `medium`, `high`, `xhigh`, `max` effort ladder. New selections without a
saved effort start at `medium`; valid saved or explicit choices are preserved.
Claude Code's existing Fable 5.1 default is unchanged.

Released `gpt-6-sol` and `gpt-6-luna` are available in the Codex, OMP/Scaffold,
Prime Agent, and mobile catalogs. Codex defaults and implicit legacy Sol defaults
now select `gpt-6-sol` at `high` effort; explicit saved models and efforts remain
unchanged. Astra and Anthropic defaults are unchanged. Both new models support
1,050,000 context tokens, 922,000 maximum input tokens, and 128,000 output tokens.
Crew exposes `low`, `medium`, `high`, `xhigh`, and `max`; provider `none` is not
represented in Crew's shared effort enum. Tool-bearing reasoning uses the existing
Responses API routes, not Chat Completions. The OMP/Prime gateway records the base
per-million input/cache-read/output rates ($2/$0.20/$10 for Sol,
$0.10/$0.01/$0.50 for Luna); requests above 272K input tokens incur the provider's
2x input/cache and 1.5x output multipliers.

`gpt-6.1-sol` is also selectable in Codex, OMP/Scaffold, Prime Agent, and mobile
catalogs with the same context/output limits and `low`–`max` efforts; Crew's
existing defaults remain unchanged. Its base input/cached-input/output rates
are $2/$0.10/$10 per million tokens.

The Crew OMP/Prime gateway registers Opus 5.5 with 1,000,000 context tokens,
128,000 output tokens, and per-million-token costs of $4 input, $20 output,
$0.20 cache read, and $5 for 5-minute cache writes. The provider uses adaptive
thinking with `display: "summarized"`, so progress arrives in the existing
reasoning stream. Signed thinking and conversation history are not rewritten.
Forced `tool_choice` is preserved for an upstream error, never changed to `auto`.
Keep conversations append-only; editing old turns or changing the system prompt
or tools can invalidate signed thinking. Direct user-owned OMP provider overrides
remain authoritative and require their own compatible model configuration.

Native Claude Code owns its Messages transport and signed history; Crew passes
the exact model ID and supported `--effort` flag, rather than inventing transport
flags. Opus 5.5 has native 1M context and always-on adaptive thinking, so Crew
does not offer its old context/thinking toggles or unverified CLI options.
Use a current Claude Code supporting this release. See the
[official model overview](https://platform.claude.com/docs/en/models/opus-5-5/overview)
and [migration guide](https://platform.claude.com/docs/en/models/opus-5-5/migration-guide).

The focused transport smoke runs the installed OMP against a credential-free
loopback server, exercises a real read tool, and verifies signed thinking replay:
`node scripts/omp-gateway-revival-smoke.mjs /path/to/omp --opus-only`.

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

The composer’s working timer measures the most recent turn, not the age of the
session. Shared session publications carry `startedAt`; heartbeats and input
resolution preserve it, and a new turn resets it. Desktop and iOS consume that
turn start. Accurate shared-session timing requires an updated owner runtime.

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

### Model-stream recovery

Crew-owned OMP runs allow three model-level retries with OMP's bounded backoff;
provider/SDK HTTP retries remain disabled. Completed tools and the user task are
not replayed by Crew. Existing OMP replay-safe stream repair remains unchanged.
Desktop, iOS, and web show **Reconnecting to model — attempt 2/4** during recovery
and clear the indicator on resumed progress, interruption, or terminal completion.
Retry counters are live session metadata, not transcript errors or reasoning text.

Inference diagnostics join Crew's `request_id` to the server's `requestId` using
`x-agent-auth-request-id`, separately from OpenAI's `x-request-id`. IDs are bounded
and credential-like values are omitted; transport causes are classified without
logging raw exception messages, prompts, or credentials. Partial HTTP 200 streams
remain errors, and inference timeout behavior is unchanged.

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

Starting with **0.1.137**, recover a failed import into its preserved target:

```bash
"$COMET_EXECUTABLE" session handoff "$COMET_SESSION_ID" \
  --prompt-file "$PROMPT_FILE" --database-environment local \
  --recover-chat-id "$TARGET_CHAT_ID" --recover-sandbox-id "$TARGET_SANDBOX_ID"
```

Both target flags are required. Recovery uses a separate RPC so older engines
refuse rather than silently creating another sandbox. The accepted owner,
project/deployment/session, sandbox, database and agent route must still match.
**0.1.139** validates the requested route from the owner-scoped sandbox's stored
`agentRoute`, not a post-inference account-attribution receipt. Missing or changed
routes fail closed; an unused or paused sandbox needs no prior inference receipt.
Imported native context, admitted commands and active runs refuse recovery;
read-only peer diagnostics do not. A bounded native-ID probe checks the active
profile before any worktree replacement, including renamed journals and lost
import responses. Incomplete inspection fails closed. Preparation and initial
command admission share the same scope gate, including ordinary Start/Run RPCs.

Starting with **0.1.140**, pause, resume and stop requests have a five-minute
deadline for VM placement and runtime bootstrap. Metadata requests and connection
establishment retain their 30-second limits. Cancellation interrupts both the
request and an incomplete response body; longer lifecycle waits never enable a
creation fallback or relax ownership, source, database or route checks.

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
30-second metadata-request deadline. Cancellation still stops an in-flight upload.
The worktree and native-context verifiers run from temporary sandbox files rather
than exceeding the runtime's 16 KiB exec-argument limit with inline program text.
The reconstructed checkout has a separate 1 GiB byte budget; compressed Git
objects and archive bytes are checked independently before checkout publication.
Files and symlinks have a 25,000-item budget; real directories have a separate
25,000-item budget. Paths, symlinks, object expansion and history remain bounded.
Recovery's read-only journal scan checks at most 4,096 entries, 64 KiB per header
and 16 MiB aggregate header bytes within a 10-second exec deadline.

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

## Local Rust builds

Rust builds stay local. Use the standard-library runner rather than invoking Cargo
directly from development scripts:

```bash
export ASHLER_INCREMENTAL_TSC_CHECKS=false
python3 scripts/local-cargo.py build -p comet --bin comet
target_dir="$(python3 scripts/local-cargo.py --print-target-dir)"
"$target_dir/debug/comet"
scripts/dev-demo.sh --slow
```

Local Cargo commands share the main checkout's `target` cache across worktrees
(resolved from the absolute Git common directory). `CARGO_TARGET_DIR` overrides
that default; relative paths resolve against this checkout. Cargo's `--target-dir`
takes precedence. The runner caps jobs at two (one is allowed), sets scheduling
priority to at least nice 10, and serializes commands through the per-user
`~/.cache/crew/cargo-build.lock`, even for different target directories. Cargo exit
statuses and termination signals are preserved. `--print-target-dir` never builds
or waits for that lock. Native Crew sessions (`COMET_LOCAL_AGENT_RUNTIME=1`) always
use the local policy, even though their shell sets `CI=true`. Outside that runtime,
truthy `CI` (other than `false`/`0`) keeps checkout-local cache defaults and skips
the runner's job, priority, and gate restrictions.

The demo builds Crew and `rpc_probe` once, snapshots both executables before
releasing the build gate, then launches them directly; the running UI and RPCs
do not hold the gate. Packaging similarly snapshots its executable under the gate,
then creates artifacts in this worktree's `target/package`. macOS staging uses a
separate `staging` subtree of the selected compilation cache. The repeatable
`--copy-binary RELATIVE_TARGET_PATH DEST` runner option provides that snapshot.
Build then execute a binary directly for long-lived apps; manual `cargo run`
through the runner retains native Cargo behavior and holds the command gate.

On the configured workstation, `~/.cargo/config.toml` also caps default jobs and
uses a low-priority, single-compiler wrapper with its own lock, protecting raw
Cargo invocations outside these scripts. Those user-local settings are not
repository configuration. Never run local typechecks; keep
`ASHLER_INCREMENTAL_TSC_CHECKS=false`, including for Git operations.

Small offline regression (a disposable real Rust crate, not a desktop rebuild):
`python3 scripts/test_local_cargo.py`. Packaging preflight without compiling,
signing, or notarizing: `node --test scripts/package-macos.test.mjs`.
Where the user-local compiler guard is installed, check it without compiling:
`python3 ~/.cargo/test-crew-rustc-wrapper.py`.

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
