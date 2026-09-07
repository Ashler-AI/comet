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
# Production preparation: Crew, ai.ashler.crew, version 1.0 build 5 (tentative)
xcodebuild -project Comet.xcodeproj -scheme Comet \
  -destination 'platform=iOS Simulator,name=Crew Mobile Parity' build

# Staging: Crew Staging, ai.ashler.crew.staging, version 1.0 build 8
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
Production Worker deployment remains blocked on approved production APNs
credentials and must use `deploy.yml` dispatch `target=production`, never a local
production deploy. The checked-in workflow also invokes a typecheck, which this
agent is prohibited from starting; its approval gate must not be bypassed.

The release workflow verified the desktop production manifest, checksums, and
latest pointer. Local `comet update --check` returned HTTP 401 without a current
production login, so authenticated client download was not verified locally.
Typechecks were intentionally not run because global instructions prohibit them.

Source reconciliation verified that fetched `origin/main` (`f89dd43`) is an
ancestor of the production release branch, which initially held exactly three
additional release commits. The stale local `main` checkout was not its baseline.
The already-published branch had not been merged, however. The release workflow
now fails production promotion unless its dispatch/tag SHA is an ancestor of
`origin/main`, including candidate-reuse dispatches. Further promotion must wait
for the PR merge; published 0.1.72 artifacts must not be overwritten.

The full `cargo test -p comet-ui` suite passed **531 tests** with zero failures;
the production port changed no comet-doc or comet-engine source files. A fresh
`scripts/package-macos.sh` bundle generated an actual `awaitingInput` transition,
but macOS displayed its Crew permission notice rather than an attention banner.
Permission was not granted automatically; real banner delivery and click routing
remain unverified until the user allows Crew notifications.

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

- The home screen merges every non-archived workspace chat with the signed-in
  principal's imported session refs.
- `comet://invite/{chatId}/{sessionId}/{grantId}` links pin missing membership
  and open the session directly.
- New-session launch supports the selected desktop device or a Scaffold OMP
  environment, including source ref and database snapshot selection. Scaffold
  creation, attachment, readiness, and command admission run through the
  trusted desktop controller; the phone receives no sandbox credential.
- Existing local and Scaffold sessions accept run/steer/stop/input commands.
  Local OMP sessions with durable native context can be forked from the session
  toolbar.

## Architecture

```
Sync/
  LoroProtocol.swift    loro-protocol 0.3 wire codec (byte-compatible port of
                        the crate's encoding.rs: magic/varBytes/type/payload)
  RoomClient.swift      room.rs port: join with oplog VV, snapshot backfill,
                        resubmit-from-server-VV, DocUpdate+Ack, fragments,
                        %EPH presence sub-room, ping/pong lease, backoff
  WorkspaceStore.swift  ws4/{projectScope} mirror: project-shared
                        devices/spaces/chats/sessions plus principal-scoped
                        session refs and viewer-side writes
  SessionStore.swift    session doc mirror: joined transcript, owner
                        publications, send reconciliation; off-main projection
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

Offline regression launch: `-visibility-e2e` runs session visibility and
attention-transition scenarios and opens demo mode. Debug builds additionally
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
