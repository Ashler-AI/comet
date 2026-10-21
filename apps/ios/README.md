# Crew Staging for iOS

A native UIKit viewport built on upstream's Rust client, registry/chat2
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

The shared scheme is **Crew Staging**, bundle ID **ai.ashler.crew.staging**,
signing team **825LYXGJR6**, version **1.0**. This replaces the existing staging
app while retaining its sandbox and default team/bundle Keychain access group.
Legacy has no explicit app-group entitlement; none is invented. The upstream
LiveActivity extension uses `ai.ashler.crew.staging.LiveActivity` and needs an
authorizing team distribution profile, which may be an existing matching wildcard
profile because the extension requests no app-specific capabilities. Production
`ai.ashler.crew` is not built or uploaded.

Checked-in staging settings (substituted into Info.plist):

| Setting | Value |
| --- | --- |
| `CREW_CONTROL_ORIGIN` | `https://scaffold-staging.internal.ashler.com` |
| `CREW_EDGE_ORIGIN` | `https://comet-staging.internal.ashler.com` |
| `CREW_PROJECT_SCOPE` | `ashler-staging` |
| `CREW_DEPLOYMENT_ID` | `ashler-staging` |

Scaffold public-client PKCE uses `comet-staging://callback`. The shared Rust
helper verifies discovery, resource, state/expiry, user/project/deployment and
exact origins. Credentials are reverified before opening a profile; a transient
authority outage retains local data. Debug-only endpoint overrides remain for
the existing demo/test harness, not a second production runtime.

Simulator build from this worktree (does not install or launch an app):

```sh
xcodebuild -project apps/ios/Zeron.xcodeproj -scheme 'Crew Staging' \
  -configuration Debug -destination 'generic/platform=iOS Simulator' \
  -derivedDataPath /tmp/Crew-Staging-Native-Cutover-DerivedData \
  ARCHS=arm64 ONLY_ACTIVE_ARCH=YES \
  ZERON_SKIP_CORE=0 CODE_SIGNING_ALLOWED=YES CODE_SIGN_IDENTITY=- build
```

Use ad-hoc simulator signing: disabling signing omits simulated identity and
Keychain rejects writes with OSStatus `-34018`. Derived data is separate from
the user's normal Xcode build; Rust artifacts remain in this worktree's `target/`.
These simulator commands target Apple Silicon: the Rust build phase emits only
`aarch64-apple-ios-sim`. Pin `ARCHS=arm64` for a generic simulator destination;
otherwise Xcode also attempts x86_64 and cannot link the arm64-only Rust library.
`ONLY_ACTIVE_ARCH=YES` alone does not constrain a generic destination.

The Zeron target's **Rust core** build phase runs `scripts/ios/build-core.sh`,
which builds `crates/mobile` for the active platform (always optimized — the
`mobile` cargo profile) and refreshes the committed UniFFI bindings in
`Zeron/Core/Generated/`. Regenerate bindings during the integrated build after
changes to `CrewLogin`, `CrewAccount`, or `crewPrepareLegacyBackup`.
`ZERON_SKIP_CORE=1` is only safe after rebuilding matching bindings/library.
`crates/mobile/uniffi.toml` leaves native error presentation to
`Core/CoreError.swift`, preserving the message instead of reflecting enum names.

### Explicit simulator session import

For an approved local test only, a **Debug simulator** build accepts
`-import-staging-session`. Put the staging desktop's `scaffold-session.json`
at `<simulator app data container>/tmp/crew-scaffold-session.json` with mode `0600`,
then launch only `ai.ashler.crew.staging` with that flag. Discover the container
with `xcrun simctl get_app_container <simulator-udid> ai.ashler.crew.staging data`;
installing a build can change its path.
Never put a bearer in launch arguments, environment variables, logs or chat.

The one-shot reader rejects symlinks, hard links, non-private/non-owned files and
files over 64 KB. It unlinks the regular transfer file before parsing or network
access. Existing `CrewLogin.restore` verifies the exact authority, Edge, user,
project and deployment before the normal sign-in path stores it in Keychain.
Failed imports do not erase a previously saved account or silently sign into it.
Relaunch without the flag to use normal verified Keychain restoration.

This copies a reusable, revocable **Scaffold** credential—not Google cookies,
passwords, passkeys or provider credentials. Desktop and simulator share its
revocation. It verifies authenticated mobile behavior, not independent Google
sign-in. The hook is absent from device and Release builds.

The existing `CrewIsolationTests` cover native error presentation, private-file
consumption and symlink/hard-link/size/encoding rejection. Staging cutover
verification must be run against the exact integrated source; earlier Canary
results are not release evidence for this build.


## Crew integration

- `CrewLogin` wraps `zeron_sync::scaffold_auth`; no Swift OAuth/sync stack is
  copied. Keychain retains `ai.ashler.crew.staging` service with new scoped accounts;
  legacy account entries remain untouched and are not trusted as new credentials.
  The verified profile hash includes user, project, deployment, Edge, and
  authority. Drafts, cache, commands and migration records are profile-scoped.
  Sign-out removes credentials, not cached-only history or pending work.
- Personal registry synchronization discovers sessions from every authorized
  execution host belonging to the same user. A host's offline presence does
  not delete its rows/history.
- Cold/warm/signed-out URL handling retains pending route hints. The parser
  accepts existing `comet`/`comet-staging` session-directory and Scaffold routes;
  only `comet-staging` is registered with this staging app. Settings → Open
  existing Crew link validates route hints against the authenticated scope.
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
  sandbox. Stop is terminal for its execution epoch; a stopped/failed sandbox
  exposes a separate **Start new epoch** action with explicit confirmation.
  It retains the logical session/control-record IDs and history while replacing
  physical execution and its native association. Old requests are not replayed.
  The typed `start` uses the selected `sandbox_id` control record, scope and
  `expectedLifecycleEpoch` from the observed environment's `lifecycle_epoch`.
  Missing or malformed epochs block Start; the control plane rejects a stale
  epoch after a concurrent start/stop. It never substitutes Resume, Attach or
  Create or promises the same physical VM.
  Composer Stop remains single-turn cancellation.
  Commands are persisted atomically before dispatch, sent once, and retained
  with confirmed/rejected/unknown outcomes. Reconciliation never creates a
  replacement sandbox or automatically replays an unknown operation.
- Managed inference stays on the host through Agent Auth. Upstream's direct
  Codex voice route has no managed adapter: Crew voice is visibly unavailable
  and rejected in the shared client rather than falling back to host CLI
  credentials. Upstream dev/demo voice code remains intact.

## Legacy iOS native migration

Same-bundle upgrades retain `Application Support/CometDocs`. After online
Scaffold identity verification, before constructing `CoreClient` or starting any
writer, `crewPrepareLegacyBackup` imports the matching scoped cache into
`Application Support/profiles/scaffold/<verified-profile-key>`. The legacy
frontend/transport is not linked or run. Foreign scopes fail closed, originals
remain untouched, and a matching receipt makes reopening idempotent.

| Legacy data | Native cutover contract |
| --- | --- |
| Scoped workspace/chat Loro | Validate owner/project/deployment, convert into native registry/session documents; never feed old raw Loro to live sync |
| Public/native session IDs, models, checkout/worktree | Retain exact recorded identities and configuration; never invent missing host state |
| Intents/outcomes, recovery snapshots | Preserve bytes/hashes and exact command IDs; unknown outcomes are not replay permission |
| Pending images | Preserve decoded local attachment copies alongside originals |
| Pruned phone history / host-native journals | Report missing data explicitly; owning host migration remains required |
| Legacy Keychain entries | Retain under existing service/access group, require fresh sign-in |

`legacy-ios-import.json` records file hashes, native IDs, retained intents,
outcomes, missing data and reconciliation status. Settings → Legacy iOS migration
shows the receipt and exports immutable originals, never a live SQLite store.
Do not remove originals while reconciliation is outstanding. Appearance
preferences stay native; new drafts and commands are scoped to the verified
profile. Native importer tests and exact-device acceptance remain release gates.

`CrewChatMetadata.originalEpochHistory` also retains the required
`coordinatorLifecycleEpoch` separately from the original writer's `lifecycleEpoch`.
This is Rust-owned JSON metadata, not a new UniFFI record or a Swift migration.
The coordinator epoch must be at least the original epoch; missing provenance
must not be filled from a current session or another device.

### Isolated offline smoke (integration owner only)

Run after the final Rust changes land. This is an alternative to the build-only
command above: build once for testing, then execute against a newly created
simulator. It never installs over an existing simulator app, imports credentials,
uses `-signedout`, or erases a simulator. Do not substitute a prior Canary login
for staging authentication evidence.

```sh
# From the repository root. Integration-owner commands for a fresh isolated smoke.
set -eu
SMOKE_ROOT=$(mktemp -d /tmp/Crew-Staging-Native-Smoke.XXXXXX)

# Pure importer fixture: unique temporary source/profile directories; no network.
cargo test --locked -p zeron-client --lib \
  legacy::tests::native_hydration_preserves_identity_and_unknown_commands_without_replay \
  -- --exact

xcodebuild -project apps/ios/Zeron.xcodeproj -scheme 'Crew Staging' \
  -configuration Debug -destination 'generic/platform=iOS Simulator' \
  -derivedDataPath "$SMOKE_ROOT/DerivedData" \
  ARCHS=arm64 ONLY_ACTIVE_ARCH=YES \
  -only-testing:ZeronTests/CrewIsolationTests \
  -only-testing:ZeronUITests/SessionFlowTests/testSendEchoesAndStreamsReply \
  ZERON_SKIP_CORE=0 CODE_SIGNING_ALLOWED=YES CODE_SIGN_IDENTITY=- build-for-testing

SMOKE_UDID=$(xcrun simctl create "Crew Native Offline ${SMOKE_ROOT##*/}" 'iPhone 17 Pro')
printf 'Smoke evidence: %s\nFresh simulator: %s\n' "$SMOKE_ROOT" "$SMOKE_UDID"
xcrun simctl boot "$SMOKE_UDID"
xcrun simctl bootstatus "$SMOKE_UDID" -b
xcodebuild -project apps/ios/Zeron.xcodeproj -scheme 'Crew Staging' \
  -configuration Debug -destination "platform=iOS Simulator,id=$SMOKE_UDID" \
  -derivedDataPath "$SMOKE_ROOT/DerivedData" \
  ARCHS=arm64 ONLY_ACTIVE_ARCH=YES \
  -resultBundlePath "$SMOKE_ROOT/OfflineSmoke.xcresult" \
  -parallel-testing-enabled NO \
  -only-testing:ZeronTests/CrewIsolationTests \
  -only-testing:ZeronUITests/SessionFlowTests/testSendEchoesAndStreamsReply \
  CODE_SIGNING_ALLOWED=YES CODE_SIGN_IDENTITY=- test-without-building
```

The importer check preserves exact native IDs, worktree/model, both epoch fields,
attachment bytes and original snapshots; verifies idempotent reopen and foreign
deployment rejection; and leaves unknown commands out of the send queue. The
simulator checks staging identity/profile isolation and demo echo/streaming through
the generated bridge. They do **not** prove authenticated same-bundle migration:
`crewPrepareLegacyBackup` deliberately reverifies Scaffold identity online before
hydration. That separate acceptance requires authorized staging credentials and
retained matching legacy data; do not add an offline authentication bypass.

The integration owner completed an ad-hoc-signed arm64 Debug `build-for-testing`,
including Rust core compilation, UniFFI regeneration and Swift linking. The first
generic-destination attempt failed by requesting x86_64 against the arm64-only
Rust library; the explicit architecture settings above corrected the caller.
On a newly created iPhone 17 Pro simulator running iOS 26.5, all seven
`CrewIsolationTests` and `SessionFlowTests/testSendEchoesAndStreamsReply` passed
(eight tests, zero failures). Evidence is retained at
`/tmp/Crew-Staging-Native-Smoke.tT9JjF/OfflineSmoke.xcresult`, with exported images
under its sibling `Attachments/` directory. The owner inspected both images:
they show completed reply markdown/code/table rendering and the composer, not
mid-stream visual evidence. Existing simulator profiles were untouched and no
credentials were imported. This run does not establish authenticated same-bundle
migration, physical-device acceptance, distribution signing or TestFlight upload.

## Integration acceptance

Run Rust client Crew link and native legacy import tests,
and `ZeronTests/CrewIsolationTests`. Exercise two desktops plus iOS with the
same verified identity: local/remote session discovery, offline-host history,
cold/warm/signed-out links, denied cross-project/deployment/sandbox links,
foreground/background and network recovery, long streaming, cancellation,
send after reconnect, revoked authority, and command-outcome recovery.
The existing upstream mobile layout/network/streaming suites remain intact.
Use authorized remote CI for typechecks; local typechecks are intentionally
not run to preserve workstation resources.
Exercised checks and remaining gates are recorded in
[`docs/UPSTREAM-PORT-PLAN.md`](../../docs/UPSTREAM-PORT-PLAN.md).

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

Tapped notifications must carry matching `userId` and `projectScope`; new Edge
payloads also carry matching `deploymentId`. Existing two-field legacy payloads
remain valid only for that same verified account/project. Bare session IDs and
foreign or malformed scopes are rejected. Cold-start taps retain their scope;
sign-out discards pending notification routes.

APNs uses the existing staging topic `ai.ashler.crew.staging` and team
`825LYXGJR6`. Debug signs with development APNs; Release/archive/distribution
uses production APNs. Existing deployment secrets and App ID capabilities must
already match. The release workflow does not create capabilities, tester groups,
testers, permissions, or public App Store submissions.

## Staging archive, export, upload and readback

The candidate staging release is **1.0 (37)**, not an allocated build number.
The prior authenticated App Store Connect read for app **6808397041**, Crew
Staging by Ashler, showed **1.0 (36) Testing** in **Ashler Internal** (35 and 34
also Testing), and Build Uploads with **All Statuses** showed 36 Complete,
Oct 15 02:55, with no higher upload. The integration owner's fresh read attempt
on 2026-10-21 redirected to login with `authResult=FAILED`; the current browser
session did not authenticate. Build 36/next 37 therefore remains unvalidated
against current metadata. Obtain a fresh authenticated read before allocating
a number or uploading; do not import credentials or bypass authentication.
Existing Xcode account/team signing access was observed;
the LiveActivity profile inventory remains a separate release prerequisite.
`testflight.yml` accepts exact `reviewed_sha` and this explicit `build_number`.
It creates an ad-hoc archive without Apple credentials, never uploads or changes
App IDs/capabilities/groups. Production is not a selectable environment.

Distribution follows the established non-compiling **local Xcode account** flow,
team `825LYXGJR6`, cloud-managed signing. API keys are not a prerequisite. Verify
the exact archive checksum/source/build before export. Both main app and upstream
LiveActivity extension require matching team distribution profiles; inspect actual
account inventory rather than silently creating an App ID or granting capabilities.
The authenticated App ID inventory contains the staging main app and `XCWildcard *`,
but no explicit staging LiveActivity App ID. The extension has no custom signing
entitlements, app groups, shared Keychain or APNs capability; ActivityKit uses
`pushType: nil`. Per [Apple TN3125](https://developer.apple.com/documentation/technotes/tn3125-inside-code-signing-provisioning-profiles),
an extension's concrete signed application identifier may be authorized by a
matching wildcard profile. The verifier allows this only for the capability-free
extension, retaining exact main-app/APNs checks. An existing App Store distribution
profile and successful Apple export still need verification; a wildcard App ID
alone is not proof of distribution readiness.
Read-only local Xcode cache inspection found four unexpired Xcode-managed Team
Store profiles: production, staging, Canary and Canary LiveActivity. All use team
`825LYXGJR6`, expire `2027-09-02 21:36:25`, have no device allowlist and disable
`get-task-allow`. None authorizes staging LiveActivity or a wildcard. The portal
Profiles page listed no manual profiles; neither observation proves that a usable
cloud-managed wildcard profile exists. Do not invoke `-allowProvisioningUpdates`
until the release owner confirms the precise permitted signing/profile changes.

Commands from repository root, only after explicit staging release authorization:

```sh
# BUILD_NUMBER is the next number read from the existing authenticated Apple account.
gh workflow run testflight.yml --ref crew/staging-native-cutover \
  -f reviewed_sha="$REVIEWED_SHA" -f build_number="$BUILD_NUMBER"
# CI executes this archive command; no local compilation is required:
xcodebuild -project apps/ios/Zeron.xcodeproj -scheme 'Crew Staging' \
  -configuration Release -destination 'generic/platform=iOS' \
  -archivePath /tmp/Crew-Staging.xcarchive archive \
  CODE_SIGNING_ALLOWED=NO CURRENT_PROJECT_VERSION="$BUILD_NUMBER"
scripts/ios/prepare-staging-archive.sh /tmp/Crew-Staging.xcarchive "$BUILD_NUMBER"
# Download/extract the verified CI archive before the following local-only steps.
xcodebuild -exportArchive -archivePath /tmp/Crew-Staging.xcarchive \
  -exportPath /tmp/StagingInspection -exportOptionsPlist scripts/ios/ExportOptions-Staging.plist \
  -allowProvisioningUpdates
python3 scripts/ios/verify-staging-ipa.py /tmp/StagingInspection/Zeron.ipa "$BUILD_NUMBER"
# Upload uses the same reviewed archive and existing Xcode account; internal only.
xcodebuild -exportArchive -archivePath /tmp/Crew-Staging.xcarchive \
  -exportPath /tmp/StagingUpload -exportOptionsPlist scripts/ios/UploadOptions-Staging.plist \
  -allowProvisioningUpdates
python3 scripts/ios/verify-staging-ipa.py /tmp/StagingUpload/Zeron.ipa "$BUILD_NUMBER"
```

Record the upload receipt and exact uploaded IPA hash, then read App Store Connect
for this exact bundle/version/build: processing `VALID`, internal beta state and
existing Ashler Internal assignment. Processing alone is not Testing availability.
No tester, group, account-permission or public App Store changes are part of this flow.
If an existing ASC API credential is separately available, the optional read-only
`staging-metadata.py next 37` and `readback BUILD` helpers paginate metadata using
`AC_KEY_PATH`, `AC_API_KEY_ID`, `AC_API_ISSUER_ID` and PyJWT[crypto]. They never
create credentials or change Apple state. Offline checks:
`python3 scripts/ios/staging-metadata.py self-check` and
`python3 scripts/ios/test-staging-release.py`.
The parent integration owner ran the metadata self-check and all three release
contract tests successfully, including wildcard profile matching. The isolated
simulator build and eight tests above also passed. Distribution signing and
upload have not run; current Apple authentication, build-number availability and
the LiveActivity distribution profile remain release gates. This mobile smoke
does not establish that every native suite or the full staging cutover has passed.
