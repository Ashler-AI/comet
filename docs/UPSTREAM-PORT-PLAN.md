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
CI includes native recovery and actual CLI subprocess regressions. These new
changes are locally verified source, not yet an installed artifact.

Coordination request `8cd29fbc-3147-41f0-8d03-e77f99b7cb22` is persisted in
Scaffold session `95e5f344-d332-445d-b91e-dd2e691153f6`. Its prior turn ended with
`503 agent_auth_v2_account_unavailable`; no integration reply is present yet.
The new harness tier's native Crew state/cold-restore contract and isolated
deployment ownership must be confirmed before a shared rollout. No peer source,
existing sandbox or shared deployment was changed for this coordination.

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

## Private Canary managed-startup recovery (October 23)

Retained managed-user credentials preserve synced intent during a temporary
verification failure, without opening a profile or granting authority. The
existing background refresh activates the same scoped profile after recovery;
invalid helper configuration and definitive rejection fail closed. Device
bootstrap still requires verified authority. Headed sign-in stays interactive,
and headless startup retains its shutdown future while waiting, including SIGTERM.

The preserved newer Mac integration passed an isolated native CLI cold-start
HTTP 503 outage: the production refresh recovered after 60,058 ms with the same
process, device, profile and seeded chat, and no temporary local authority.
SIGTERM during the wait exited successfully. No live provider credentials were
used; fixture processes and data were removed. This is local startup proof,
not a claim of deployed natural authority renewal or shared release promotion.

