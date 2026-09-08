# Crew for iOS

A native SwiftUI viewport onto the comet-native mesh. The phone is a **peer
device**: it joins the same Loro CRDT rooms as every other device (workspace
doc + per-chat session docs over the edge's Durable Objects), renders the
mirrors, and drives remote engines through the durable command queue. No
engine runs on the phone.

## Build & run

Requires Xcode 26+ (iOS 26 SDK — Liquid Glass APIs).

```sh
cd apps/ios
xcodebuild -project Comet.xcodeproj -scheme Comet \
  -destination 'platform=iOS Simulator,name=iPhone 17 Pro' build
```

Or open `Comet.xcodeproj` in Xcode and run. Dependencies (SPM, resolved
automatically): [loro-swift 1.13.x](https://github.com/loro-dev/loro-swift)
(matches the engine's loro 1.13), [swift-markdown](https://github.com/swiftlang/swift-markdown)
(cmark-gfm: tables/strikethrough/tasklists — the same feature set as the
desktop's pulldown-cmark config).

Production and staging use the same Swift target with separate checked-in schemes,
bundle IDs, persisted state, credentials, invite schemes, and cloud endpoints:

```sh
# Production: Crew, ai.ashler.crew, version 1.0 build 6
xcodebuild -project Comet.xcodeproj -scheme Comet \
  -destination 'platform=iOS Simulator,name=Crew Mobile Parity' build

# Staging: Crew Staging, ai.ashler.crew.staging, version 1.0 build 12
xcodebuild -project Comet.xcodeproj -scheme 'Crew Staging' \
  -destination 'platform=iOS Simulator,name=Crew Mobile Parity' build
```

| Scheme | Edge | Scaffold | Project scope | Invite scheme |
| --- | --- | --- | --- | --- |
| `Comet` | `comet.internal.ashler.com` | `scaffold.internal.ashler.com` | `ashler-production` | `comet://` |
| `Crew Staging` | `comet-staging.internal.ashler.com` | `scaffold-staging.internal.ashler.com` | `ashler-staging` | `comet-staging://` |

Performance changes shared by both builds are documented in
[`docs/memory-plan.md`](../../docs/memory-plan.md#9-crew-0167-performance-audit-2026-09-05):
single-flight reconnects with durable resubmission, coalesced snapshot saves,
workspace-cache retention, and bounded parse-cache lifetime. Unicode numeric
highlighting always advances, and code recoloring no longer copies each token's
entire line prefix.

The 2026-09-05 release uploaded production **1.0 (3)** and staging **1.0 (2)**
to TestFlight. App Store Connect processed both and assigned them to the existing
**Ashler Internal** group. The staging tester's installation of build 2 was
confirmed; the production invitation was resent and remains awaiting acceptance.
These are internal TestFlight releases, not public App Store submissions.

Staging **1.0 (5)** adds compacted-history recovery and host-discovered harnesses.
The model picker exposes Claude Code, Codex, OMP, and Prime Agent when advertised
by the selected host, with lazy model rows and search. Catalog failures remain
visible and retryable; Scaffold retains its OMP-only launch contract.

A stale cache must not treat an import with pending dependencies as successful.
Recovery validates a fresh server snapshot, preserves recoverable offline edits,
and atomically switches the room, projection, and disk saver to the recovered
replica. Divergent offline conflicts or unsupported container changes retain the
old data and surface a synchronization error rather than silently overwriting it.
App Store Connect confirmed build 5 in **Ashler Internal** and its installation
by the existing tester on 2026-09-06.

### Snapshot-first bootstrap recovery

Native and mobile clients send a snapshot when the edge has no history. For
fragmented catch-up, they prefer a snapshot only when it is smaller than the
missing-operation export; normal incremental updates stay unchanged.

The edge loads a snapshot into a fresh candidate before replaying persisted and
buffered deltas. Adoption requires complete replay, coverage of accepted history,
and matching state/oplog frontiers. Already-included snapshots are acknowledged
without re-import; concurrent snapshots require a rejoin/resync before submission.
Missing dependencies reject the candidate without replacing accepted state. The
incoming covering snapshot is stored through the chunked blob store, with the
existing delta log and buffered deltas retained for cold replay. No merged-snapshot
re-export is needed at adoption. Notification policy is unchanged.

Cold materialization is single-flight: simultaneous joins share one replay and
cannot spend the replay-crash budget several times or overwrite each other's
materialized documents. Four simultaneous cold reads previously reproduced an
automatic snapshot/log drop without a preceding replay failure. The serialized
path preserves all 604 session entries in the same Cloudflare runtime scenario.

The staging safeguards were deployed on 2026-09-07 as
`c8be5b20-9eba-43f4-9b74-fda0eb8a4e6a`; removal of the redundant adoption export
was deployed as `e907dc7e-9d77-4535-b6f9-9271ff2d6bf2`.

A version-attributed workerd comparison used the same real workspace plus 499
synthetic dependent deltas. When the incoming snapshot already included those
deltas, both implementations used 28,966,912 bytes (27.63 MiB) through bootstrap
and observation, and 75,759,616 bytes (72.25 MiB) after cold-replay trimming.
Both preserved 595 valid notification rows and emitted zero attention events.
Removing the full-snapshot export avoided redundant work but did not lower that
fixture's WASM high-water mark. The earlier 499-row captured-live-log experiment
was a different fixture and is not the basis for these version-specific numbers.

The missing-history fixture exposed a separate `importBatch` problem in Loro:
batch replay detaches the document and checks out the latest operation log at
the end. With 499 deltas absent from the incoming snapshot, `e907dc7e` reached
240,189,440 bytes through observation and failed the memory assertion.

Staging `bcf5b1cd-3773-441f-9a45-adaa8bf8bcb7` replaces batch replay with streamed
single-delta imports. A final applied-version check covers every earlier pending
span, so later successful rows cannot hide unresolved dependencies. No retained
rows are dropped. In a matched-byte workerd API experiment, batch import reached
236,388,352 bytes; single imports applied all 499 missing deltas and read all 604
sessions at 70,320,128 bytes.

The actual edge bootstrap, observer, and cold-replay paths also passed using the
real workspace plus 499 genuinely new dependent deltas: 73,793,536 bytes (70.38 MiB)
through observation and 76,152,832 bytes (72.63 MiB) after cold-replay trimming.
Reverse-order replay passed at 70.38 MiB through observation and 72.38 MiB after
trimming, including resolution of the accumulated pending chain. Both preserved
the final delta value and 595 notification rows, and emitted zero attention events.
These are isolated scenario measurements, not a bound for arbitrary payloads or
multiple co-resident rooms. Old clients' full-history update uploads remain a
separate activation concern.

The complete edge Vitest suite passed: 136 tests across 16 files. The later
[staging deployment](https://github.com/Ashler-AI/comet/actions/runs/34165594033)
passed remote typechecking, tests, and the real Edge/Rust collaboration smoke;
it deployed `b683ff3e-8af0-4d1d-a80d-7a840df95ef1`. Local typechecks remain
intentionally disabled under workstation policy.

Desktop **0.1.75** was published to staging and installed. After explicit user
approval to interrupt active runs, the old **0.1.70** engine exited on
2026-09-07 at 22:39:51 UTC. The new engine reported **0.1.75**, restored the
same device identity, and resumed the existing native OMP conversation.
At 22:41:46 UTC, workspace sync was connected with a fresh acknowledgement and
zero rejoins, full resyncs, or disconnects since startup. This verifies workspace
recovery, not every chat room: one open chat reported three rejoins.

Staging then recorded six APNs HTTP **200** responses around 22:40 UTC, including
receipt `85E8F1FA-4390-4168-5184-745C1FCA5265` for event timestamp
`1788820786445`. These were recovery-time session transitions, not an additional
manual test notification. Physical iPhone display and tap routing remain
unverified; provider acceptance is not a delivery receipt from the device.

Staging iOS **1.0 (11)** was uploaded on 2026-09-07 with the snapshot catch-up
changes. Its archive and cloud-signed inspection IPA both contain
`aps-environment = production`; the IPA passed strict signature verification
before upload. Build **11** finished processing and was assigned to Ashler
Internal. Build **10** lost its push entitlement during unsigned-archive cloud
signing; it was expired in App Store Connect with user approval.

When archiving with `CODE_SIGNING_ALLOWED=NO` for cloud-managed distribution,
first ad-hoc sign the archived app with the expanded Release entitlement
(`aps-environment = production`), as builds 9 and 11 did. Export for inspection
before upload, inspect the signed IPA with `codesign -d --entitlements :-`, and
verify the signature with `codesign --verify --deep --strict`. A push-enabled
provisioning profile alone does not add the entitlement to an unsigned archive.

For physical-device verification, correlate staging `apns_finished` with the
attention event's `updatedAt`. The trace includes the provider HTTP `status`,
the response `apns-id` as `apnsId` when present, and `removeRegistration`.
`apns_response` records the same status and receipt before parsing a rejection
body. A 200 establishes APNs acceptance, not display or tap delivery on the
iPhone. Transport failures remain `delivery_exception`, not an acceptance.

### Staging 1.0 (6): mobile status feedback

- Session rows show unread, live status, and last-updated time independently.
  Local and shared sessions combine owner status with current-turn transcript
  activity; inferred liveness does not refresh the displayed timestamp.
- Pending sends show `Sending…` immediately. Host admission hands off to the
  processing spinner and elapsed time, including before the first response
  frame. Completion clears processing; rejected sends clear sending feedback
  and preserve the draft for retry.

Uploaded to **Ashler Internal** on 2026-09-06. App Store Connect confirmed
installation of **1.0 (6)** on the existing tester's iPhone 13 Pro. Verification
covered 19 native simulator checks, visible sending/processing/terminal states,
and a fixture-free `Release-Staging` build before the device archive and upload.

### Coordinated Crew 0.1.71 release

Production **1.0 (4)** and staging **1.0 (7)** were archived, cloud-signed, and
uploaded on 2026-09-06 from the same source as Crew 0.1.71. App Store Connect
processed both builds and assigned them to **Ashler Internal**. The production
tester remains **Invited**; neither new build's physical-device installation
is claimed. Staging build 7 preserves the previously distributed build 6 fixes.

The fresh simulator build rendered the Crew login surface. Authenticated mobile
verification awaits the system sign-in consent; two independent native Crew
engines separately verified live Scaffold messaging and reconnect convergence.
Typechecks were intentionally not run because global instructions prohibit them.

### Crew 0.1.72 / production iOS 1.0 (5)

The release worktree ports the staging build 8 session-visibility and notification
source onto the current production baseline. Desktop **0.1.72** was promoted from
candidate run [34135975355](https://github.com/Ashler-AI/comet/actions/runs/34135975355)
by [34136878825](https://github.com/Ashler-AI/comet/actions/runs/34136878825), which
verified byte-identical artifacts and production channel readback. Scaffold runtime
deployment was unchanged. Staging remains **1.0 (8)**.

The initial Debug production-review simulator build **5** crashed after its
regression logged success: logout DELETE started after regression teardown
invalidated the probe's ephemeral URLSession. This was **Debug-harness-only**:
Release uses `URLSession.shared`, never invalidates it, and excludes the harness.
The faulting request method is also used in Release, but the invalidated-session
condition is not reachable there. `signOut()` returns optional cleanup work so
the regression can drain it without delaying local logout. The original upload
was cancelled during analysis after transferring only its asset-description XML;
it did not complete an IPA upload. The subsequent rebuilt archive is the one
uploaded successfully; no shipping crash is inferred from the Debug incident.

The fresh archive, built from crash fix `ed34d79`, uploaded successfully on
2026-09-07 at **15:23:50 UTC** as production **1.0 (5)**. Xcode reported the
package processing, then upload success with no errors or warnings; delivery ID
`ce659ad3-9bcc-4b1f-ae8c-ecf8a5536ffc`. The inspection IPA passed signature
verification, and both its signed entitlements and embedded profile specify
`aps-environment = production` for `825LYXGJR6.ai.ashler.crew`.
Authenticated App Store Connect inspection on 2026-09-07 confirmed build **5**
upload **Complete** at **11:23 AM**, with access assigned to **Ashler Internal**.
That group's one internal tester remains **Invited**, with no install/session
recorded. No physical-device installation or notification delivery is claimed.
Production notification code was deployed through the staging-verified workflow
after direct integration into main. Background APNs delivery remains blocked on
approved production credentials; code deployment alone does not enable pushes.
The temporary manual typecheck-bypass input was removed after review. Both push
and manual deployment candidates again require the TypeScript check; agent policy
does not change the release pipeline's requirements.

The release workflow verified the desktop production manifest, checksums, and
latest pointer. Local `comet update --check` returned HTTP 401 without a current
production login, so authenticated client download was not verified locally.
Typechecks were intentionally not run because global instructions prohibit them.

Source reconciliation verified that fetched `origin/main` (`f89dd43`) is an
ancestor of the production release branch, which initially held exactly three
additional release commits. The stale local `main` checkout was not its baseline.
The previously published release branch was merged directly into main as
`4a1a398` on 2026-09-07; PR #16 was automatically marked merged. The workflow
fetches main explicitly and checks dispatch/tag ancestry against `FETCH_HEAD`,
including candidate-reuse dispatches. Published 0.1.72 artifacts are immutable.

`node --test scripts/release-workflow.test.mjs` passed **22 tests**. The new
behavioral cases execute the actual version/reuse shell blocks with temporary
single-branch Git clones and tar archives: no `origin/main` tracking ref, merged
and unmerged source, invalid/nonproduction reuse IDs, GitHub compare outcomes,
unsuccessful/foreign-workflow source runs, outer archive corruption, and separate
desktop/Scaffold/unified checksum failures.

The full `cargo test -p comet-ui` suite passed **531 tests** with zero failures;
the production port changed no comet-doc or comet-engine source files. A fresh
`scripts/package-macos.sh` bundle generated an actual `awaitingInput` transition,
but macOS displayed its Crew permission notice rather than an attention banner.
Permission was not granted automatically; real banner delivery and click routing
remain unverified until the user allows Crew notifications.

After direct integration, builds and tests ran from `/tmp/crew-notifications-main`:
**825 Rust tests passed**, with three existing environment-dependent tests ignored;
**132 edge tests** and **22 release tests** passed. `package-macos.sh` produced
Crew **0.1.72**, and the bundle passed signature verification. A fresh production
iOS **1.0 (5)** archive exported successfully; the signed IPA and embedded profile
both specify production APNs. This merged-source archive was not re-uploaded over
the already accepted TestFlight build 5.

Main commit `d8e1239` deployed through
[34141398021](https://github.com/Ashler-AI/comet/actions/runs/34141398021).
The real Edge/Rust collaboration smoke, binding freshness, and edge tests passed;
Typecheck was explicitly skipped. The workflow promoted the byte-identical staging
candidate with digest `fab2ce12da3cd918dd2f11b4c0953e6ef569fde80646171b0c0ba9d44a085620`.
Production Worker version: `5f344996-6aec-4b27-81de-01a96f9b4f65`.
Both staging and production `/health` returned `ok: true` after deployment.
The existing 0.1.72 desktop candidate was promoted from merged main by
[34141507706](https://github.com/Ashler-AI/comet/actions/runs/34141507706), with
all publish/readback jobs successful; no released artifact bytes were replaced.
The `comet-release-production` environment currently has no configured protection
rules; the workflow boundary was retained, but no manual reviewer approval is claimed.
This successful run is not evidence of reviewer approval or TypeScript-check
completion. The subsequent pipeline restoration was not followed by another
deployment; no new workflow was started to evade the no-typecheck agent rule.

The bounded Debug build was installed and exercised with `-visibility-e2e`:
the lifecycle completed with `drained logout DELETE`. Both missing-DELETE and
missing-token fault injections emitted the expected deadline `FAIL` after about
five seconds, with no lifecycle success marker. The supervised processes stayed
alive after those markers until intentional shutdown. DiagnosticReports still
contained only the two pre-check Comet reports; no new Comet `.ips` appeared.

Both the bounded `xcodebuild` run and `package-macos.sh` ran with cwd
`/tmp/crew-production-notifications-release`. Simulator derived data was
`/tmp/crew-production-notifications-build5/SimulatorDerivedData`; its recorded
`WorkspacePath` points to that release worktree's `apps/ios/Comet.xcodeproj`.
The bounded `SessionNotifications.swift` copies in the release, local preservation,
and staging worktrees share SHA-256
`d86de1dd1d066d720b3ee300e7d02dfb993c47b6269481511d9bcdbfdc5e14a2`.

The direct main merge automatically closed [PR #16](https://github.com/Ashler-AI/comet/pull/16).
The user checkout is intentionally left on
`work/crew-notifications-local-preservation`, **not main**, with the notification
work committed and pushed. Main now also contains the integrated feature, while
the local main branch has not been reset over the preserved checkout. Unrelated
doc/engine/harness and data-script changes remain
untouched. Staging copies are committed on
`release/crew-staging-notification-activation`, not left as dirty release edits.

Home includes detached and missing-space sessions and an **Archived sessions**
section with explicit **Restore**. Imported chat IDs remain opaque, while
`SessionEnvironment` projection/writes and verified deployment routing are retained.
All four target configurations use `Comet/Comet.entitlements`, with
`aps-environment = $(CREW_APS_ENVIRONMENT)`: Debug variants use `development`,
Release variants use `production`. Production remains `ai.ashler.crew`, team
`825LYXGJR6`, the production Crew/Scaffold endpoints, and `ashler-production`.
Verify the final distribution-signed IPA's push entitlement and provisioning
profile before uploading; source configuration alone is not delivery evidence.

### Connecting

- **Production**: Crew discovers Scaffold's OAuth metadata, dynamically
  registers the native client, completes authorization-code + PKCE S256 in the
  system browser, validates the issued `sc_rc_` bearer, and joins the
  deployment's verified project scope.
- **Dev**: against a local `AUTH_MODE=dev` edge, launch with a user id +
  project scope; the bearer is `userId@projectScope`.
- **Demo mode**: fully offline dataset with a scripted streaming reply —
  explore the UI with no infrastructure. Launch args for screenshot rigs:
  `-demo [-route chat:<id>|space:<id>] [-stream]`.


### Session control coverage

- The home screen merges the signed-in principal's workspace chat memberships
  with imported session refs. Unrelated project chat/status rows are not projected;
  detached and missing-space members remain reachable on desktop and mobile.
- `comet://invite/{chatId}/{sessionId}/{grantId}` links pin missing membership
  and open the session directly.
- New-session launch supports the selected desktop device or a Scaffold OMP
  environment, including source ref and database snapshot selection. Scaffold
  creation, attachment, readiness, and command admission run through the
  trusted desktop controller; the phone receives no sandbox credential.
- Existing local and Scaffold sessions accept run/steer/stop/input commands.
  Local OMP sessions with durable native context can be forked from the session
  toolbar.
- Existing and new sessions accept images from Photos or Files, including image-only
  drafts. The phone normalizes HEIC/other images to JPEG or PNG, caps selection at
  10 images (24 MiB each, 32 MiB total), and shows removable previews.
- Images upload in 45 KB chunks to the actual execution device before command
  admission. Runs carry typed attachment paths plus the desktop's prompt trailer;
  steers use the same trailer. Failure preserves the draft. Uncertain retries retain
  command identity and payload; a later materialized message is not admitted again.
- Mobile transcript image rendering is not implemented: sent images currently
  appear as attachment path text. Inline image display remains desktop-only;
  successful mobile upload/send does not imply mobile image read-back support.
- Scaffold creation is separated from first-run admission, retaining the created
  environment and route across attach/upload/admission retries. Scoped hosts permit
  upload RPCs only with file capability, the exact session, and no other-device target.
- List/header names use canonical environment/workspace metadata, with a cached
  first-user preview for untitled sessions. Metadata preload avoids full transcript
  projection and skips already-named archived rows; navigation activates transcripts.
- Disk caches are scoped by edge, auth mode, principal, project, room, and deployment.
  Old unscoped caches are not merged into a new identity's replica.
- Workspace reprojection does not prune cached stores while membership is empty,
  or while a session is open, has a Scaffold route, is sending, or awaits a send
  acknowledgement. This defers cache cleanup; it does not alter membership
  visibility or server-side authorization. Sign-out still stops all stores.

### Mobile parity verification and activation

The staging simulator exercised principal-scoped visibility, detached/archived rows,
image selection/preview, and image-only sending in existing and new sessions through the offline demo. The native
regression also passed lost-reply retry identity/payload, late-materialization dedupe,
metadata-only titles, transcript activation, and deployment cache isolation.

An isolated real Edge/Rust host relay committed **100,019 byte-identical bytes** in
45 KB chunks, denied another session's upload, and retained grant-revocation behavior.
The full edge suite passed **137 tests**. These are local simulator/isolated-host
proofs, not a physical-phone or live Scaffold image-send claim.
The final source was subsequently compiled and exercised entirely in authorized
remote CI. [Mobile run 34179062008](https://github.com/Ashler-AI/comet/actions/runs/34179062008)
built commit `4f8c10d94020ff3d7600fff9a08b99af8be31953` on arm64 with Xcode 26.6
and iOS SDK 26.5. All five probes passed, including **OK Crew store eviction**;
the Release-Staging archive succeeded with production APNs in its ad-hoc signature.
The downloaded simulator binary, identified as build 12, also passed all five
probes and rendered the session list locally without recompilation.

[Desktop release 34178218428](https://github.com/Ashler-AI/comet/actions/runs/34178218428)
compiled and passed **532 UI tests**, packaged **0.1.76**, and published staging.
Its source is `7dd1222a8f098a961866623a8203a8f45b58aa31`; the later mobile-only
regression inference and archive packaging fixes do not change desktop/edge source.
The production publication job was skipped; no active desktop engine was restarted.

[Backend deployment 34178218455](https://github.com/Ashler-AI/comet/actions/runs/34178218455)
passed remote typechecking, the 137 tests, and real scoped-host upload/revocation
smoke, then deployed staging Worker **8b5a9c92-7678-4b2a-89a9-ffd5cccb217b**.
The staging health endpoint returned `ok: true`; production deployment was skipped.

The downloaded mobile archive was locally exported for distribution signing,
without compilation. The **Crew Staging 1.0 (12)** inspection IPA passed strict
signature verification; its signature and provisioning profile both contain
`aps-environment = production` and `825LYXGJR6.ai.ashler.crew.staging`.
Inspection IPA SHA-256: `36d045233715d1daad37cdf3141bd18e7affb4b9380cd71f0803fd26edb29ef2`.
The same archive uploaded successfully at **2026-09-08 02:21:32 UTC**, delivery
`ecc19154-6e66-405a-8f0d-33473c70bf09`, with no upload errors or warnings.
The actual uploaded IPA also passed strict signature verification and retains
the production APNs entitlement. Uploaded IPA SHA-256:
`05fb66e5e2381090732934f1fc3a57eda12a780355c6f944d5e361e1d02b2034`.
App Store Connect subsequently showed upload **Complete** for **1.0 (12)**.
The TestFlight build list shows build **12**, **Ready to Submit**, expiry in
90 days, the existing **AI** internal-group badge, and **1 invite**. Its installs
column is still `–`; no physical-iPhone installation or image/notification
delivery is claimed. No local typechecks or compilation ran during this
authorized rollout.

### Production parity release

Production uses the same session, notification, image-upload, and recovery source
as staging. The mobile workflow accepts `environment=production` to build the
`Comet` scheme with `Debug`/`Release`, bundle `ai.ashler.crew`, and build **6**.
The default remains staging build **12**. Both environments verify all five
simulator probes and the built app's Edge/Scaffold endpoints, project scope,
invite scheme, version, architecture, and archive push entitlement. No Apple
credentials are uploaded to CI: distribution export, signed-IPA inspection,
and upload use the established local Xcode account flow above.

Desktop **0.1.78** passed 532 UI tests and all three fork regressions in
[34236389287](https://github.com/Ashler-AI/comet/actions/runs/34236389287), including
the large-journal fix from main `14f4344`. Its production promotion was cancelled
before publication when another session advanced main and began staging
**0.1.79**. The final desktop-only production preparation is **0.1.80**, based on
main `0c99f65`, including the newer accepted-session cleanup fixes. This avoids
moving staging backward and does not promote the other run's Linux artifacts.

Production mobile **1.0 (6)** was compiled and passed all five probes in
[34236389108](https://github.com/Ashler-AI/comet/actions/runs/34236389108) at
`d7a548a70e29367d533d7ba1207815d9cfd67ca8`. The downloaded simulator app passed
the same probes locally and visibly rendered detached and archived sessions.
The later main changes are desktop-only; no additional mobile archive is needed.
The distribution-signed inspection IPA passed strict signature verification;
both its signature and profile contain production APNs and
`825LYXGJR6.ai.ashler.crew`. Its SHA-256 is
`df102ff052785a25dd5925503bb4338c39256eaa99ab8f63b33731af8fd3840b`.
Apple accepted the upload at **2026-09-08 14:32:34 UTC**, delivery
`1e919453-4b14-4f99-8aad-4e6cff7e7264`, with no errors or warnings.
The actual uploaded IPA also passed strict signature verification and preserves
production APNs; its SHA-256 is
`b88f40ede84d414df287d05db6fd6cb4c9cf01fb87deba4bb6dde737bfc6fd56`.
Apple processing completion, tester availability, physical-phone installation,
and device notification/image delivery are not yet verified for build 6.

[Backend run 34236389366](https://github.com/Ashler-AI/comet/actions/runs/34236389366)
passed remote typechecking, 137 tests, byte-identical scoped image upload,
cross-session denial, and host revocation before deploying the exact staged
candidate to production as **14510f54-5185-430f-9719-6e9a803e7f3d**. Production
and staging health endpoints returned `ok: true`. No local compilation or
typechecks ran, and no active desktop engine was restarted by this rollout.

On 2026-09-08, production Worker secret-name inspection found no `APNS_KEY_ID`
or `APNS_PRIVATE_KEY`. Production background pushes require approved production
APNs credentials; neither deploying source nor distributing a push-entitled app
provisions them. Staging credentials are not implicitly authorized for copying.

## Architecture

```
Sync/
  LoroProtocol.swift    loro-protocol 0.3 wire codec (byte-compatible port of
                        the crate's encoding.rs: magic/varBytes/type/payload)
  RoomClient.swift      room.rs port: join with oplog VV, snapshot backfill,
                        resubmit-from-server-VV, DocUpdate+Ack, fragments,
                        %EPH presence sub-room, ping/pong lease, backoff
  WorkspaceStore.swift  ws4/{projectScope} mirror: project-shared storage,
                        principal-member chat/status projection and viewer writes
  SessionStore.swift    metadata-only list projection; joined transcript on open,
                        owner publications, upload/admission and send reconciliation
Markdown/
  MarkdownModel.swift   block model + incremental tail re-parser (re-parse
                        from the 2nd-to-last top-level block; link-defs force
                        full parses) — parser.rs port
  Highlight.swift       line tokenizer with carry state, paint-only
  MarkdownBlockView.swift  desktop metrics: body 14/22, headings 19/27…14/22,
                        code 12.5/18 (analytic line rows), violet inline code,
                        accent blockquotes, hairline tables
Transcript/
  TranscriptRows.swift  rows_for_entry port: block-granularity rows, stable
                        ids ({msg}#{part}.{block}, {msg}#g{n}), fingerprint
                        versions, consecutive-tool grouping
  TranscriptView.swift  lazy stack + stick-to-bottom (pin breaks only on user
                        scroll, 70pt re-engage band, 320pt jump button),
                        tool-group folds, error/input chips
  Veil.swift            paint-only streaming fade (EMA-tracked duration,
                        1−(1−p)^1.6 curve)
Composer/               glass pill, Send→Steer→Stop morph, QuestionPanel
                        (paged, numbered options, 220ms auto-advance)
Theme/                  theme.rs port: oklch→sRGB converter, exact palette,
                        Geist/Geist Mono, motion timings + flavour words
```

### Parity notes (desktop ⇄ mobile translations)

| Desktop | iOS |
| --- | --- |
| Sidebar: Spaces + recency-sorted Sessions | Home includes every active workspace chat, including detached/missing-space rows, plus foreign memberships |
| Horizontal session tabs per space | Space detail: recency-sorted session list |
| Tab close = archive | Swipe-to-archive; Archived sessions remain accessible, with explicit Restore |
| Composer `white_alpha(0.03)` pill + hairline | Liquid Glass pill (`glassEffect`) + hairline |
| Harness brand SVG marks (icons.rs) | Same path data via a native SVG path parser (`BrandMarks.swift`) |
| Harness/model picker popover + curated catalogs | Brand-mark cards + catalog menu + reasoning-ladder chips (`HarnessCatalog.swift`, ported from crates/harness) |
| Add-space palette (device + folder browser) | New-space sheet: device tabs + remote folder browser (ListFolders over the device-room relay, git repos badged) |
| ControlRpc over device-room relay | `DeviceRelayClient` — binary `uleb128(len)+header+payload` frames, `{"s","k","to","from"}` header, ndjson ControlRpc; used for ListFolders + direct-to-host `Mutate {createSpace}` (local doc-write fallback when the host is offline) |
| Hover timestamps / copy | Context menus |
| gpui `list()` sum-tree virtualization | `LazyVStack` + stable row ids + version fingerprints |
| Stick-to-bottom spring, wheel-up breaks pin | Scroll-phase-gated pin + spring scrollTo, same 70/320pt thresholds |

Status colors, fonts, spacing, markdown metrics, veil timing, command-ledger
shapes, and the wire protocol are ports, not approximations — constants match
the desktop sources cited in each file header.

### Writer discipline (what the phone writes)

- Workspace doc: `archived`/`title`/`lastSeenAt` and related chat metadata,
  plus principal-scoped session refs and their `SessionEnvironment` routing data.
  Explicit restore clears that chat's pending worktree-deletion request; viewing
  an archived session never restores it. The phone does not advertise an engine
  device row or presence heartbeat.
- Chat creation awaits the owning host's authenticated `Mutate:createChat`
  before local echo or sending.
- Ordinary-session commands use the host's `QueueCommand` admission path;
  Scaffold commands use the verified controller route. Client-minted message
  IDs connect optimistic sends to admission errors and committed transcript
  entries. The host writes transcript entries and command outcomes.

## Session attention notifications

Open the account menu → **Notifications** and enable **Session attention alerts**.
iOS permission is opt-in. Fresh input requests, errors, and working→idle
completions alert; initial per-session hydration, heartbeat, stale/reordered
updates and archived sessions stay silent. Fresh transitions received after reconnect
still alert. Viewing the affected session suppresses foreground banners. Tapping an alert opens its session only when
its user and project match the current sign-in.

Background delivery uses APNs, not a background WebSocket. The Xcode target
has the Push Notifications capability and `Comet/Comet.entitlements`; a device
build needs an Apple provisioning profile with that capability. APNs environment
comes from the embedded profile (sandbox on Simulator; production for App Store
distribution). Configure the deployed edge with:

- `APNS_KEY_ID`: Apple APNs signing key ID.
- `APNS_TEAM_ID`: Apple developer team ID.
- `APNS_PRIVATE_KEY`: the complete Apple `.p8` PKCS#8 PEM, stored as a Worker secret.
- `APNS_TOPIC`: exact signed app bundle identifier (`ai.ashler.crew.staging` for
  Crew Staging; `ai.ashler.crew` for production).
- `NOTIFICATION_CREDENTIAL_KEY`: a dedicated Worker secret containing canonical
  base64 for exactly 32 cryptographically random bytes. Used for AES-256-GCM
  encryption, not an APNs credential. Keep it stable across deployments;
  rotation requires foreground registration renewal before old registrations can deliver.

Without valid configuration, registration returns HTTP 503 and the settings
screen reports that only running-app local alerts are available. It does not
claim background delivery. Foreground activation retries registration and
renews its 30-day lifetime. Sign-out immediately clears local state and unregisters
from APNs; server DELETE is best-effort after an in-flight PUT. It never blocks
offline logout. Explicitly disabling alerts requires successful server confirmation
and reports network failures. Server registrations expire after 30 days or are
removed when current human authorization is rejected or APNs invalidates the token.

The edge stores push tokens privately and AES-256-GCM encrypted credentials in
Durable Object SQL, never plaintext credentials in SQL, shared documents, or logs.
The dedicated encryption key lives in Worker secrets; authenticated encryption
binds each credential to installation, user, and project. Legacy plaintext
registration tables are purged during migration; apps must renew registration.
The credential is decrypted transiently to recheck human authorization before each
delivery. Existing `AuthGrant` records cover sandbox device grants, not human
Scaffold credentials, so they cannot replace these checks. Authority outages fail
closed without deleting registrations; explicit authorization rejection and APNs
invalid-token responses remove only the matching registration revision.

Alerts contain generic Crew copy and routing IDs, not titles or transcripts.
Registration is principal/project scoped; single-session device grants cannot register.

Offline regression launch: `-visibility-e2e` runs session visibility,
attention-transition, retry, metadata and cache-isolation scenarios and opens demo mode. Debug builds additionally
exercise queued APNs token arrival, scoped routing, offline disable/logout and
late-response isolation using an in-process HTTP responder. Every lifecycle wait
has a five-second deadline and logs a stage-specific `FAIL` before cancelling
the probe safely; success is logged only after logout cleanup drains. Results
append to `Documents/e2e.log`; no sign-in or push permission is requested.

With a Debug simulator build installed, run the existing offline hook:

```sh
xcrun simctl launch --terminate-running-process booted ai.ashler.crew -visibility-e2e
xcrun simctl get_app_container booted ai.ashler.crew data
```

Read `Documents/e2e.log` below the returned data-container path. Expect the
`OK Crew session visibility`, `OK Crew attention transitions`, and
`OK Crew APNs lifecycle` markers and no `FAIL` entries from this launch. The
lifecycle marker includes `drained logout DELETE`; verify the app remains alive
afterward, since a log marker alone cannot catch a subsequent teardown crash.
For deterministic Debug-only failure coverage, append
`-notification-e2e-missing-delete` to suppress the logout response, or
`-notification-e2e-missing-token` to remove the logout credential before the
DELETE can reach the responder. Run these separately; each must log
`FAIL Crew APNs lifecycle deadline: logout DELETE responder` within five seconds
of that stage, omit the lifecycle `OK` marker, and leave the app alive after
teardown. Neither flag affects a release build or sends real network traffic.
The visibility scenario also checks environment/deployment routing survives
opaque-ID upsert and projection. Substitute `ai.ashler.crew.staging` for staging.
