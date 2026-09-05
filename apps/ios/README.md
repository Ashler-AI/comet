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
# Production: Crew, ai.ashler.crew, version 1.0 build 3
xcodebuild -project Comet.xcodeproj -scheme Comet \
  -destination 'platform=iOS Simulator,name=Crew Mobile Parity' build

# Staging: Crew Staging, ai.ashler.crew.staging, version 1.0 build 2
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
| Sidebar: Spaces + attention-sorted Sessions | Home screen sections (same sort ranks: awaiting > errored > working > completed > idle) |
| Horizontal session tabs per space | Space detail: vertical session list (creation order) |
| Tab close = archive | Swipe-to-archive |
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
