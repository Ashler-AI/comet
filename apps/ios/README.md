# Crew Canary for iOS

An isolated UIKit viewport built on upstream's Rust client, registry/chat2
transport, reconnect/network/background handling, and incremental transcript
layout. No engine or provider credential runs on the phone. Crew-specific
authentication and typed controller operations use the shared Rust core.

**Rust decides what to paint and where; Swift paints, scrolls and handles
gestures.** Everything in Rust is platform-neutral — the future Android app
links the same library. See [`docs/mobile-rewrite.md`](../../docs/mobile-rewrite.md).

## Build & run

Requires Xcode 26+, an iOS 26+ simulator/device, and Rust with the
`aarch64-apple-ios` and `aarch64-apple-ios-sim` targets. Internal Xcode target,
module, and Rust library names remain upstream `Zeron`/`zeron_*`.

The shared scheme is **Crew Canary**, bundle ID **ai.ashler.crew.upstream-canary**.
The widget/tests use child bundle IDs. No production/staging app ID or URL
handler is replaced. Signing team is deliberately unset; an authorized device
build must supply its own team and canary provisioning profiles.

Supply all four Xcode build settings (substituted into Info.plist):

| Setting | Required value |
| --- | --- |
| `CREW_CONTROL_ORIGIN` | Exact trusted Scaffold authority origin |
| `CREW_EDGE_ORIGIN` | Dedicated upstream-canary Edge origin |
| `CREW_PROJECT_SCOPE` | Project actually verified by that Edge/authority |
| `CREW_DEPLOYMENT_ID` | Deployment actually returned by `/auth/session` |

Missing settings fail closed; there is no production fallback. Scaffold must
permit public-client PKCE OAuth with redirect
`crew-upstream-canary://callback`. The helper verifies discovery, resource,
callback state/expiry, user/project/deployment, and exact configured origins.
The authority accepts this exact callback only when its Crew link scheme,
project and deployment are configured; registration, authorization and exchange
share that validator. The app uses `Crew Canary` for both its display name and
native authentication consent, while executable/module names stay upstream.
On cold launch credentials are reverified before opening the profile. A
transient authority outage retains credentials and all local data.

For an authorized simulator build, use `-project Zeron.xcodeproj -scheme
'Crew Canary' -destination 'platform=iOS Simulator,name=iPhone 17 Pro'` plus
the four settings above and `CODE_SIGNING_ALLOWED=NO`. Do not run an install,
archive, signing, or TestFlight operation against an existing app identity.

The Zeron target's **Rust core** build phase runs `scripts/ios/build-core.sh`,
which builds `crates/mobile` for the active platform (always optimized — the
`mobile` cargo profile) and refreshes the committed UniFFI bindings in
`Zeron/Core/Generated/`. Regenerate these bindings as part of the first
integrated build: the new `CrewLogin`, `CrewAccount`, Scaffold credential case,
and `CoreClient.crew*` APIs are not in the old generated Swift file.
`ZERON_SKIP_CORE=1` is only safe after rebuilding matching bindings/library.

## Crew integration

- `CrewLogin` wraps `zeron_sync::scaffold_auth`; no Swift OAuth/sync stack is
  copied. Keychain uses a canary service plus configured-origin/scope hash.
  The verified profile hash includes user, project, deployment, Edge, and
  authority. Drafts, cache, commands and migration records are profile-scoped.
  Sign-out removes credentials, not cached-only history or pending work.
- Personal registry synchronization discovers sessions from every authorized
  execution host belonging to the same user. A host's offline presence does
  not delete its rows/history.
- Cold/warm/signed-out URL handling retains pending route hints. The parser
  accepts existing `comet`/`comet-staging` session-directory and Scaffold
  routes as well as `crew-upstream-canary`; only the latter is registered
  with iOS. Settings → Open existing Crew link allows testing old links
  without stealing the installed app's handlers.
- Cached links validate `Chat.crew` owner, project, deployment, session and
  sandbox identity. Missing rows resolve via a desktop controller's
  `ResolveCrewSessionLink {url} -> {chatId}` and normal registry publication.
  The host must publish canonical verified metadata on session adoption.
  Resolution is browse-only; it must not attach, resume or create execution.
- Settings → Scaffold sandboxes calls `crew.status` (requires
  `role: local_controller`), `scaffold.refresh`, `scaffold.control` and
  `scaffold.reconcile` over the upstream relay. Create includes source ref,
  region, local/staging-snapshot database selection, provider/model, and an
  optional opaque Agent Auth account. The configured host owns runtime
  version selection and validates scope. Attach explicitly resumes a paused
  sandbox. Stop is terminal; composer Stop remains single-turn cancellation.
  Commands are persisted atomically before dispatch, sent once, and retained
  with confirmed/rejected/unknown outcomes. Reconciliation never creates a
  replacement sandbox or automatically replays an unknown operation.
- Managed inference stays on the host through Agent Auth. Upstream's direct
  Codex voice route has no managed adapter: Crew voice is visibly unavailable
  and rejected in the shared client rather than falling back to host CLI
  credentials. Upstream dev/demo voice code remains intact.

## Legacy iOS preservation

The distinct canary app cannot and must not read another installed app's
sandbox or Keychain. Export a consistent **copy** of its Application
Support/CometDocs folder through an authorized backup/export process, keep the
old installation, then select that folder in Settings → Legacy iOS backup.

The version-1 importer inventories bounded regular files, rejects symlinks
and nested folders, copies every file, records sizes/SHA-256 in a manifest,
and exports materialized Loro snapshots as offline JSON. Intents/outcomes
remain immutable evidence and are never replayed. Unknown or unreadable
records remain byte-for-byte preserved and are explicitly reported.
The native export picker exports the complete preservation archive.

This is a safe quarantine/data-preservation migration, **not** a claim that
pending operations are reconciled or cache-only history has been published.
An authenticated desktop migration/reconciliation must account for every
retained ID and pending outcome before cutover; do not delete the old app or
backup while `requiresReconciliation` is true. Legacy credentials are not
imported; sign in again. Existing appearance preferences remain native,
while new-session selections and composer drafts use the new profile.

## Integration verification (not run during this port)

Run the Rust client tests for Crew link parsing and copy-only legacy import,
and `ZeronTests/CrewIsolationTests`. Exercise two desktops plus iOS with the
same verified identity: local/remote session discovery, offline-host history,
cold/warm/signed-out links, denied cross-project/deployment/sandbox links,
foreground/background and network recovery, long streaming, cancellation,
send after reconnect, revoked authority, and command-outcome recovery.
The existing upstream mobile layout/network/streaming suites remain intact.
Use authorized remote CI for typechecks; local typechecks are intentionally
not run to preserve workstation resources.

## Layout

```
Zeron/
  App/         AppDelegate, SceneDelegate, AppModel (owns CoreClient; maps
               workspace snapshots to view models; Keychain credentials)
  Core/        Generated UniFFI bindings; Fonts (the exact bytes Rust measures,
               registered with CoreText) + CoreText fallback measurer
  Design/      Palette (Zeron color roles → light/dark), Glass helpers,
               cell-grid status glyph, harness brand marks
  Transcript/  TranscriptListView (virtualized scroll host over LayoutFrame),
               RowView/RowModel (CoreText painter for Rust display lists,
               streaming veil), FrameRelay
  Session/     SessionViewController, SessionSource (Core/Fixture), new-session
               canvas
  Composer/    ComposerBar (glass capsule ⇄ card with inline context chips;
               send/queue/steer/stop), question and queue panels, attachments
  Threads/     Sessions (foldable sidebar sections)/folder/search lists,
               cells, new-project folder browser
  Shell/       Tab bar (Sessions, Settings, search; "New session" accessory
               with live summary), sign-in
  Debug/       Transcript lab + hitch meter
ZeronLiveActivity/  Widget extension: the voice call's Live Activity (Dynamic
               Island + Lock Screen), with the orb's states exported from
               zeron-orb as vector assets
ZeronShared/   Compiled into the app and the extension: the activity's
               attributes and its mute/end App Intents
```

### Transcript pipeline

1. `zeron-client` applies session-doc updates incrementally (O(changed
   entries)) and publishes immutable snapshots.
2. `TranscriptView.attach(client, chatId)` subscribes **in Rust**; the layout
   thread turns entries into rows (one per markdown block / user message / tool
   group), reparses streaming markdown incrementally, and measures every row
   with `zeron-text` at the viewport width.
3. Each pass publishes a `LayoutFrame`: exact heights + prefix-sum offsets.
   `rowsIn(y0, y1)` is a binary search; `display(i)` builds a display list
   (text runs at exact positions, boxes, links, scrollers, widgets).
4. `TranscriptListView` positions reusable `RowView`s at those offsets and
   paints runs with CoreText at the Rust coordinates — measurement and
   rendering can't disagree. Display models for rows beyond the viewport are
   prefetched off the main thread.
5. Agent visuals (`show_visual` Luau scripts) get a worker thread each
   (`crates/mobile/src/visual`): it runs the script, lays the node tree out,
   draws canvases and charts as vector paint, renders scenes and shaders with
   wgpu, and reports its height back to the row. `VisualView` paints what
   arrives (rasterized on the worker thread) and forwards touches; the demo's
   "Agent visuals" chat (`-demo -route chat:chat-visuals`) has one of each.

## Launch arguments

| Arg | Effect |
| --- | --- |
| `-demo` | Offline demo workspace (Rust `DemoHost`: registry, docs, streaming replies) |
| `-fast` / `-longreply` | Demo stream speed / reply length |
| `-big` / `-huge` | Demo transcripts with 120 / 600 turns |
| `-route chat:<id>` / `new` / `more` / `search` | Open a screen at launch |
| `-signedout` | Clear stored credentials |
| `-dev <userId> <orgId> [-edge <url>]` | Dev bearer against an `AUTH_MODE=dev` edge (e.g. `wrangler dev`); not persisted |
| `-harness <id>` | Default harness for new sessions (`mock` for live-stack tests) |
| `-bench` (with `-lab`) | Display-link fling benchmark; writes `Documents/bench.json` |
| `-measureopen` | Session open latency; writes `Documents/open.json` |
| `-lab [-turns N] [-autostream] [-autoscroll] [-top] [-meter]` | Transcript lab over fixture markdown, with an on-screen hitch meter |

## Tests

```sh
# Rust
cargo test -p zeron-text -p zeron-markdown -p zeron-client -p zeron-mobile
cargo test --release -p zeron-mobile --lib bench_layout -- --ignored --nocapture

# Line-break accuracy vs CoreText (same font bytes), flows, hitch benchmarks
xcodebuild … -only-testing:ZeronTests/LineBreakAccuracyTests test
xcodebuild … -only-testing:ZeronUITests/SessionFlowTests test
xcodebuild … -only-testing:ZeronUITests/ScrollPerformanceTests test
```

Live stack (real edge + headless engine; see `ZeronUITests/LiveStackTests.swift`):

```sh
(cd edge && npx wrangler dev --port 27650 --var AUTH_MODE:dev) &
ZERON_DATA_DIR=/tmp/e2e ZERON_IPC_PORT=27811 ZERON_EDGE_URL=http://localhost:27650 \
  ZERON_EDGE_TOKEN=alice@org1 ZERON_ORG_ID=org1 ZERON_HARNESS=mock target/debug/zeron headless &
TEST_RUNNER_ZERON_LIVE_EDGE=http://localhost:27650 xcodebuild … -only-testing:ZeronUITests/LiveStackTests test
```

The test launches the app with `-harness mock`: a chat's configured harness
wins over the engine's `ZERON_HARNESS` default, so without it a real agent
would run.

## Session notifications

The edge sends them (`edge/src/push-notify.ts`, `registry-room.ts`): when a
session's registry row changes it applies the desktop's rule (run finished,
waiting on your input, run failed) and pushes to every phone that registered
with `POST /registry/:org/push-target`. The app asks for permission the first
time a session is started from the phone (or from Settings → Notifications),
registers its APNs token with the choices from Settings, stays quiet while
open, and opens the session a notification is tapped for.

Setup once per deployment: an APNs auth key (Apple Developer → Keys →
Apple Push Notifications service), then `wrangler secret put APNS_KEY_P8` (the
`.p8` contents) and `wrangler secret put APNS_KEY_ID` in `edge/`. The
TestFlight workflow enables the Push capability on the App ID itself.
`/registry/:org/stats` shows registered phones and recent deliveries.

## Canary distribution boundary

This work does not authorize App Store/TestFlight release, physical-device
installation, signing, or production/staging replacement. The parent canary
workflow must supply the explicit identity/configuration above. APNs requires
the separate `ai.ashler.crew.upstream-canary` topic and authorized canary
credentials/provisioning; do not reuse an installed production topic.
