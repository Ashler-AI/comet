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

### Production and staging

The shared `Comet` scheme uses `Debug` / `Release` for Crew (`ai.ashler.crew`).
The shared `Crew Staging` scheme uses `Debug-Staging` / `Release-Staging` for
Crew Staging (`ai.ashler.crew.staging`), with staging source build number 8.
Both use Apple team `825LYXGJR6`; separate bundle IDs isolate persisted state
and credentials. `CREW_*` build settings populate Info.plist and `ReleaseConfig`.

| Scheme | Edge | Scaffold | Project scope | Invite scheme |
| --- | --- | --- | --- | --- |
| `Comet` | `comet.internal.ashler.com` | `scaffold.internal.ashler.com` | `ashler-production` | `comet://` |
| `Crew Staging` | `comet-staging.internal.ashler.com` | `scaffold-staging.internal.ashler.com` | `ashler-staging` | `comet-staging://` |

### Staging 1.0 (8): session visibility and attention alerts

Build 8 was uploaded on 2026-09-07, processed by App Store Connect, and assigned
to **Ashler Internal**. The exact application source is `67dadc7` on
`release/crew-staging-1.0-8`; this primary checkout also contains subsequent
configuration changes and is not that exact release snapshot. Build 8's signed
IPA and embedded profile carry `aps-environment = production`, using refreshed
staging profile `a9f3f9ce-32c7-4e9f-a678-bc967e5a44aa`.

After explicit approval, backend source `8a7877d` on
`release/crew-staging-notification-activation` was deployed to
`ashler-comet-edge-staging` as Worker version
`13508539-fe5a-47d9-a5d5-5cf5bd362bb5`. This includes the authenticated
`/notifications/device` route and workspace attention observer. Production
was not deployed. Staging uses `APNS_TOPIC = ai.ashler.crew.staging` and
`APNS_TEAM_ID = 825LYXGJR6`.

Apple key `99D9D7GGF4` (**Crew Staging Notifications**) enables APNs only,
**Sandbox & Production**, **Team Scoped (All Topics)**. Its private key and ID,
plus a newly generated 32-byte `NOTIFICATION_CREDENTIAL_KEY`, are stored in
staging Worker secrets. The one-time PKCS#8 P-256 download was uploaded
successfully; both local `.p8` copies were deleted. The encryption secret was
written without a trailing newline.

Staging health passed; authenticated Sandbox and Production registration probes
and their cleanup DELETE returned HTTP 200. These checks verify credential
encryption and signing-key parsing, not Apple acceptance or device delivery.

The subsequent live attention probe exposed a native Workers `fetch` receiver
bug before Apple was contacted. Source fix `eefd8e3` invokes the transport as
a standalone function; regression coverage preserves that native calling
contract. Allowlisted rejection diagnostics never log provider bodies,
credentials, tokens, request URLs, or session content. Final staging Worker
version is `5f208fff-1f91-43bd-85e4-4b2825c10f27`.

After full workspace hydration, temporary working-to-awaiting-input transitions
reached Apple in **Production and Sandbox**, each returning **400 BadDeviceToken**
for the intentionally invalid probe token. No **403 InvalidProviderToken** was
observed. Both probe registrations and temporary workspace chat/session rows
were removed. Temporary tracing and local signing fixtures were removed.
These are real upstream rejection results, not successful device delivery.
The probes used disposable rows in the shared staging workspace, not a separate
project. The existing registration list was not inspected first, so other
subscribed tester devices may have received a generic attention alert.

The paired iPhone was unavailable to local tooling. To verify physical delivery,
open Crew Staging build 8, enable **Session attention alerts**, background the
app, and trigger a fresh attention transition. No new app build is required.

### Connecting

- **Production**: Comet discovers Scaffold's OAuth metadata, dynamically
  registers the native client, completes authorization-code + PKCE S256 in the
  system browser, validates the issued `sc_rc_` bearer, and joins the
  deployment's verified project scope.
- **Dev**: against a local `AUTH_MODE=dev` edge, launch with a user id +
  project scope; the bearer is `userId@projectScope`.
- **Demo mode**: fully offline dataset with a scripted streaming reply —
  explore the UI with no infrastructure. Launch args for screenshot rigs:
  `-demo [-route chat:<id>|space:<id>] [-stream]`.

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
  SessionStore.swift    session doc mirror: entries/parts (continuations
                        joined), command ledger appends (rule 1), host nudge
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
| Horizontal session tabs per space | Space detail: vertical session list in recency order |
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

- Workspace doc: its own device row, chat creates (host = the space's owning
  device), `archived`/`title`/`lastSeenAt` LWW sets, presence heartbeats.
- Session docs: command ledger appends only (`run`/`steer`/`interrupt`/
  `respondInput`), with client-minted message ids for optimistic echo. The
  host writes all transcript entries and command outcomes.
- After queuing a command it POSTs `/device/{host}/nudge` so a cold host
  opens the doc and drains — delivery stays durable in the doc regardless.

## Session attention notifications

Open the account menu → **Notifications** and enable **Session attention alerts**.
iOS permission is opt-in. Fresh input requests, errors, and working→idle
completions alert; initial per-session hydration, heartbeat, stale/reordered
updates and archived sessions stay silent. Fresh transitions received after reconnect
still alert. Viewing the affected session suppresses foreground banners. Tapping an alert opens its session only when
its user and project match the current sign-in.

Background delivery uses APNs, not a background WebSocket. The Xcode target
has the Push Notifications capability and `Comet/Comet.entitlements`; a device
build needs an Apple provisioning profile with that capability. The source
entitlement uses `$(CREW_APS_ENVIRONMENT)`: `development` for `Debug` and
`Debug-Staging`, `production` for `Release` and `Release-Staging`. Registration
reads the embedded profile's APNs environment (sandbox on Simulator). Verify
the final signed app and profile agree before distribution. Configure the
deployed edge with:

- `APNS_KEY_ID`: Apple APNs signing key ID.
- `APNS_TEAM_ID`: Apple developer team ID.
- `APNS_PRIVATE_KEY`: the complete Apple `.p8` PKCS#8 PEM, stored as a Worker secret.
- `APNS_TOPIC`: exact signed app bundle identifier: `ai.ashler.crew` for
  production or `ai.ashler.crew.staging` for staging.
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
late-response isolation using an in-process HTTP responder. The regression drains
the logout DELETE before invalidating its URLSession, including on failure paths.
Results append to `Documents/e2e.log`; no sign-in or push permission is requested.
Check process survival after the lifecycle marker as well as the log: a marker
alone cannot catch a subsequent teardown crash.
