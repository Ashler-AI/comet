# Crew on zgx: thin port

A thin port of Crew's essential behaviour onto upstream Zeron's current engine and
its new desktop interface (`apps/zgx`). It is a testing lane: it must never touch
the daily-driver Crew Staging install, its profile, or the shared staging backend.

## Pins

- Upstream: `zeronsh/zeron` `fbb4083a4` (main, 2026-11-24, after 0.3.20), merged in as
  `e2f795b28` and `b1ebb2133` on 2026-11-25 ("Merged upstream 0.3.20", below). It was
  `e393c4a84` (2026-11-19, merged in as `a3dad815f`) until then.
  The port started from `94ae5b10812abb854704bc014c2bcebdec64eac9` (v0.3.4, 2026-11-13);
  the 241 commits since (v0.3.8, the GPUI app's removal in #1499) merged with 19
  conflicts, 18 hunks of them content.
- Crew: `Ashler-AI/comet` `e886844d4` (0.2.157, 2026-11-24). It was `888bd9447` (0.2.155)
  until 2026-11-25, and `1932df84d` (0.2.151) until 2026-11-24. Crew's sandbox behaviour
  is ported:
  - #231: a command is never relayed to a sandbox's host;
  - #237: a send to a paused sandbox resumes it;
  - #243: a deliberate headless stop publishes each hosted chat's final tail
    (`crew_final_tail.rs`, "Synced with Crew 0.2.157" below);
  - #242's aim, that a send to a paused sandbox reaches its resumed host, through the
    engine's wake (#237's), which now also wakes on a send the Edge only queued;
  - #239, crew-web's paused view: crew-web comes in the Linux archive, which now ships
    every file its contract lists.

  Not ported:
  - #233, client-side bandwidth fixes in upstream's own engine files (chat2 tail
    uploads, diff sidecars) and in `crew_directory`, which the port hasn't;
  - #234, Antigravity's computer use, and #240, managed Antigravity's persistent home
    (no managed Antigravity here);
  - #227, the app's name and link scheme, which the port has its own of;
  - #242 itself, which is the GPUI composer's.

  Crew main up to `8dac44a82` (0.2.159, 2026-11-27) adds only #245 and #247, the GPUI
  composer's queued sends to a Scaffold session. Neither touches the engine, the
  harness, crew-web or `scripts/crew`, so the host and its Linux archive need nothing
  new. The port's engine does the same as those two: see "Checked against Crew
  0.2.159" below.
- Platform: the task branch `czhen/crew-zgx-harness` sits on `ashler-platform`
  `master` `51a030e31` (rebased 2026-11-25, from 2026-11-24's).
- Branch: `crew/zgx-thin-port`, on `origin` (pushing it is approved).
- Crew's last upstream import is `a0468502a` (v0.2.129+1, squashed by PR #152),
  not the git merge base v0.2.113. Compare each Crew file with that version, and
  audit dropped Crew lines with `scripts/audit-upstream-port.py` from the Crew
  repository. The evaluation behind continuing this port, rather than merging
  upstream into Crew main, is [zgx-migration-evaluation.md](zgx-migration-evaluation.md).

## Principles

These apply `docs/UPSTREAM-PORT-PLAN.md` §7 from the Crew repository to zgx.

- Upstream owns sync, persistence, the registry, rendering, harness lifecycle,
  settings and the zgx interface. Crew owns its auth and control-plane adapters,
  managed inference context, scoped lifecycle operations and distribution.
- Crew code lives in Crew-named files: `crew_*.rs`, Crew modules under `edge/src`,
  `docs/crew/`, `scripts/crew/`. Upstream files get the smallest seam edits that work.
- Interface changes go into Crew built-in zgx plugins (`apps/zgx/plugins-builtin/crew-*`)
  or configuration switches before any upstream screen is edited. Upstream ships
  its own Git and GitHub features as built-in plugins.
- Use upstream's hooks instead of threading Crew types through upstream structs:
  `RunEnvironment` for Agent Auth, the Cloud RPCs for Scaffold sandboxes.

## Isolation rules

1. Agent shells on this Mac inherit Crew Staging's environment: `ZERON_DATA_DIR=~/.crew-staging`,
   `ZERON_IPC_PORT=27957`, `ZERON_MANAGED_ENGINE=1`, `ZERON_APP`, and the `COMET_*`,
   `CREW_*` and `OMP_*` variables (tokens included). Never build, test or run an engine
   or zgx with that environment: use `scripts/crew/clean-env`.
2. This build's identity is its own (`crates/cli/src/crew.rs`):
   - A development build uses data `~/.crew-zgx-dev` and IPC port 27960.
   - The packaged canary (`CREW_FLAVOR=zgx-canary` at build time) uses
     `~/.crew-zgx-canary` and port 27959.
   - Both use sign-in callback port `0`, their own app id, bundle id, URL scheme
     and launchd label, and no updater.
   - The default Edge is a loopback port nothing serves, so an engine stays
     signed out and local until a real Edge is named.
3. Hard refusals, in the engine and in zgx, whatever the environment says,
   with no variable that allows them:
   - ports 27654, 27655, 27871 and 27954–27958;
   - any data dir with a folder on its path named `.crew…`, `.comet…` or `.zeron…`,
     other than `.crew-zgx-…`. Symlinks are resolved and `..` is folded. That covers
     `~/.crew`, `~/.crew-staging`, `~/.crew-upstream-canary`, `~/.comet-native*`,
     `~/.zeron`, `~/.zeron-staging` and whatever lies inside them.
   - Engines started by zgx or `crates/launch` drop inherited `ZERON_*`, `COMET_*`,
     `CREW_*`, `OMP_*`, `CLAUDE_CODE_*` and the agent session's `ANTHROPIC_*` route
     first (`crates/launch/src/crew.rs`).
   - Upstream's `~/.comet-native` → `~/.zeron` data-dir move is removed.
4. Agents run with a home of their own: `<data>/home` unless `ZERON_ZGX_ENGINE_HOME`
   names another. The real `~/.claude`, `~/.codex` and `~/.zeron` are untouched.
5. The backend is a local Edge (`wrangler dev`, its own environment) built from this
   branch. A v0.3.4 engine must never sync through `comet-staging.internal.ashler.com`:
   it would become a second writer in rooms that v0.2.149 clients use, including
   registry kinds they do not know (`preferences`, `sidebarPins`, `repositoryLooks`).
6. No pushes, pull requests, CI runs or deployments without the owner's explicit approval.

## Slices

| # | Slice | Scope | Exit gate |
| --- | --- | --- | --- |
| 0 | Baseline | Branch from the pinned upstream commit; this plan | Done |
| 1 | Isolation and flavour | Crew flavour in `crates/cli` (paths, env defaults, no `~/.comet-native` → `~/.zeron` move); refusals in the engine's config and zgx's engine manager; environment scrub in zgx and `crates/launch`; zgx app id; engine builds without gpui (`ui` feature); upstream CI workflows removed; `scripts/crew/{clean-env,build-engine,dev-zgx}` | Unit tests prove the refusals and the scrub; the engine refuses a dirty shell's `~/.crew-staging`:27957; dev zgx runs on `~/.crew-zgx-dev`:27960 with a local agent turn |
| 2 | Crew sign-in (engine) | Copy `scaffold_auth.rs` and the profile/token types; Scaffold mode in `auth.rs`, `EngineConfig`, `resolve_profile`, `build_auth` (callback 0); Crew runtime config resolution; refuse `AccessToken`; no dev-auth fallthrough on an empty WorkOS client id | Engine auth tests pass; the test engine signs in with Scaffold and reports `SignedIn{orgId = projectScope}` |
| 3 | Edge | Upstream v0.3.4 edge plus Crew's auth stack (`auth.ts`, `auth-routes.ts`, `auth-envelope.ts`, `scaffold-session.ts`, `authority.ts`, `registry-scope.ts`, `peer-route.ts`, `native-grant-epoch.ts`); grafts in `index.ts`, `forward.ts`, `chat-room.ts`, `registry-room.ts`, `device-room.ts`, `preview-route.ts`; WorkOS, cloud, vault, runner and plugin-key routes off; `CHAT_RELAY_LAYER` unset; local wrangler environment | Edge unit tests pass; the signed-in test engine joins its registry and chat rooms on the local Edge |
| 4 | zgx Crew mode | Crew endpoint (callback 0); `session_exists` also accepts `scaffold-session.json`; username, orgs, onboarding, card and org choice answered locally; `account.provider = "scaffold"`; one Crew onboarding path; hide Members and profile edits | Headed: sign in, finish onboarding, open the main window, run a local agent turn in an isolated home |
| 5 | Agent Auth | Copy `inference_relay.rs` and the Agent Auth client; a Crew `RunEnvironment` (`crew_inference.rs`) that fails closed; credential stripping and Claude/Codex overrides; managed model catalog with a `crewAccountId` option; session replacement on account change | Real Claude Code and Codex turns from zgx go through Agent Auth; every other path fails closed |
| 6 | Scaffold | Crew adapter behind `CloudStatus`, `ListGithubRepos`, `ListCloudBranches`, `CloudSessions` and `cloud_chat_host`; trimmed `scaffold.rs`, `crew.rs` command ledger, `rpc/crew.rs`; `cloud-session` capability for sandbox hosts | From zgx's Cloud row: create and attach a sandbox session, send, stream, interrupt |
| 7 | Packaging | Signed Crew zgx test app from a build-only CI run (adapted `scripts/package-zgx-macos.sh`) | Needs approval; installs beside Crew Staging without sharing anything |

Deferred:
- OMP: the test flavour defaults to Claude Code. OMP is needed before any cutover, for imported history and sandboxes.
- Scaffold pause/resume/stop/recovery, handoff, devbox/Boat.
- Collaboration and annotations, settlement, imports and backups.
- Deep links, Crew update channels, iOS, web.

## Seam notes

### Sign-in and Edge

- **Copy:**
  - Crew's `crates/sync/src/scaffold_auth.rs`, plus `base64`, `percent-encoding`, `chrono` and `tempfile`.
  - `EngineProfile::scaffold`, `scaffold_store_root`, `ScaffoldStoreBinding`, `ProfileTokens` and `DeviceGrantAuthority`. Upstream `profile.rs` is unchanged since v0.2.113.
- **Edit** upstream `crates/engine/src/auth.rs`; don't replace it. Upstream added runner mode, step-up, `SignInOptions` and account deletion since the fork. Add a Scaffold branch to:
  - `new` and `detect`;
  - `start_sign_in_with`, `complete_sign_in` and `clear_session`;
  - `refresh_serialized`, `spawn_refresh_loop` and the loopback handler.

  Org creation and selection refuse in Scaffold mode.
- **`rpc.rs`:** refuse `AccessToken` in Scaffold mode. The `sc_rc_` bearer lasts until revoked and carries exec and lifecycle scopes. zgx's direct Edge calls (`/username`, `/onboarding`, `/auth/orgs`, `/cards`) are answered locally instead.
- **As built (slice 2):**
  - Crew logic sits in `crates/engine/src/crew_auth.rs` and `crew_profile.rs`, child modules of
    `auth` and `profile`; upstream's structs gain one `crew` field each.
  - `scaffold_auth.rs` is Crew's file except for one thing: it defines its own `AccessToken`,
    because upstream's `zeron_proto` has none. `zeron-sync` gains `base64` and `chrono`;
    `percent-encoding` isn't needed, and `tempfile` stays a dev-dependency.
  - The authority comes only from `<data_dir>/crew-runtime.json` (`edgeUrl`, `scaffoldUrl`,
    `projectScope`, optional `deploymentId`), read in `apps/zeron/src/crew_runtime.rs`.
    No environment variable names it. Its `edgeUrl` replaces `ZERON_EDGE_URL`, and an unusable
    file stops the engine (exit 78). So does an `edgeUrl` that is another Crew's Edge (Crew
    Staging's, Crew's or Crew Canary's local one): rule 5. Without the file, the engine is unchanged.
  - Signed in, `AuthStatus` is `{state: signedIn, user: {id, email, name}, orgId: <projectScope>}`.
    The credential is `<data_dir>/scaffold-session.json` (0600).
  - Sign-in uses a loopback callback on an ephemeral port (`http://127.0.0.1:<port>/callback`),
    both headed and headless. `SignIn` ignores its hints. `CompleteSignIn` takes the callback URL
    or `state.code`.
  - In Scaffold mode, `AccessToken`, `StepUp`, `DeleteAccount`, `CreateOrg` and `SelectOrg`
    answer `RpcError::Capability("crew.scaffold_unsupported")`. `ListOrgs` answers `[]`.
    Cloud, vault, runner and plugin keys are off.
  - Dev mode only with an explicit `ZERON_EDGE_TOKEN`, never because the WorkOS client id is
    missing or empty, and never from the Edge's `/health`. The one exception is the dev identity
    of an `EngineCore` that never had auth attached (tests and embeds); it is set explicitly.
  - `crates/ui/src/state.rs` (the old gpui app) gets `scaffold: None, device: None` in its
    `EngineConfig` literal, and no Crew mode.
- **Edge protocol** that v0.3.4 expects and Crew's edge lacks:
  - `/chat2/:id/checkpoint-part` and its staged commit;
  - the attachment blob route;
  - caller-device stamping;
  - registry `/chat-host`;
  - unauthenticated `/releases/*`, which the updater and harness-compatibility fetch use.

  Relay frames stay off until the authority checks cover them.

### zgx Crew mode

- Every development build of this branch is Crew's (`apps/zgx/src/crew.rs`,
  `crew::MODE`; the `beta` feature stays upstream's and is not built here).
- **Endpoint:** no WorkOS client, sign-in callback port `0`, and the Edge named
  by `crew-runtime.json` in the engine's data dir, else the loopback default.
  `ZERON_ZGX_ENV`, `ZERON_ZGX_EDGE`, `ZERON_ZGX_WORKOS_CLIENT` and
  `ZERON_ZGX_CALLBACK_PORT` do nothing. Upstream's staging and production
  tables stay in `src/auth.rs`, unreachable.
- **Engine start:** `ZERON_EDGE_URL` and `ZERON_WORKOS_CLIENT_ID` are removed,
  never set. `ZERON_CALLBACK_PORT=0` is passed.
- **Session:** a launch counts `session.json` or `scaffold-session.json` as kept.
- **Account calls:** `crew::install` runs after `auth::install` and
  `settings::install` in `main.rs` and replaces their actions:
  - `username` and `card` answer nil;
  - `orgs` answers the session's `orgId` (else `projectScope`) with role `member`;
  - `choose_org` accepts only that project and never calls `SelectOrg`;
  - `me` comes from the session;
  - setup's pages are kept in `crew-onboarding.json` in zgx's app data dir;
  - username claims, org creation, invites and profile edits answer `refused`.
- **Fail-closed seam:** any other Edge call (members, photos) is refused in
  `Auth::bearer` before `AccessToken` or a request.
- **Scripts:** the host publishes `crew` (`ui/account/crew.luau`) and
  `account.provider = "scaffold"`.
  - Signed-in setup takes `ui/screens/onboarding/crew.luau`'s path from
    `state.luau`'s `way()`.
  - Settings swaps Profile for `settings/crew_account.luau` (same id) and
    drops Members, from the index too.
  - Window titles and About read "Crew (zgx dev)".
- **Updates:** no release feed yet (update channels are deferred), so a Crew build
  starts no updater (`about.rs` `Desktop::start`): nothing asks an Edge for Zeron's
  releases from the app. About says a canary is updated by installing a newer build,
  shows no check, beta channel, release notes or website, and still calls a canary
  packaged. The engine's own updater still asks its Edge (the local one refuses,
  401).
- **Links:** a Crew build's links are in its own scheme (`zeron_cli::crew::URL_SCHEME`,
  `crew-zgx-canary` for the canary, which its bundle registers). The app and the
  engine call `zeron_notify::set_scheme` as they start, so a chat's "Copy link", a
  clicked banner and an opened link all use it. A `zeron://` link isn't followed.
  The app takes links from macOS under its own id (`zeron_cli::crew::BUNDLE_ID`; only
  upstream's beta did before). Checked in the canary: a `crew-zgx-canary://` link
  opened its chat, and a `zeron://` one was refused.
- **Pointing at an Edge:** use the engine's `crew-runtime.json`; `scripts/crew/dev-zgx`
  passes no Edge. `ZERON_ZGX_PRIVATE_SIGNIN=1 scripts/crew/dev-zgx` signs in in
  a private Chrome window. The script passes that variable through `clean-env`.

**Headed check** (slice 4's exit gate), once slices 2 and 3 are on this branch:

1. Run `scripts/crew/build-engine`.
2. Write `~/.crew-zgx-dev/crew-runtime.json` with `edgeUrl` (the local `wrangler dev`
   Edge), `scaffoldUrl` (the authority: decision 1), `projectScope` and `deploymentId`.
3. Run `scripts/crew/dev-zgx onboarding`. With no argument it opens the main
   window and skips setup.
   - stderr says `zeron-zgx: account: crew (<edgeUrl>, callback port 0)`.
   - The window title is "Crew (zgx dev)".
4. Sign in. Setup should go Sign in → Make it yours → agents → "<project> is
   ready" (Member) → main window.
5. Check Settings:
   - Account shows the email and "In <project>.", with no photo, name, title,
     username or delete.
   - There is no Members page.
   - About says "Crew (zgx dev)".
6. Check the engine's environment. Get its pid from
   `lsof -nP -iTCP:27960 -sTCP:LISTEN`, then run `ps eww -p <pid>`.
   - It shows `ZERON_CALLBACK_PORT=0`.
   - It shows no `ZERON_EDGE_URL` and no `ZERON_WORKOS_CLIENT_ID`.
7. Run a local Claude Code turn. Its login lives in `~/.crew-zgx-dev/home`.
8. Quit and relaunch `scripts/crew/dev-zgx`. The main window comes back signed
   in, and it never shows signed out while the engine starts: the engine's
   `scaffold-session.json` counts as a kept session.

### Local Edge (slice 3)

- **Layout.** Crew's auth stack is in `edge/src/crew/`. `crew/router.ts` answers
  first through one call in `index.ts`: it authenticates, authorizes the route,
  answers Crew's own routes and hands upstream's room routing a trusted request.
  The rooms (`chat-room`, `registry-room`, `device-room`, `preview-room`) carry
  marked `Crew:` grafts.
- **Unrouted:** WorkOS and upstream's `/auth/*`, the user directory, usernames,
  cards, onboarding, plugin keys, step-up, the vault, Cloud, runners, `/admin`,
  billing, `/models/v1.json` and the public `install.sh`. These answer 401
  without a bearer and 404 with one. The legacy session and workspace lanes
  answer 410. The relay layer is off in any Crew deployment, whatever
  `CHAT_RELAY_LAYER` says.
- **Upstream's newer lanes:**
  - `checkpoint-part` and the staged commit need a writable authority.
  - Attachment blobs follow the session's blob rule. A sandbox host may also
    DELETE an attachment it collects.
  - Registry `/chat-host` is never reached: chat rooms get no org header.
  - Caller-device proof is closed: the Worker never stamps `fromDevice`.
  - `/releases/*` is Crew's authenticated feed: 503 without its bucket. The
    engine's unauthenticated harness-compatibility fetch gets 401 and keeps its
    built-in list.
- **Run:** `scripts/crew/clean-env npm --prefix edge run dev:crew-zgx` serves
  `http://127.0.0.1:27961` (environment `crew-zgx-local`). Durable Objects and
  R2 are local, with no routes or crons, and wrangler's telemetry and `cf.json`
  fetch are off. Point the engine at it with `"edgeUrl": "http://127.0.0.1:27961"`
  in `~/.crew-zgx-dev/crew-runtime.json`; zgx's Crew mode passes no Edge of its own.
- **Variables:** put these in `edge/.dev.vars.crew-zgx-local`, which is never committed.
  - `SCAFFOLD_CONTROL_PLANE_URL` is the Scaffold authority (decision 1). The
    default is a loopback port nothing serves, so `sc_rc_` bearers answer 503.
  - `SCAFFOLD_PROJECT_SCOPE` must equal the engine's project scope. Default
    `crew-zgx-local`; for staging Platform sandboxes it is `ashler-staging`.
  - Optional: `SCAFFOLD_DEPLOYMENT_ID`, and `SCAFFOLD_SESSION_FRESH_MS` and
    `SCAFFOLD_SESSION_GRACE_MS`.
  - The only secrets are the optional release feed's: `COMET_RELEASES_GCS_BUCKET`,
    `GCP_RELEASE_SERVICE_ACCOUNT_EMAIL` and `GCP_RELEASE_SERVICE_ACCOUNT_PRIVATE_KEY`.
- **Offline:** add `-- --var AUTH_MODE:dev --var ENVIRONMENT:local`. The bearer
  is then `user@crew-zgx-local`, and nothing is verified.
- **Tests:** `npm run test:unit`, `npm run test:workerd` (upstream's tier) and
  `npm run test:crew-workerd` (Crew's), each through `scripts/crew/clean-env`.

### Staging Edge

- This branch's Edge runs as its own Worker, `crew-zgx-edge-staging`, at
  `https://crew-zgx-edge-staging.cgoggins.workers.dev` (first deployed by run 44861439161
  on 2026-11-20). It has its own Durable Objects (migration `crew-zgx-staging-v1`) and its
  own bucket (`crew-zgx-staging-blobs`), with no routes or crons, on Crew Staging's Scaffold
  authority (project and deployment `ashler-staging`).
- **Config:** `edge/wrangler.crew-zgx-staging.jsonc`, standalone, because an environment
  would inherit upstream's account, routes and migrations.
- **Deploy:** push a `crew-zgx-edge-staging-*` tag. `.github/workflows/crew-zgx-edge-staging.yml`
  runs in the `comet-staging` environment: the Edge's unit and Crew workerd tests on
  Node 24, `scripts/crew-zgx-staging-guard.mjs`, `wrangler deploy`, then a `/health` check.
  The token can replace any Worker in Crew's account, so the guard refuses any config that
  names anything else.
- **Pointing a build at it:** set `crew-runtime.json`'s `edgeUrl` to the origin. A
  `scaffold-session.json` is tied to its Edge, so sign in again, in a fresh data dir: rooms
  don't carry over from a local Edge.

### Agent Auth

- Use `RunEnvironment::prepare(harness, chat_id, &mut RunRequest)` (`crates/engine/src/registry.rs`).
  It runs on every spawn, including turns, idle starts and titles. An error becomes
  `HarnessError::NotSignedIn`, and `RunRequest::env` is never serialised.
- This deviates from the playbook's route context in `RunControls`, which means 58 struct
  literals in 39 files that conflict on every upstream update. Needs owner sign-off.
- Upstream has no "run ended" callback, so routes retire when replaced or at shutdown.
- **Paths around the managed route in v0.3.4** must fail closed in managed mode:
  - account pins and plugin account switching;
  - the composer's device-wide account switch;
  - Codex voice;
  - CLI model probes.
- **Platform endpoints:**
  - `POST /api/agent-auth/v2/authority`;
  - `POST /api/agent-auth/v2/{messages,responses}`;
  - `GET /api/agent-auth/routes/{session}`;
  - `/api/agent-accounts`.
- **As built (slice 5), Claude Code and Codex only:**
  - Engine code is in `crates/engine/src/crew_inference.rs`, with `crew_agent_auth.rs`
    (the Agent Auth client, from Crew's `ScaffoldClient`) and `crew_inference_relay.rs`
    (Crew's `inference_relay.rs`) as its child modules. The harness side is
    `crates/harness/src/crew_inference.rs`.
  - Managed means a Scaffold engine: `auth.scaffold_enabled()`, signed in or not. It is
    installed in `Engine::assemble_runtime` before the core is assembled, so boot
    recovery's runs are managed too. It is then the registry's only run environment:
    `PinRunEnvironment`, Muse's and plugins' `run.preparing` pins never run. An engine
    without a Scaffold authority is unchanged.
  - The authority is the profile's `ProfileTokens` only. The client talks to
    `crew-runtime.json`'s `scaffoldUrl` and nothing else, and never follows redirects.
  - Each spawn of Claude Code or Codex gets a route for its chat, the chat's
    `crewAccountId` model option (else Agent Auth's choice) and its provider.
    `RunRequest::env` gets only `ZERON_CREW_INFERENCE` (the provider),
    `ZERON_CREW_INFERENCE_URL` (the relay), `COMET_INFERENCE_TOKEN`, and the CLI's own:
    `ANTHROPIC_BASE_URL` and `ANTHROPIC_AUTH_TOKEN` with Bedrock, Vertex, Foundry and
    nonessential traffic off, or `OPENAI_BASE_URL` and `OPENAI_API_KEY`. The token is a
    loopback token; Agent Auth's authority stays in the engine.
  - A chat with no model gets the managed default (`claude-opus-5-5`, `gpt-6-sol`),
    which `prepare` writes into the request.
  - Titles (no chat) run in a titling session of their own per harness, on Agent
    Auth's choice of account.
  - The same chat and account keep one route and token, so a warm start and its run,
    or a parked session, launch alike. Another account or provider retires the chat's
    earlier routes. A retired token answers 410, or follows the chat's current route
    for the same provider, as in Crew. Shutdown retires every route and stops the relay.
  - `credential()` is a fingerprint of the chat's account (from its workspace row, else
    its current route) and the route's serial. Choosing another account replaces a
    parked session before its next turn.
  - Harness seams: `apply_run_env` strips Crew's list of inherited provider credentials
    (`STRIPPED`) when the env carries a route. Claude Code also gets `apiKeyHelper: ""`
    and `forceLoginMethod: "console"`, and a route that isn't Anthropic's stops its
    launch. Codex gets the `crew` model provider (`-c` overrides; the token never on
    argv), `modelProvider: "crew"` on `thread/start` and `thread/resume`, and no
    realtime voice. A route for another provider stops the launch.
  - Upstream seams: `registry.rs` (one `crew` field; `environments()`;
    `discover_models_with_lease`), `lib.rs` (install, follow the workspace, shutdown),
    `rpc.rs` (`ListModels`, the account calls handed to `crew_accounts.rs`,
    `prepare_voice_bridge`) and
    `plugin_agents.rs` (`activate`, `models`, `pin_mechanism`, `set_pin`). The engine
    gains `hyper`, `hyper-util` and `http-body-util`, which `zeron-mcp` already builds.
- **Fails closed**, in managed mode:
  - Harnesses other than Claude Code and Codex: every spawn and `ListModels` refuse.
  - Signed out, Agent Auth refusing or unreachable, a stopped engine, or no authority:
    the spawn errors, titles included, and nothing is put in its environment.
  - `ActivateAgentAccount`, plugins' `accounts.switch` and pins refuse.
    `pin_mechanism` reports nothing pinnable.
  - Codex voice: `prepare_voice_bridge` answers `Unsupported`, and a managed Codex run
    never attaches realtime.
  - Model lists (`ListModels`, plugins' `models`, titling's model choice) are each
    harness's own, as for an upstream engine: Claude Code's `initialize` and Codex's
    `model/list`, through upstream's catalog cache. The probe runs as a managed launch
    would (`zeron_harness::crew_inference::set_managed_discovery`): on a placeholder
    route, with no inherited credential, and Codex on the `crew` provider. It lists the
    CLI's models without reading this device's login. Offline and logged out, Codex
    0.169.1 lists 11 models, `gpt-6.2-sol` its default.
  - `CrewInference::offer` keeps what Agent Auth can route (`relay::routable`: no other
    provider's ids). Each model gets an Account option: "Automatic · shared pool", then
    each healthy login from `/api/agent-accounts` (cached a minute, 5 s at most,
    deduplicated as Crew's settings do). No model is named in Crew's code: one a CLI
    release adds is offered as it arrives.
  - A run that names no model runs its harness's own default, kept from its last list
    or asked of the harness then. Titles use upstream's cheapest-model rule over the same
    lists (Haiku for Claude Code; "luna", Codex's small tier, added to the rule).
  - **Development switch:** `COMET_INFERENCE_MODE=local` (or `ZERON_INFERENCE_MODE`), as
    Crew's launcher reads it. Launch the app with it through `scripts/crew/clean-env`,
    with no engine of the flavour running. The app hands it to the engine it starts,
    which then runs agents on this device's own logins as upstream does, and setup and
    Settings → Providers offer own logins again (the host's `crew_managed`).
- **Checked, left as upstream:** Auto's reviewer runs inside the agent's process
  (Claude Code's classifier, Codex's `auto_review`), so it uses the route. Command and
  skill discovery start the CLI's `initialize` handshake, with no model turn. Local
  logins, API keys and usage reads remain; managed runs never see them.
- **Deferred:**
  - OMP, Antigravity, Crew's Boat admission gate, and sandbox (device) hosts. A device
    host is managed, but its grant is untested against Agent Auth.
  - `GET /api/agent-auth/routes/{session}` (route receipts): no surface shows them yet.
  - `warm_signed_out` still reads the CLI's own login, so a managed chat may skip a
    warm start.
  - Codex features that call OpenAI's backend with a ChatGPT login outside the model
    provider, if any, aren't blocked; the isolated home has no such login unless one is
    made there.

### OMP (the Agent Auth phase's remainder)

Crew's default agent under Scaffold is OMP, Crew's build of the Pi coding agent
(`omp`, oh-my-pi). Upstream has no OMP harness. Its Pi driver can't drive OMP: against
`omp` 18.22.1, `--no-themes` and `--session-id` aren't OMP flags, `get_commands` doesn't
exist, and turns end on `agent_end`, not `agent_settled`. So:

1. `HarnessId::Omp` (slug `omp`), with its arms in upstream's matches. Crew rows that
   name `omp` must deserialize.
2. A Crew driver, `crates/harness/src/crew_omp/` (Crew main's `omp/` trimmed to managed
   turns): RPC startup on its `ready` frame (chunked frames when offered), `agent_end`,
   goal and todo rows, host-tool replies, `--mode rpc --approval-mode yolo`, `--resume`,
   the screenshot cap (Agent Auth's 8 MiB), and every model role pinned to the routed
   model. Left out for now: the supervisor (upstream reaps agents), the journal writer
   check, fork, Google, and the sandbox profile.
3. The managed route chosen per model, not per harness: an OMP model's provider
   (`anthropic/…` or `openai/…`) picks the relay route. The relay already serves
   `/v1/models`, `/v1/messages` and `/v1/responses` with bearer or `x-api-key`. OMP gets
   `auth_gateway.ts` (`comet-anthropic` and `comet-openai` providers on the relay), with
   its guard exiting rather than throwing, and never `COMET_INFERENCE_TOKEN`.
4. Native model lists: `omp models --json`, probed as a managed launch (placeholder
   `ANTHROPIC_API_KEY`/`OPENAI_API_KEY`, base URLs nothing serves). Offline and signed
   out, 18.22.1 lists 84 Anthropic and OpenAI models, `claude-opus-5-5` and `gpt-6.2-sol`
   among them. With nothing set it lists none. Rows carry `cost`, so titles can take the
   cheapest. No row is marked default.
5. `omp` in `MANAGED` (crew.luau), its mark, ported tests, and the exit gate: real
   Anthropic and OpenAI OMP turns from zgx through staging Agent Auth. Then OMP becomes
   a Scaffold engine's default, as in Crew.

Status on 2026-11-20:

- Steps 1–5 are built (`13891d510`, `7128ab04d`). OMP wears Pi's mark. A chat that names
  no model runs Claude Code's own default on OMP's Anthropic provider.
- The exit gate passed in the installed canary, through staging Agent Auth:
  - "OMPPONG" on `comet-anthropic/claude-opus-5-5`, and "OMPOPENAI" on
    `comet-openai/gpt-6.2-sol`;
  - `omp --mode rpc --approval-mode yolo --extension …/agent-auth.ts`, with the gateway
    on the engine's relay and no `COMET_INFERENCE_TOKEN`;
  - the engine's only outside connection was scaffold-staging.
- The model was set through the composer's `chat_model` state, because popups weren't
  opening for the backgrounded app at the time.
- Settings → Providers shows OMP's managed card. `ListModels` gives 84 models (21
  Anthropic, 63 OpenAI).
- Open:
  - Speed: a turn in an existing OMP chat answered in about 4 s by the wall clock, and
    OMP's journal put its own turn at 2 s. The devtools `wait`'s "met after" figures
    (70 s, 34 s) overstate time for a backgrounded window, as they did for Codex. Command
    and skill discovery in the project's worktree take 3 s and 1 s.
  - OMP isn't the default yet. In zgx the composer names each new chat's agent (the
    engine's `default_harness` only fills chats without a config row), so making OMP the
    default means changing what every new chat starts on: the owner's call.
  - Zeron's MCP tools aren't wired into OMP, as in Crew.

### Shared accounts

Crew's Shared accounts page, as zgx has it. Agent Auth's logins (`/api/agent-accounts`)
are the accounts every managed agent runs on, so a Crew build lists and manages them in
Settings → Providers.

- **App** (`ui/screens/settings/crew_accounts.luau`, one seam in `providers.luau`): a
  "Shared accounts" section after the agents, on this computer's page. It has a Claude card
  and a ChatGPT card, built from upstream's `ProviderCard`:
  - each account with its plan and two usage meters, its "5-hour" and "Weekly" windows by
    name. A window Agent Auth hasn't reported shows 0: most staging accounts report only a
    Weekly one, which upstream's first-window rule would also show as the session.
    After the plan: how it stands with Agent Auth when it can't be used ("Needs
    attention", "Disabled", "Revoked"), and in words any other window, a model's or
    tier's own ("Opus weekly 80%"), or "Usage unavailable";
  - no "In use", nothing to switch to, no API keys and no banked resets: a chat runs on
    one automatically, or on the one its Account option names;
  - adding one is its agent's sign-in (Claude Code's paste-code login, Codex's browser
    login), shown on the card as upstream shows it;
  - removing one asks first ("Remove this shared account?").
- The cards read the agents' rows: Claude's from `claude-code`, ChatGPT's from `codex`.
  OMP's rows carry the same accounts again, by provider, so they're not shown twice.
  OMP's card, like the others, says it's on Crew's shared accounts.
- The model picker has no Account row or usage ring of upstream's for a managed agent.
  A chat's account is the Account option (`shared.luau`'s `model_accounts`).
- The host passes an account's `usageError` (`error`) and its named windows with their
  resets (`windows`) to its row. For a Crew build it also shows the engine's words when a
  shared sign-in fails (`crew::shared_accounts`).
- **Engine** (`crates/engine/src/crew_accounts.rs`, `crew_shared_login.rs`): one seam in
  `rpc.rs` hands a managed engine's account calls to Crew's code. Every other engine
  answers them as upstream does.
  - `ListAgentAccounts` lists `/api/agent-accounts` (for a minute; `forceUsage` lists again).
    Each login is shown once, through a healthy copy: Crew's `login_key` and
    `shared_logins`, ported.
    - Rows: an Anthropic login is Claude Code's and OMP's (`provider: "anthropic"`); an
      OpenAI login is Codex's and OMP's. None is active or switchable.
    - Plan windows come in the order the app reads them: the family's 5-hour, then its
      Weekly, then a model's or tier's own ("Opus weekly", "5-hour (Spark)").
    - A failed list gives no rows and a warning per agent.
  - `StartAgentLogin` `{harness, provider?}`:
    - Claude Code, or OMP with `anthropic`, starts Claude's paste-code sign-in.
    - Codex, or OMP with `openai`, starts ChatGPT's browser sign-in: upstream's Codex
      capture, in a throwaway home.
    - Signed out of Crew, it's refused before a browser opens.
  - `CompleteAgentLogin` and `PollAgentLogin` import the capture
    (`POST /api/agent-accounts/import`). Nothing is written to this device: no slot, no
    Keychain item, no CLI store.
    - A code is redeemed once. A failed import keeps the tokens in memory (15 minutes at
      most) for the next poll or completion. The app's "Try again" starts a new sign-in.
  - `ForgetAgentAccount` revokes the login and every saved copy
    (`DELETE /api/agent-accounts/{id}`).
  - An import or a revocation lists again, and the chats' Account option follows.
  - Refused with a sentence: `ActivateAgentAccount`, `RenameAgentAccount`, `AddAgentApiKey`,
    `VaultPutApiKey` and `ConsumeCodexResetCredit`, and any account call relayed from
    another device.
  - `agent_accounts.rs` gains the module and one field: Claude's capture carries
    `accountUuid`, Agent Auth's key for it.

### Scaffold sandboxes

**Plan (2026-11-20, owner-approved staging deploys).** Staging Platform pins one Crew Edge
and one engine image per harness: a request picks only a region. So the port runs its own
staging harness region, `harness-zgx`, whose image carries this branch's engine and whose
Edge is this branch's (`https://crew-zgx-edge-staging.cgoggins.workers.dev`, deployed:
"Staging Edge"). The pieces:

1. **Linux archive** of this branch's engine, as Platform installs Crew's
   (`comet-<v>-linux-x86_64.tar.gz`: `comet` with the engine linked in, Crew main's
   `apps/web` files as `crew-web/`, `crew-web-contract.json` with `crew.native.web.v1`,
   and a `scaffold-manifest.json`, `schemaVersion` 2, `scaffold.comet-runtime.v2`).
2. **Sandbox host** in the engine: `comet --data-dir .scaffold/crew-data --ipc-port 39400
   headless --device-bootstrap-file F --edge-url E --scaffold-url S --project-scope P
   --deployment-id D` (the harness's native runner accepts exactly this), the bootstrap
   (`cg1.` grant to a `cs1.` token), `scaffold-device-session.json`, `scaffold-authority`
   for the desktop's probe, the device guard, OMP as the sandbox host, managed inference on
   the device token, and exit 0 on SIGTERM within 30 s.
3. **Desktop**: a Crew Platform client (create with `region: "harness-zgx"`, list,
   inspect, exec, files, the Edge's device grant, attach), a command ledger for Create and
   Attach (a create is idempotent by session id; reconcile only observes), a Crew adapter
   behind upstream's Cloud RPCs (`CloudStatus`, `CloudSessions`, `ListGithubRepos`,
   `ListCloudBranches`, `cloud_chat_host`), and the relay pre-filter (one refused frame
   ends the whole device link). The zgx composer passes the chat's config to the Cloud
   create.
4. **Platform** (a task branch, then `scaffold-staging`): the control plane picks a
   region's own Edge on staging only (`SCAFFOLD_CREW_REGION_EDGE_URLS`); a second harness
   release for `harness-zgx` (from `scaffold-harness-canary.yml`); an overlay of the live
   session image with this branch's archive.

The harness release (a StatefulSet, volume and HTTPRoute, applied with Helm as the old
canary harness was) and the push to the shared `scaffold-staging` Worker each need the
owner's word before they're applied.

**Built: pieces 1 and 2 (2026-11-20).** Tested on this Mac only. Nothing has run on Linux or
against staging yet.

- **Launch** (`crates/cli/src/crew_args.rs`; one seam each in `crates/cli/src/lib.rs`,
  `paths.rs` and `env.rs`). The command line takes exactly the native runner's argv
  (`native-crew-runner.mjs`). Its options are global, so they may come before or after the
  subcommand.
  - `--data-dir` (made absolute) and `--ipc-port` replace `ZERON_DATA_DIR`, `ZERON_IPC_PORT`
    and the flavour's defaults across the process, logs included. The refusals still
    apply, and a headless engine now meets them before it opens its log.
  - `--edge-url`, `--scaffold-url`, `--project-scope` and `--deployment-id` name a Scaffold
    authority. `headless --device-bootstrap-file` names a sandbox host
    (`apps/zeron/src/crew_cli.rs`, one call in `engine_config_from_env`).
  - An authority on the command line must be complete. Its origins are https (or loopback
    http), never another Crew's Edge, never beside `ZERON_EDGE_TOKEN`, and never different
    from the data dir's `crew-runtime.json`.
  - The runner's `COMET_DATA_DIR`, `COMET_EDGE_URL` and `COMET_SCAFFOLD_URL` may only
    restate the command line (else exit 78). They never name an authority or a data dir
    themselves (isolation rule 1). The supervisor's own launch (Platform's other path, when
    no native runner is on), which passes the Edge only in `COMET_EDGE_URL`, is refused.
- **Bootstrap** (`crew_cli::configure_device`, Crew's, made stricter):
  - opened without following links: a regular file of 1 B to 16 KiB, private (`0600` or
    tighter) and the user's;
  - Crew's fields only: `deviceJoinGrant`, `projectId`, `deploymentId`, `sessionId`,
    `deviceId`, `lifecycleEpoch`, `sandboxId`, and an optional `edgeUrl`;
  - Scaffold's id pattern, `deviceId` = `comet-scaffold-{sandboxId}-e{lifecycleEpoch}`,
    and a `cg1.<32 hex>.<secret>` grant;
  - bound to the command line's project, deployment and Edge.

  A host's default agent is OMP.
- **Engine** (`crates/engine/src/crew_sandbox.rs`).
  - `EngineConfig.device` is now a `DeviceLaunch`: the grant and the file it came in.
  - `build_auth` exchanges the grant at the Edge (slice 2's code) and removes the bootstrap
    only once that grant is the verified authority.
  - The `cs1.` token lives in `scaffold-device-session.json`: 0600, in Crew's shape, which
    the runner's restart check reads field by field. It's refreshed at every start and
    before it expires. A rejection leaves the tombstone `{}`.
  - The engine runs as the grant's device.
- **Probe**: the hidden `scaffold-authority` calls `ScaffoldHostAuthority` over local IPC.
  It prints the grant as one line, in Crew's key order: `grantId`, `expiresAt`,
  `principalSubject`, `scope` (`projectId`, `deploymentId`, `sessionId`), `sandboxId`,
  `deviceId`, `lifecycleEpoch`, `capabilities`. Logs go to stderr.
  - Crew's binary keeps JSON maps in order (gpui turns on `preserve_order`); this one's sort
    them, so the command re-serializes the typed grant.
  - The RPC answers this device's local IPC only: never a relayed call or an agent's.
- **Guard** (`crew_sandbox::admit`, one seam at the top of `EngineRpc::handle`). On a host:
  - a `chatId` must be its session's. `UploadChunk` and `UploadCommit` need that session and
    `session.files`.
  - this device's credentials and agent installs are refused: Crew's list, plus upstream's
    API keys, ACP agents and harness updates.
  - a relayed call must carry the Edge's `__crewAuthority` for the host's owner, project and
    deployment. A scoped one must also name this device and session, be unexpired and carry
    a grant id, and may not delegate. A peer's may only `DeliverPeerMessage` or
    `LocalDevice`. The envelope is removed before any handler, sign-in changes stay local,
    and an envelope supplied locally is refused.
- **OMP**: `OmpHarness::scaffold_host()`, registered on a host by `crew_inference::install`.
  It runs `--profile scaffold-host --no-extensions --no-skills --no-rules` with `CI=true`,
  without the sandbox's provider keys, `CLAUDE_CONFIG_DIR`, `CODEX_HOME` or broker token,
  and keeps its journals under that profile.
- **Inference**: a host is always managed (the local switch is ignored). Agent Auth is asked
  with the device's own `cs1.` token, through the profile's `ProfileTokens`. Only the host's
  own session runs, on OMP, Claude Code or Codex; titles and other chats are refused. A host
  lists no shared accounts, since Platform binds its route.
- **Lifecycle**:
  - `Engine::run` installs its SIGINT and SIGTERM handlers before the exchange, so a stop
    during startup ends cleanly once the credential is durable.
  - After the engine drains, the headless command gives its runtime's blocking work 2 s.
  - The runner's words appear on an auth failure: "Crew sandbox authentication failed…" for
    a grant refused or unreachable at start, and "headless authentication revoked; stopping
    synced runtime" for one revoked while running.
  - `engine.lock` is unchanged.
- **Archive**: `scripts/crew/package-linux-sandbox <version> [--binary path]`, with versions
  like `0.3.9-zgx.1`. It writes `<target>/package-crew-zgx-linux/comet-<version>-linux-x86_64.tar.gz`
  and `scaffold-manifest.json`. The manifest has schema 2, `scaffold`,
  `scaffold.comet-runtime.v2`, the version, `source` (the commit and crew-web's `origin/main`
  commit) and `files`.
  - `comet` is `zeron`, built in release as upstream's Linux release builds it (fat LTO,
    `LUAU_CXXFLAGS=-DNDEBUG`), in the canary flavour.
  - `crew-web/` is the seven assets from `git archive origin/main apps/web`.
  - `scripts/crew/check-native-crew-archive.mjs` ports `install-native-crew.mjs`'s checks
    and the image builder's manifest gate, and runs after packaging. Platform's own
    installer and validator, run from `scaffold-staging` on a stand-in archive, accepted it.
- **CI**: `.github/workflows/crew-zgx-linux.yml`, on `crew-zgx-linux-<version>` tags
  (`crew-zgx-linux-0.3.9-zgx.1`). On ubuntu-24.04 it installs Crew's apt list, runs the
  checks' own tests, builds and packages, shows `file` and `ldd`, runs `comet --help` and
  `comet headless --help` from the archive, and runs the archive checks. The archive and
  manifest are the artifact (14 days). `actionlint` is clean. Not run yet.
- **What the desktop must match** (Crew main's `scaffold.rs`):
  - write the bootstrap as Crew does: Crew's seven fields and nothing else (an eighth,
    `edgeUrl`, may be added), in the workspace as `.comet-device-bootstrap-<uuid>.json`,
    mode 0600 (`put_file`, then `chmod 600`);
  - exec the runner's 16-element argv in `background` mode, `timeoutMs: 0`, no `env` or
    `cwd`;
  - probe with `.scaffold/crew/bin/comet --data-dir .scaffold/crew-data --ipc-port 39400
    scaffold-authority` (inline, 10 s), until it confirms within 30 s, and compare the
    parsed fields. A host that isn't up yet, or has no verified grant, exits non-zero;
  - mint the join grant with Crew's seven capabilities in Crew's order (the host echoes
    the Edge's);
  - host the session's chat on the grant's device, its id the session id, on `omp`
    (crew-web also wants an absolute `cwd` and `roomGen` 2);
  - send only the owner's calls through the relay, any `chatId` the session's.
- **Open**:
  - The first tagged run is the first Linux build and the first Linux run.
  - Device-token Agent Auth is untested against Platform. There, a `cs1.` authority is
    bound to the session's `agentRoute` (provider, model, account), whatever the chat
    asks for. Its answer's `authorityId` is optional, while the client requires one.
  - Piece 4 must also teach Platform's `crewDeviceSession` (`crew-device-authority.ts`) the
    region's Edge. It verifies a `cs1.` bearer only at `SCAFFOLD_CREW_EDGE_URL` and
    `SCAFFOLD_CREW_CHAT2_EDGE_URL`, while a zgx host's token is this branch's Edge's: Agent
    Auth would answer 403 `crew_device_authority_unavailable`. The Edge's `/auth/session`
    answer for a device has every field that check reads.
  - Not ported: original-epoch history, Crew's command ledger (crew-web sends through
    `ReadCommand`), the OMP supervisor and Scaffold's own OMP inference profile, Antigravity.
  - As in Crew, a transient Edge failure at enrollment or restart ends the epoch: the
    runner reads "Crew sandbox authentication failed" either way.
  - A host doesn't advertise upstream's `cloud-session` capability (the earlier notes'
    plan). Whether its registry row should look like a Cloud session machine's is the
    desktop adapter's call (piece 3).

**Built: piece 3, the desktop (2026-11-21).** Tested on this Mac against a fake Platform and
a fake Edge only; nothing has created a sandbox on staging yet.

- **Files.** Crew's code is in `crates/engine/src/crew_cloud.rs` (the adapter), with
  `crew_scaffold.rs` (Platform's client, the Edge's device grants, Attach) and
  `crew_ledger.rs` (the command record) as its child modules, `crew_workspace.rs` (a child
  of `workspace_host`) and zeron-doc's `registry/crew.rs`. Seams:
  - `rpc.rs`: one field and builder; the Cloud RPCs handed to the adapter before
    upstream's; `cloud_chat_host`'s first lines; `answer_for_cloud` (OMP's agents, never
    the Edge's model list, and a sandbox host's catalog answered here too); and the
    pre-filter at the top of `forward` and `remote_terminal`.
  - `lib.rs`: the module, an `EngineCore` slot, its install in `assemble_runtime` (a synced
    Scaffold desktop, never a host), `rpc_service` and shutdown.
  - `crates/rpc` `device_room.rs`: a `comet-scaffold-…` host is dialed with `LocalDevice {}`
    as its readiness probe, as Crew's are (the Edge refuses `ListHarnesses` there, which
    would end the link it was proving). Every other device keeps upstream's probe.
  - `ScaffoldConfig.sandbox_region`, read by `apps/zeron/src/crew_runtime.rs` from
    `crew-runtime.json`'s optional `sandboxRegion` (a Platform region name).
- **Create** (`POST /api/code-sandboxes`, the `sc_rc_` bearer, no redirects): `region` (the
  configured `sandboxRegion`, and nothing is created without one), `databaseEnvironment:
  local`, `agentRoute` from the chat's config, and `cometRuntimeProfile` `{version:
  scaffold.comet-runtime.v1, projectId, deploymentId, sessionId: <chat id>}`.
  - The route: OMP's own model ids (`anthropic/…`, `openai/…`, `openai-codex/…`; Claude Code's
    and Codex's ids map to their providers), hosted on OMP (no `harness`); the chat's
    `crewAccountId` makes it `pinned`.
  - Platform derives the sandbox from the profile, so a create is idempotent by session: the
    same session and route answer the same sandbox (200); another route is a 409.
  - An answer is used only once it passes Crew's checks (`into_environment`: a
    `remote_code`/`comet_remote` sandbox, a verified owner, valid times, the exact v1
    profile) and is this controller's, in this region, with an epoch.
  - Branches: only `master` (the deployment's default, sent as no `source`).
- **Ledger** (`<profile store>/crew-commands.sqlite3`, Crew's `crew_commands` table): `create:<session>`
  and `attach:<sandbox>:e<epoch>` are claimed before anything is sent and settled once,
  `confirmed` or `rejected`. A 4xx (not 408 or 429), a refused lifecycle or a contract
  mismatch is a rejection; anything else stays unknown. Every 30 s once the registry has
  synced (and on the next ask), unknown commands are only observed: a Create is looked for
  by listing for its session, never sent again; an Attach by probing its host, never minted
  or launched again; a sandbox gone terminal or to another epoch retires it.
- **Attach** (Crew main's, less resume): inspect (a paused, stopped or failed sandbox is
  refused); probe `scaffold-authority` inline and reuse an exact host; else mint a grant at
  this engine's Edge (`POST /auth/device-grants`, Crew's seven capabilities in Crew's order,
  15 minutes), upload Crew's seven bootstrap fields as `.comet-device-bootstrap-<uuid>.json`
  and `chmod 600` it, launch the native runner's 16-element argv in `background` with
  `timeoutMs: 0` (no `env` or `cwd`; `--edge-url` this engine's Edge, `--scaffold-url` the
  authority without a trailing `/`), and probe until it confirms within 30 s. The probe is
  compared field by field (grant id, unexpired, owner, scope, sandbox, device, epoch, the
  capabilities in order). The file is removed afterwards whatever happened. A probe waits
  while Platform says that epoch's runtime is starting.
- **Adapter** (a signed-in, synced Scaffold desktop):
  - `CloudStatus` `{state: ready, deviceId: "cloud", available}`, available only signed in
    with a sandbox region. `CloudEnable` answers the same.
  - `ListGithubRepos`: `Ashler-AI/ashler-platform` (default `master`). `ListCloudBranches`:
    `master`.
  - `CloudSessions`: Platform's list now, this region's only: Creating (and restoring)
    provisioning, Starting (and resuming) starting, Ready (and agent running) ready, Paused
    sleeping, Failed error, Stopped off. Listing refreshes the sandboxes' chat rows.
  - Wake, retry, sleep, delete, sizes, reach, usage and billing refuse with a sentence;
    vault and GitHub calls stay upstream's (unavailable).
  - `cloud_chat_host` (`Mutate createChat` with `cloud`, local IPC only): a UUID chat id, a
    model, the registry synced; then create, publish, attach in the background, and answer
    `{deviceId: comet-scaffold-<sandbox>-e<epoch>, cwd: /workspace/ashler-platform}` with
    the row written. The same chat again answers the same host.
- **The chat row** (Crew's `publish_crew_environment`, less Start and original-epoch
  history; published at create, attach and every refresh, for every epoch): device
  `comet-scaffold-<sandbox>-e<epoch>`, no project, `cwd` `/workspace/ashler-platform`,
  `roomGen` 2, OMP config from the route (`danger-full-access`, `crewAccountId`), and Crew's
  `crew` field beside the typed row: `ownerSubject`, `sessionId`, `scope`, `agentRoute` and
  `environment` (`ownerPrincipal` the verified subject, `source {kind: scaffold, sandbox_id,
  lifecycle, lifecycle_epoch, region, links}`). It never re-homes another device's chat,
  binds another sandbox, or goes back an epoch. The host's device row is written when
  missing (`linux`, `cloud-session`). That is what `bindingMatches` admits the host by.
- **Pre-filter** (`guard_scaffold_target`): a call forwarded to a `comet-scaffold-…` device
  goes only if a chat row binds that host and the call is one the Edge relays to it: its
  session's `chatId`, and `WatchDocMessages`, `WatchQueue`, `FocusChat`, uploads, Crew's
  annotations, `QueueCommand` and `RelayCommand` of a typed kind, the queue (`QueueMessage`,
  `UpdateQueuedMessage`, the edit lease calls, `Move`/`Remove`, `Send`/`SteerQueuedMessageNow`),
  `StopWaiting` and `StopBackgroundTask`. Anything else is refused here, in a sentence.
- **Edge** (`edge/src/crew/authority.ts`, `scopedRpcAllowed`): grown with the queue
  (`session.chat`), `StopWaiting` and `StopBackgroundTask` (`session.control`), and
  `RelayCommand` (the typed kinds, as `QueueCommand`): the desktop's command escort relays
  one when rows are slow, and zgx routes the queue's edits to the chat's host. Needs a new
  `crew-zgx-edge-staging-*` deploy.
- **zgx**: the composer's send goes with a Cloud create (`request`), and the host makes the
  chat's config from it (`chat::merged_config`: agent, model, account). In Crew mode the
  Cloud row shows whenever `CloudStatus` is available and a project's GitHub repository is
  Ashler-AI/ashler-platform; the Cloud and Billing pages and Providers' Cloud choice don't;
  `comet-scaffold-…` devices are Cloud machines, never computers to pick.
- **Pointing a build at it**: `scripts/crew/local-backend staging-edge [canary|dev]` writes
  `crew-runtime.json` for `https://crew-zgx-edge-staging.cgoggins.workers.dev` with
  `"sandboxRegion": "harness-zgx"`, and exits.
- **Open**:
  - (Staging since: "Deployed: piece 4, and the exit gate", below.)
  - Platform's own launch mints a grant at the region's Edge and writes the chat row (its
    clock-0 upsert, which the desktop's row outranks). With that host already serving, the
    desktop's Attach only probes and reuses it; only without it does the desktop mint and
    launch.
  - Attachments to a sandbox host don't work yet: upstream's uploads and the doc host's
    attachment escort send no `chatId`, so the pre-filter refuses them (the escort's, sent
    straight on a link, the Edge refuses).
  - Not here yet: titles for sandbox chats, and changing a chat's model after create
    (Platform binds the host's inference to the create's route). Since 2026-11-24 a send
    resumes a paused sandbox ("Synced with Crew 0.2.155", below). Pause and stop controls,
    and the Web and Tilt views, are left out on purpose. The machine tray follows a
    sandbox's host since 2026-11-22 (below), in upstream's words.

**Deployed: piece 4, and the exit gate (2026-11-21 and 22).** Everything is staging-only and
owner-approved. Nothing changed in production, and Platform has no PR to `master`.

- **Platform** (task branch `czhen/crew-zgx-harness` in `~/ashler-platform-crew-zgx`, local):
  - The control plane picks a region's own Edge (`SCAFFOLD_CREW_REGION_EDGE_URLS`, staging
    `harness,harness-zgx`). `crewDeviceSession` verifies a `cs1.` bearer at that Edge too.
  - It reaches the port's Edge through a service binding, `CREW_ZGX_EDGE` →
    `crew-zgx-edge-staging` (`crewEdgeFetcher`, `crew-edge.ts`). Cloudflare refuses a
    Worker's `fetch` to another `workers.dev` Worker of the same zone (a 404, error 1042),
    so grants failed with `crew_device_grant_rejected:404` before it. The staging deploy
    guard (`deploy-worker.sh`) allows that one binding; production's is untouched.
  - `scaffold-staging` at `1a24d8daa82e…` (master merged, then the task branch), deployed
    worker-only (run 45040880022, bindings `AGENT_AUTH`, `AGENT_AUTH_V2`, `CREW_ZGX_EDGE`).
  - The harness release `scaffold-crew-zgx-harness` (namespace `scaffold-staging`,
    `deploy-harness-crew-zgx.mjs --apply`; `/_scaffold/harness-zgx`). Revision 2 runs
    `scaffold-session-crew-zgx@sha256:2c9a3ae2b71a…`. That is the live
    `scaffold-session@sha256:ac8988011da9…` (checkout `e4e989bca802…`) with
    `0.3.9-zgx.1` (archive `5b452e15d927…`, from `crew-zgx-linux-0.3.9-zgx.1`) in place of
    Crew's engine, in the overlay layout.
  - Revision 1 ran the harness from the image's checkout (`imageLayout: session`), and every
    start failed in "prepare" with "missing session operation response". The harness runs
    `session-ops.mjs` and `session-init.mjs` inside the session's view, from
    `/opt/scaffold/internal/scaffold/harness/node`. That view binds `/opt/scaffold`, not the
    checkout. The overlay now copies the harness there from its base's own checkout (a
    regression check in `build-crew-zgx-session-image.test.mjs`).
  - Rollback: `helm rollback scaffold-crew-zgx-harness 1 --kube-context platform --namespace
    scaffold-staging`.
- **Exit gate, passed 2026-11-22.** Run in the installed canary (`0.3.9`, staging Edge,
  `sandboxRegion: harness-zgx`), from the Cloud row, on Claude Opus 5.5 with automatic
  routing:
  - **Create:** sandbox `rcs_64af2931681bfd231bb4bbed` on `scaffold-crew-zgx-harness-0.e1`,
    Ready 8 s after it was asked for.
  - **Attach:** the host attached 12 s after the send (`run_crew-78840397…`).
  - **Send:** the reply `ZGXSANDBOX7` came back.
  - **Stream:** a 3,000-line reply grew on screen every 2 s.
  - **Interrupt:** Stop ended the turn in about 3 s ("Interrupted"), and the stored reply
    stayed at line 765.
- **Found and fixed:**
  - A failed launch said nothing: the chat held its message as waiting for a day. The tray
    followed only `cloud-…` machines.
  - Now `crew.sandbox_host` lets the chat's Cloud tray follow its sandbox (`afbf0d714`, a
    seam in `cloud_tray.luau`). A failed one shows "Machine failed", with Platform's
    `lifecycleError` when it has one, and "Start a new chat to try again." It offers no Try
    again (`8b1872225`): Crew can't restart a sandbox yet, and the engine refuses Cloud's
    retry with the same words.
  - Checked in the installed canary: the sandbox that failed in "prepare" says "The sandbox
    didn't start: missing session operation response. Start a new chat to try again." A
    fresh GPT-6.2 Sol launch showed the alert within 25 s. The working sandbox's chat shows
    none and still answers.
  - A failed sandbox's chat nudged its host (`POST /device/<host>/nudge`, a 404 each) every
    few seconds while its message waited: upstream's remote-wake retry, which no terminal
    state settled. `crew_cloud::sandbox_retired` (`ed92e9b7c`, a seam in
    `doc_host_delivery.rs`) settles a wake whose host is a failed or stopped sandbox's.
    Measured on the staging Edge (`wrangler tail`): six failed chats sent 136 nudges in 85 s
    before; after, none in 89 s. The engine logged each wake settled as it started.
- **Open:**
  - A launch on GPT-6.2 Sol (`openai`, automatic) fails before its runtime runs
    (`omp_inference_access_required`). Agent Auth answers Platform's authority request 429
    `omp_inference_route_exhausted`. Four ChatGPT Pro accounts were at 100% weekly; the one
    with room, a Team account, wasn't used for it (seen 2026-11-22, `rcs_41079b57…`).
    Claude's routes launch.
  - Platform records no reason on the sandbox for a launch refused before its runtime runs
    (only a session event has it), so that chat says only "Its sandbox failed." A
    `lifecycleError`-like field for launch failures would let it say why.
  - Staging's shared harness, `scaffold-harness` (region `harness`), is on the session
    layout. It likely fails every start the same way: it shows no sessions in five days.
    (Since fixed on Platform's side: #15851 puts the harness under `/opt/scaffold` in the
    session image itself.)
  - Staging holds this testing's sandboxes: six failed, and `rcs_64af…` left running.

**Synced with Crew 0.2.155 and Platform master (2026-11-24).** How Crew and Scaffold work
together changed under the port:

- **Platform enrolls Crew itself.** Every create and resume of a harness session carries
  the Worker's device bootstrap, and the harness enrolls the session's native Crew with it
  once the session is ready (Ashler-AI/ashler-platform#16475). It does what the desktop's
  Attach does. The desktop's Attach still runs: its probe finds that host and reuses it.
  Only without it does the desktop mint and launch one, and the native runner admits one
  launch.
- **Scaffold pauses sessions.** It pauses a Board agent's session when the agent ends, and
  any session idle for 24 h (the Worker's idle sweep, staging and production). So a Cloud
  chat whose sandbox is paused must come back on a send. Crew main does it in its
  composer (#237).
- **The port resumes on send, in the engine** (`crew_cloud::wake`):
  - **Trigger:** a waiting send's nudge answered 404, meaning the host isn't connected
    (a seam in `doc_host_delivery.rs`). The sandbox is woken at most once every 30 s.
  - **Cloud's Wake** (a sleeping chat's line, or a wake that failed) does the same.
  - **The wake:** a paused sandbox is resumed (`POST …/resume`, 5 minutes; Platform runs
    one resume at a time). Its new epoch's chat row is published, so the waiting send goes
    to that epoch's host. Then the host is attached.
  - A failed or stopped sandbox stays ended, in its alert's words.
  - Upstream's sleeping-machine line, "it wakes when you send", is now true of a Crew
    sandbox.
- **A sandbox's host takes no relayed command** (Crew main #231). The desktop's command
  escort stops once the command's attachment bytes land (`crew_cloud::relay_admitted`),
  and the chat doc is the only road. The pre-filter and this branch's Edge refuse
  `RelayCommand` to a session-scoped host, as Crew's own Edge always has. zgx's queue and
  stop calls keep going to the host.
- **Found and fixed: a sandbox host blocked every checkpoint, so it could never pause.**
  - From 2026-11-23 the idle sweep tried to pause `rcs_64af…` every 5 minutes (342
    times by 11-24 21:00Z). Each try failed in the harness's save with "missing
    session operation response".
  - The harness reports any error of its session helper that way. It reads the helper's
    stdout first, so the helper's stderr is lost.
  - Run by hand inside the session, the helper's state archive (`crew-data.py`) refuses
    `~/.codex/tmp/arg0-linux/*`. Those are Codex's dispatch links into
    `/usr/local/lib/node_modules`, outside the snapshot roots.
  - Codex laid them because upstream's harness update checker ran `codex --version` at
    the host's start and every day after, to the second of the links' times.
  - A host now starts no update checker (a seam in `lib.rs`, on
    `profile.device_id_override()`). Platform installs a host's agents, and the host
    refuses their updates already.
  - `crew_sandbox_host.rs` puts fake `codex` and `claude` first on the host's PATH, with
    a system-only PATH behind them, and asserts that neither runs. Without the gate the
    host ran both within seconds.
  - `rcs_64af…` keeps its links: it stays unpausable until it is stopped or its links
    are removed.
  - So does `rcs_c8dc…`, made on the old host the same day. Its save on the harness's
    rollout failed, so the new pod holds it for a pause retry (`checkpointPending` in its
    `state.json`) and starts nothing in it, by design: "never auto-restart during
    capture". Platform still reports it ready, and the pod logs nothing for it, so its
    chat's sends go unanswered. Worth raising with Platform: the control plane isn't told.
- **Found and fixed: each send dropped the desktop's link to a sandbox's host.**
  - The link came up, and 16 ms later the Edge answered `session_scope_denied`. A refused
    frame ends the whole link, with every stream on it.
  - The cause was the plugin system. It calls other devices over the link cache itself
    (`SERVE_PLUGIN_CALL`, `SERVE_PLUGIN_AGENTS`, `PLUGIN_DELIVER`, `CHECK_PLUGIN_KEY`),
    so it bypasses the pre-filter. The built-in `git` plugin's `turn_start` hook calls
    the chat's host on each send; its refusal is in the app's log now.
  - The plugin system now sends a `comet-scaffold-…` host nothing (`20402183f`: a check
    at each of its five link calls, in `plugins/mod.rs`). A service call is refused here
    (`no_service`, "… is a Crew sandbox: plugins don't run there"), a delivery reads
    `not_installed`, and a broadcast skips the host. The pre-filter logs each call it lets
    through to a sandbox host, and each it refuses, at debug.
- **Found and fixed: a new Cloud chat's first send showed a script error.** The bug is
  upstream's, from 2026-11-06, and still in its main (0.3.20).
  - In `ds/transcript.luau`, the prompt's waiting line calls `turn_line`. That is a local
    function made about a hundred lines below, so the call reads a global, nil:
    `ds/transcript.luau:544: attempt to call a nil value`.
  - It fires when the line has words to say in the working word's place, for example a
    turn whose host hasn't said it runs within 15 s of its start. A sandbox's first boot
    takes longer than that.
  - Fixed by declaring `turn_line` above the waiting line (`7a6af26c9`, a seam in an
    upstream file; upstream should take it). Test: `zgx test ui "Cloud machine: a prompt
    whose machine went to sleep before its reply began"`, which fails without the fix with
    staging's error.
  - A quick zgx CLI for the UI tests: build its bin in the app's own workspace, which
    reuses the app's build (50 s): `cargo build --manifest-path apps/zgx/Cargo.toml
    --config apps/zgx/.cargo/config.toml -p zeron-zgx -p zgx-script --bin zeron-zgx
    --bin zgx --target-dir target/zgx`, under `scripts/crew/clean-env` with
    `CREW_FLAVOR=zgx-canary` and `ZIG` as `package-zgx-app` sets them. It is a debug
    build, so the full suite takes about an hour, and it carries the app's `i18n`, so
    `thousands archived …` fails on "2,037" (Status, slice 6).
- **Verified on staging (2026-11-24):**
  - Deployed: this branch's Edge (`crew-zgx-edge-staging-20261124-1`), and harness-zgx
    revision 6. That revision's image is `…@sha256:f5900f55…`, with the Linux archive
    `0.3.9-zgx.3` from `cd0522528`.
  - The desktop: a debug canary of `20402183f`; for the second chat on, with the
    transcript fix (`7a6af26c9`).
  - Two new Cloud chats (`rcs_8b13…`, `rcs_8892…`) each answered their first prompt:
    - In the harness's log, Platform enrolled each host (`crew_enrolled`), and the
      sessions were ready in 15.8 s and 14.4 s. Each restore save and turn save
      succeeded.
    - Neither host's home has a Codex link.
  - A 500-line reply streamed: `running` turned true about 6 s after the send, and Stop
    showed.
  - Stop on a running reply ended it within a second, its words so far kept.
  - No link dropped. The pre-filter refused the composer's `ListRefs` and the GitHub
    plugin's `WatchCheckoutChangeRequest` here, at debug.
  - After the transcript fix, no script error on a new chat's first send.
  - Once, 8 s after a new chat's first reply, the window went back to the chat open
    before it. It didn't happen again.
- **Not verified live: a send resuming a paused sandbox.** It needs a pausable sandbox
  that has been paused, and the idle sweep pauses one only after 24 h without activity
  (`lastActivityAt`: agent heartbeats and touches). Sandboxes from `0.3.9-zgx.3` save, so
  the sweep should pause `rcs_8b13…` and `rcs_8892…` about a day after their last turn
  (2026-11-25, from about 23:25Z). Then a send in their chats ("Reply with exactly the
  word ZGXFRESH1/2…") is the live check. `a_send_to_a_paused_sandbox_resumes_it` covers
  it against a fake Platform.
- **Staging now holds** this testing's sandboxes: the six failed ones, `rcs_64af…` and
  `rcs_c8dc…` (both unpausable, above), and `rcs_8b13…` and `rcs_8892…` (healthy).
  The tag `crew-zgx-linux-0.3.9-zgx.2` names an archive whose build was cancelled.
- **Left out on purpose:**
  - Pause, stop and start controls: Scaffold owns the lifecycle, and a send resumes.
  - The Web and Tilt views: Scaffold opens them on the session's runner, through the
    Worker's attach route (#15858).
- **Platform:** the task branch is rebased onto `master`. Its one conflict was staging's
  regions in `wrangler.toml`, now `harness,harness-zgx` with `harness` the default, as on
  `scaffold-staging`. Master now carries the harness tier itself: native Crew in harness
  sessions (#15857), runner Web and Tilt (#15858), asynchronous pause (#15861), and the
  harness under `/opt/scaffold` (#15851).

**Synced with Crew 0.2.157 and Platform master (2026-11-25).** Crew main's paused-session
work (#239, #242, #243) and Platform's (the paused agent view, #16744, #16739), in the port:

- **Found and fixed: a port sandbox's crew-web loaded nothing.**
  - `package-linux-sandbox` copied a list of crew-web's files of its own. Crew main then
    added two: crew-web's page imports `public/transcript.js` (#236), and Scaffold's paused
    view imports `detached.mjs` (#239). Neither was in the archive.
  - The packager now ships exactly the files crew-web's `contract.json` lists (`assets`).
    `check-native-crew-archive.mjs` refuses an archive missing any of them, on top of the
    installer's own list, and so refuses `0.3.20-zgx.1`.
- **A deliberate headless stop publishes each hosted chat's final tail** (Crew main #243,
  `crew_final_tail.rs`):
  - This host publishes a room's tail on its doc-quiesce tick, a second after a change.
    Scaffold pauses a sandbox by stopping its Crew right after the run, so a turn that
    ended within that second never reached the tail. The tail is all a paused sandbox's
    agent view and iOS read.
  - On a signal or the IPC stop, the engine captures the hosted, joined, stored rooms,
    freezes queues and revokes reachability as before. It then publishes each room's tail
    once, within 5 s, inside the harness's 30 s clean-exit wait.
  - A periodic tail still unsent at the revocation is dropped, so an older transcript
    can't land after the final one. A sign-out publishes nothing.
  - Seams: the module's include and that drop in `doc_host.rs`;
    `EngineRuntime::shutdown_publishing_tails` and the headless stop in `lib.rs`.
  - Tests (`doc_host_sync_tests.rs`): F1, the final tail carries a turn that ended just
    before the stop; F2, a stalled Edge can't hold the stop past its limit; F3, a room
    another device hosts is never published over.
- **A send the Edge only queued wakes the sandbox too.** The port's wake (#237's) fires
  on a nudge's 404: the paused epoch's host never connected, so its device room is
  unclaimed. A send made before the chat's row moved to the paused epoch goes to the last
  host's room instead. That host claimed the room, so the Edge queues the nudge (200,
  `delivered: false`), and nothing woke the sandbox. Now that answer for a sandbox host
  wakes it as well. Its resumed host finds the send at boot. Test:
  `a_send_queued_for_a_paused_sandbox_s_last_host_resumes_it`.
- **Not ported:** #242 itself (the GPUI composer's), and #240 (managed Antigravity's
  persistent home; no managed Antigravity here).
- **Platform:** the task branch is rebased onto `master` `51a030e31` (backup
  `czhen/crew-zgx-harness-pre-rebase-20261125`). Its two conflicts were `index.ts`'s
  imports from `./crew-edge`, both union. Master's paused agent view
  (`crew-web-paused.ts`) serves the shared `harness` region alone, and reads the tail from
  the default chat2 Edge. A harness-zgx session's view still resumes the session to show
  it.
  - Master's chart now mounts users' GitHub PATs into every harness (#16592), and
    `deploy-harness-crew-zgx.mjs` refuses any volume. Its values leave the bundle out
    (`userGithubPatSecretName: ""`, `0e1405d4b`), so a port sandbox doesn't push as its
    user. It never has.
- **Tests:** engine `-- crew_ doc_host:: plugins::service mcp_servers:: profile::`, 237
  passed, 1 ignored, the four new ones among them. `crew_sandbox_host` 3 passed.
  `check-native-crew-archive.test.mjs` 7 passed. Platform: the Crew region tests 25 and
  `build-crew-zgx-session-image.test.mjs` 3 passed, on the rebased branch.
- **Deployed on staging, 2026-11-25:**
  - the Linux archive `0.3.20-zgx.2` (tag `crew-zgx-linux-0.3.20-zgx.2`, run
    45652462133), with every crew-web file its contract lists;
  - the image `scaffold-session-crew-zgx@sha256:ef611198…`, over the shared staging
    harness's base (`scaffold-session@sha256:bc1ba161…`, Platform `966d2f7d`: #16744,
    #16739, Crew 0.2.157);
  - harness-zgx revision 8, with that image and `966d2f7d`'s harness. The new pod adopted
    `rcs_231b…`, `rcs_8892…` and `rcs_8b13…`.
- **Verified on staging:** a new Cloud chat (`rcs_9ac0…`) answered in 21 s. Its host is
  `0.3.20-zgx.2`. Its crew-web has `detached.mjs` and `public/transcript.js` installed
  and is serving. Platform enrolled the host, and its saves succeeded. Its home has no
  link.
- **Not verified live:** the final tail at a stop, and a send resuming a paused sandbox.
  Both need a sandbox stopped or paused, and the idle sweep pauses `rcs_c8dc…` from
  about 22:23Z today, `rcs_8b13…` from 23:03Z and `rcs_8892…` from 23:22Z. `rcs_c8dc…`'s
  pause should now finish, with #16744. `rcs_64af…` is failed in the control plane since
  05:00Z: its pause never finished.
- **Seen, for Platform:** a port sandbox's turns don't move its `last_activity_at`.
  `rcs_8b13…` still reads 2026-11-24 23:03Z after a turn at 11-25 05:47Z, so the idle
  sweep pauses a sandbox 24 h after its last recorded activity even while it's in use.
  A send resumes it.

**Claude Code and Codex in sandboxes (2026-11-27).** Slice 6 scoped a sandbox to OMP: the
Cloud row listed OMP alone (`crew_cloud::HARNESSES`), and a Claude Code or Codex model's
route named no harness, so OMP hosted it. Crew main has run both natively since #204
(2026-11-10), and this port's sandbox host already admitted them on Agent Auth
(`crew_sandbox::inference_allowed`).

- **Now** (`9d60d7b23`): the Cloud row lists Claude Code, Codex and OMP, each on this
  engine's own managed catalog. A Claude Code or Codex chat's route names its harness
  (`agentRoute.harness`, as Platform admits it: anthropic with `claude-code`, openai with
  `codex`), and the sandbox runs that agent on the model as it names it. OMP's routes are
  unchanged.
- **Not here:** Antigravity, which Crew main allows in sandboxes too: its managed
  inference isn't ported.
- **Tests:** `-- crew_ rpc::` 124 passed, including
  `a_claude_code_or_codex_cloud_chat_runs_that_agent_in_its_sandbox`.
- **Verified on staging** with a debug canary of `9d60d7b23`. The Cloud picker's tabs are
  Claude Code, Codex and OMP.
  - A Claude Code chat (`rcs_28f7…`, Opus 5.5) answered in 28 s. Its sandbox runs
    `/usr/local/bin/claude --print --input-format stream-json …`.
  - A Codex chat (`rcs_d34f…`, GPT-6.2-Sol) answered in 22 s, on `codex app-server`.
    Codex laid its `arg0` links in the home, which harness-zgx revision 8 (#16744) keeps
    when it saves.
- **Seen, for Platform:** the harness saves a session at a turn's end by watching OMP's
  transcript folder (`TRANSCRIPT_REL`, `.omp/profiles/scaffold-host/agent/sessions`).
  A Claude Code or Codex turn isn't saved then, only by the timer and at a pause. The
  same holds for Crew main's sandboxes.

**Checked against Crew 0.2.159's Scaffold behaviour (2026-11-27).** Crew main's 0.2.158
(#245) and 0.2.159 (#247) queue a send until its sandbox is up or resumed, in the GPUI
composer. Per the staging session that shipped them (f6a67f32), nothing new is asked of
a host. A paused session's chat points at the paused epoch's never-seen device, whose
nudge answers 404, and the host files a Run's message under its `messageId`. This port
does the same in its engine, live on harness-zgx revision 8:

- **Paused:** two sends to `rcs_9ac0…`'s paused chat (the second during the resume) both
  read "Sending…", with the composer free. The engine resumed the sandbox at once, and
  Platform enrolled the epoch-3 host, ready 16 s after the resume. Both messages were
  answered, in order, about 20 s after the first send.
- **New, then away:** a send to a new Claude Code sandbox, then another chat opened 1.5 s
  later. The message went out once the sandbox was up, and the answer was in the chat on
  return.
- The idle sweep paused `rcs_c8dc…` (the one with Codex's links) at 2026-11-25 22:30Z,
  as #16744 should allow.
- Signed canary `crew-zgx-canary-20261127-1` (`328c76c6e`, run 46065544630, 22 min):
  notarized, stapled and accepted by Gatekeeper on this Mac. Installed in place of the
  ad hoc build, it came up signed in to `ashler-staging` with Cloud ready and its sandbox
  sessions listed.

**harness-zgx revision 9, on the staging base with Crew 0.2.159 (2026-11-27).** The base
is the shared staging candidate `scaffold-session@sha256:b432b3a8…`, from
`scaffold-staging` `bf47629c`. It carries Platform #17336: a session reports ready
before its first save, which now runs after ready. The overlay is
`scaffold-session-crew-zgx@sha256:510de604…`, which adds archive `0.3.20-zgx.2`; the host
hasn't changed since. The task branch's chart still applies: since `51a030e31`,
staging's chart changes only the control plane. The roll waited for the shared
harness's own roll and its timing runs, because both pods sit on one node and a base
copy there would skew its numbers. It went from 22:20:29 to 22:38:05Z. Then, all from
the signed canary:

- **Rollout:** the old pod saved its four live sessions at shutdown, the Codex one
  included. The new pod adopted all four within 25 s of listening, at their own epochs,
  before its base copy finished. Claude Code, Codex and OMP chats each answered within
  6–10 s of a send, on their same devices.
- **Resume:** a send to `rcs_8b13…`, paused since 2026-11-25 on the old base, woke it.
  The harness had it ready 23.9 s later (18.9 s of that restoring the checkpoint), and
  the answer came 36 s after the send. The chat moved to `-e3`. Its first save finished
  64 s after ready, while the turn ran: under the old order, ready would have waited for
  it.
- **New sandbox:** a new Claude Code chat's sandbox (`rcs_aca7…`, route `claude-code`)
  was ready 8.2 s after its start began, its answer came 24 s after the send, and its
  first save finished 9.7 s after ready.
- About 5 minutes after adopting them, the new pod ran one timer save for each Claude
  Code and Codex session (22:27:49–22:28:06Z), including one with no turn since. The OMP
  chat's turn was saved as a turn, 15 s after it.

### Scaffold's own sessions on the new Crew (2026-11-28)

The owner asked to switch staging Scaffold to the new Crew for testing, with both Edges
kept up so Crew Staging (and its sessions) keeps working. They chose to move the
sessions Scaffold makes itself and leave Crew desktops on their own lanes.

**The lane.** Platform's staging Worker takes `SCAFFOLD_CREW_SCAFFOLD_SESSIONS_REGION`
(task branch `b3b8527d2`, `fd405ba2c`; on `scaffold-staging` as `a698184e4`, `9fec99bd`).
A new session gets that region when Scaffold makes it, its request names no region, its
owner saved none, and its database is local. Scaffold makes the Board's and Slack's
sessions through `/api/code-sandboxes` with no client profile, and the web UI's "New
session" through `/api/sessions`, signed in and with no id. The region named must have
its own Crew Edge and a provider, and the setting works on staging only.

Crew desktops keep their regions, so Crew Staging's region-less creates still go to
`harness`. So do snapshot-database sessions and anything naming a region. The flip is
`b2e0a4fe` ("harness-zgx", 04:24:47Z, Worker-only candidate 46107320315). The revert is
that line back to `""`.

**What Scaffold's runner needs from a host.** The Board, Slack and web UI run a task as
a turn in the session's own chat: Platform's `crew-agent.mjs`, over crew-web's `rpc.mjs`
on the host's local IPC (port 39400, the data dir's `ipc-token`). For a session
Scaffold makes, the Worker publishes the chat row to the session's Edge
(`crew-bootstrap.ts`): the `crew` block, the config from the agent route,
`/workspace/ashler-platform`, `roomGen` 2. Three things were missing on this port's host
(`3b78146fa`):

- `QueueCommand` ignored the runner's own `commandId`, so a reconnect started a second
  turn. `crew_commands.rs` queues once under it, refuses another request under it, and
  takes a targeted interrupt's `expectedTurnId`.
- `ReadCommand` didn't exist.
- `WatchChats` rows had no `crew` block (`crew_workspace.rs`). A sandbox host's rows
  now carry it, as Crew's do.

The rest already matched upstream's: `ScaffoldHostAuthority`, the `run`/`interrupt`
payloads with `messageId`, `WatchDocMessages`' full-reset frames, `WatchSessions`, and
`RunRequest`'s fields. Archive `0.3.20-zgx.3` (run 46104090953); overlay
`…crew-zgx@sha256:6ec8467f…` on the same base; harness-zgx revision 10, Ready 04:22Z.

**Run.** The Board session started a fresh task through the lead API on a new thread
(Slack was signed out in Chrome): `bt_7vcezdr67j`, 04:25:17Z. Its session
`rcs_d402c24a…` came up in harness-zgx: Crew host enrolled, ready 6.2 s, first save 9 s,
an OMP turn saved at 04:25:54. The task was done on attempt 1, correct, 52 s end to end.

**Open or different:**
- On harness-zgx, git uses the GitHub App (`github_credential source=github_app`), not
  the requester's token. The release mounts no users' PATs (`0e1405d4b`), and its deploy
  script refuses secret mounts. While the lane is on, a Board round that starts a new
  session and does code work commits, pushes and opens PRs as the App. That covers an
  #alerts-triage fix after "approve" that can't reuse its old session, a Forge lead
  task, and a #cz-test round. Reviews still gate merges. Mounting
  `scaffold-user-github-pats` there is the owner's call.
- Board tasks on Antigravity won't run there: managed Antigravity isn't ported.
- Not checked: a web-UI session. The staging web app needs the owner's Google sign-in.
- harness-zgx's 04:02Z roll cut 5 of 6 shutdown saves (grace 120 s), and the new pod
  left those five `checkpointPending`, "ready" with nothing running. sharp-onyx fixed
  adoption in Platform #17395/#17398. harness-zgx gets the fix, and those five back,
  when its overlay is rebuilt on a base that has it.

Earlier notes:

- Treat a sandbox host as an ordinary peer device (`comet-scaffold-{sandbox}-e{epoch}`)
  carrying the `cloud-session` capability. A Crew adapter sits behind upstream's Cloud
  RPCs, so zgx's existing Cloud row creates and attaches sessions without new Luau.
- Do not reuse upstream's Edge `SandboxProvider`, vault, runner or credits.
- **Main risk:** sandboxes run Platform's pinned `comet headless` runtime (`scaffold.comet-runtime.v2`).
  A v0.3.4 desktop needs a v0.3.4-based runtime archive that Platform admits.
- Create sends no idempotency key, so a lost response must be reconciled by session id and never re-created.

### Upstream merges

**Merged upstream 0.3.20 (2026-11-25).** `e393c4a84..fbb4083a4`: 81 commits, Zeron 0.3.10
to 0.3.20 and two after, in two merges. `e2f795b28` had eight conflicts and `b1ebb2133`
none.

- **The unified Antigravity CLI** (#1577, upstream's 2026-11-23):
  - Google's unified `antigravity` CLI replaces the standalone `agy_acp_server`. Zeron
    pins 1.5.15 (`antigravity_archive()` in `crates/harness/src/acp/mod.rs`, a SHA-512
    per target, eight targets). It keeps the legacy 1.5.0 pin trusted, so an old install
    launches until it updates.
  - It runs as `antigravity acp --disable-telemetry --gemini-dir <home> --app-data-dir
    antigravity-acp`, with `AGY_CLI_DISABLE_AUTO_UPDATE=1`.
  - Its login moves from `acp_token.json` to `antigravity-oauth-token` (migrated once,
    `0600`). A Google sign-in can be a pasted code (`antigravity acp --login`).
  - Zeron updates it from the ACP registry when it can verify the release (#1551): code
    signature on macOS and Windows, a published SHA-256 on Linux. Upstream's own installs
    are at 1.5.16 already (#1595).
  - Checked here: the macOS arm64 pin downloads and matches its SHA-512. Its `antigravity`
    is signed "Developer ID Application: Google LLC (EQHXZ8M8AV)", says `1.5.15`, and
    writes nothing to an empty home.
  - Upstream's #1595 is open: a login made in a terminal, which the unified CLI keeps in
    the macOS Keychain, isn't detected. Zeron looks for Keychain account
    `antigravity-acp`, and 1.5.16 writes `antigravity`.
  - In a Crew build it shows only with own logins (`COMET_INFERENCE_MODE=local`). Crew's
    managed agents are Claude Code, Codex and OMP: Crew's managed Antigravity (Agent Auth)
    isn't ported, and a Crew sandbox runs OMP alone. Crew main still pins `agy_acp_server`
    1.4.4.
- **Conflicts**, each a Crew seam beside an upstream addition:
  - the engine's `hyper` features (the MCP gateway's, and Crew's relay's, which serves);
  - the CLI's hidden commands;
  - the doc registry's modules;
  - the engine registry's and RPC's fields;
  - Cloud's model list: a Crew sandbox's OMP lists this engine's own catalog, never the
    edge's or an agent's fallback;
  - the provider card's props;
  - Zeron v2's base hashes (upstream's, with the composer's blessed for the merged
    default).
- **Crew files brought in step:** OMP's question (`note`) and run request (`code_mode`,
  `zeron_mcp_only`), and the shared login's capture (its provider argument).
- **MCP servers** (upstream's Settings → MCP, the vault's connections, the engine's
  gateway, `zeron mcp`) sync through Zeron Vault, and Crew's Edge has none. For a Scaffold
  profile (`mcp_servers::McpServers::crew`):
  - the engine keeps them on the device, with no vault client, so no run waits on a
    listing that can't answer;
  - it refuses an agent's request for a change, which nobody could answer;
  - Settings → MCP isn't shown;
  - an agent's "Only Zeron's MCP servers" reads "Only Crew's MCP servers": it hides the
    agent's own servers and connectors, so it uses only Crew's tools.

  Whether Crew offers MCP servers, on the device alone or with a vault of its own, is the
  owner's call.
- **Crew's wording:** upstream's ten new messages naming Zeron are kept with why (MCP's,
  and Providers' Cloud key, which Crew doesn't offer). A kept key upstream dropped is gone.
- **Brought in step after the merge** (`2680a9e11`): Crew's shared-account row test
  passes upstream's new `cloud` argument, and Zeron v2's base records the merged default
  composer (`ZERON_V2_BLESS=1 cargo test --bin zeron-zgx zeron_v2_is_in_step`). A new
  test, `profile::crew::tests::a_scaffold_profile_s_mcp_servers_use_no_vault_and_take_no_request`,
  fails with the MCP seam taken out.
- **Tests** (on this Mac, the Rust code as of `2680a9e11`):
  - `cargo check --workspace --tests`: clean.
  - `cargo test -p zeron-engine --lib -- plugins::service crew_ doc_host::remote_delivery
    mcp_servers:: harness_updates:: agent_accounts::`: 368 passed, 3 ignored. The new MCP
    test: 1 passed.
  - `-p zeron --test crew_sandbox_host`: 3 passed. `-p zeron-harness --lib -- crew_omp
    acp::`: 133 passed. `--test crew_omp`: 17 passed, 1 ignored.
  - zgx `cargo test --bin zeron-zgx`: 809 passed, 1 failed, 33 ignored. The failure,
    `transcript_scroll_tests::a_synced_chat_scrolled_up_from_its_end_passes_through_every_message`,
    is upstream's: its CI fails it the same way (run 45532385085, zgx-linux).
  - zgx `--test design_lint --test leftovers`: 1 and 3 passed.
  - Luau UI (`scripts/ci/test-zgx.sh ui`): 1,506 tests, 4 failed.
    - Three are upstream's own, failing in its CI too: "an @ pick goes out as Zeron's
      file mention", and `mention_launcher`'s "Tab and Shift+Tab go through the scopes…"
      and "a click on the launcher's blank room keeps it…".
    - The fourth is `thousands archived …`, which fails on "2,037" with this CLI's `i18n`
      (Status, slice 6).
  - Edge: unit 463, Crew's workerd 23, here and in the deploy's CI.
- **Deployed on staging, 2026-11-25:**
  - this branch's Edge, `crew-zgx-edge-staging-20261125-1` (version `4b68ab6b`);
  - the Linux archive `0.3.20-zgx.1` (tag `crew-zgx-linux-0.3.20-zgx.1`, run
    45546770301), in the image `scaffold-session-crew-zgx@sha256:23170297…` over the same
    base as before (`scaffold-session@sha256:ea8fb0ee…`);
  - harness-zgx revision 7, with that image. The new pod adopted both clean sandboxes
    (`rcs_8892…`, `rcs_8b13…`, `session_adopted` within 30 s), and their saves went on:
    a sandbox without Codex's links survives a rollout.
- **Verified on staging** with a debug canary of `2680a9e11` (0.3.20):
  - signed in as before, with no script error at launch;
  - a new Cloud chat (`rcs_231b…`, host `zeron 0.3.20`, no link in its home): Platform
    enrolled the host, the session was ready in 15.4 s, and its first reply came 25 s
    after the send. Its restore save and each turn's save succeeded;
  - a long reply streamed, and Stop ended it within a second;
  - a chat on a 0.3.9 host (`rcs_8b13…`, adopted across the rollout) answered the 0.3.20
    desktop in 11 s;
  - no link dropped;
  - Settings lists no MCP page, Providers lists Claude Code, Codex and OMP alone, and
    Claude Code's settings read "Only Crew's MCP servers";
  - the engine logged no vault call.

## Development loop

```sh
scripts/crew/build-engine        # zeron-engine -> ~/.crew-zgx-dev/bin
scripts/crew/dev-zgx             # zgx main window, own engine, signed out, local
scripts/crew/dev-zgx gallery     # zgx's component gallery: no engine at all
scripts/crew/local-backend       # Crew Staging's authority + a local Edge, served
scripts/crew/package-zgx-app     # Crew zgx Canary.app, to install by hand
```

- **Zig:** zgx's terminals (Ghostty) need Zig 0.16. Use the official
  aarch64-macos tarball, checksum-verified, in `~/.crew-zgx-dev/zig`, not
  Homebrew's, which pulls in LLVM.
- **Build directories:** builds go through `scripts/local-cargo.py`, in this
  worktree's own `target/` (`CREW_ZGX_TARGET_DIR` names another), not the shared
  one: other worktrees build other trees' crates of the same names there, and a
  path crate built from another tree can look fresh. zgx is its own cargo
  workspace and builds under `<target>/zgx`. A sibling worktree of this branch
  starts warm from an APFS clone (`cp -c -R <this>/target <sibling>/target`,
  then touch what it changed).
- **Engine binary:** build-engine copies it out to `~/.crew-zgx-dev/bin`.
- **Disk:** only debug builds fit on the development Mac. Release packaging goes
  to CI, once approved.

### Testing an installed build

A packaged build installs beside every other Crew and Zeron and touches none of
them: its own bundle id (`ai.ashler.crew.zgx-canary`), URL scheme
(`crew-zgx-canary`), data dir (`~/.crew-zgx-canary`) and IPC port (27959).

1. `scripts/crew/package-zgx-app`, then `ditto` the bundle it names into
   `/Applications/Crew zgx Canary.app`.
2. `scripts/crew/local-backend staging canary` in a terminal of its own. It writes
   `~/.crew-zgx-canary/crew-runtime.json` (Edge `http://127.0.0.1:27961`, authority
   `https://scaffold-staging.internal.ashler.com`, project `ashler-staging`) and the
   Edge's git-ignored vars, then serves the Edge until stopped.
3. Open the app and sign in: the browser signs in to Crew Staging's Scaffold.
   Rooms, sessions and their transcripts live only in the local Edge
   (`edge/.wrangler/state`) and `~/.crew-zgx-canary`.
4. Run a session with Claude Code or Codex. Signed in, every turn goes through
   Crew Staging's Agent Auth (slice 5): the shared pool unless the chat's Account
   option names one. No login on this Mac is used. Other agents refuse, by design,
   until they're ported.
   - For the engine's mock agent instead, launch the app from a terminal with
     `ZERON_ZGX_MOCK=1 "/Applications/Crew zgx Canary.app/Contents/MacOS/zeron"`.

**Driving it from an agent.** Launch it in the background with devtools on, so
it neither takes the person's keys nor opens a browser over their work:

```sh
scripts/crew/clean-env open -g -n --env ZGX_RENDER_WHEN_COVERED=1 --env ZGX_CLIPBOARD=memory \
  --env ZERON_ZGX_NO_BROWSER=/tmp/signin-url.txt "/Applications/Crew zgx Canary.app" --args onboarding --inspect
```

- `clean-env` first. `open` hands the app its caller's environment, and an agent on a
  Crew machine carries that Crew's: its data folder, IPC port, Edge and inference
  tokens. On 2026-11-27, a signed canary launched without it still kept to its own
  folder, port and Edge: it spawns its engine with its own, and `crew-runtime.json`
  names the Edge. But the app process held Crew Staging's tokens.
- `--inspect`, not `ZGX_INSPECT_SOCKET=1`: since upstream 0.3.20, a release build (each
  signed canary) reads development switches in debug builds only (`apps/zgx/src/dev.rs`).
- Drive it with `target/zgx/debug/zgx inspect --app <pid> <command>`, or its devtools
  socket `$TMPDIR/zgx-inspect-<uid>/<pid>.sock`, one JSON request a line (zgx's
  `docs/agents/inspect.md`).
- Devtools serve one window a process. Setup's window goes when setup finishes, so
  relaunch with `main` to drive the main window.
- Sign-in writes its page to the `ZERON_ZGX_NO_BROWSER` file. Open it in a background
  browser tab: with a live Crew Staging session, Scaffold approves at once and redirects
  to the engine's loopback.

**Run on 2026-11-19, from `0bda58b98`.** This is the headed exit gate of slices 2–5.

- Installed beside Crew Staging. The engine ran from the bundle with only its own
  environment: `~/.crew-zgx-canary`, port 27959, callback port 0, its own home.
- Signed in to Crew Staging's Scaffold as the owner, in `ashler-staging`. The engine
  restarted once, read its Edge from `crew-runtime.json`, and joined the registry room
  on the local Edge.
- Setup took Crew's path: look, agents, "ashler-staging is ready".
- Added a scratch project, then ran two Claude Code turns (Opus 5.5):
  - a `Read README.md` tool call, answered with the file's first line;
  - "PONG".
- The chat was titled by the managed model, and its chat2 room joined.
- During a turn, `claude` had `ANTHROPIC_BASE_URL` set to the engine's relay, an
  opaque run token, and no OAuth token or API key. Its only TCP connections were
  local. The engine's only outbound connections were to scaffold-staging's address
  (Agent Auth).
- Found and fixed: the canary shared `crew-zgx-dev`'s app data (`0bda58b98`).
- Found, then fixed in Crew mode (`ui/account/crew.luau` the switch):
  - Setup's first screen was Zeron's. It now welcomes to Crew: Crew's lines, no
    member-card wall, one "Sign in" pill, and Cancel only while the browser has
    the sign-in. There's no "Create an account" or "I already have an account".
  - Zeron's "Star on GitHub" sidebar chip is gone. Settings' last heading reads
    "Crew"; it is still the `Zeron` group, so plugin pages keep listing ahead of it.
  - The bundle leaves out the Parallax scenes plugin, which is Zeron's member-card art.
  - Setup's agents step and Settings → Providers show only the agents a Crew engine
    runs (`MANAGED`, which the harness's `the_app_offers_the_agents_crew_runs` test
    holds to `managed_models`). Each card says "On Crew's shared accounts" in place of
    accounts, connect rows and API keys. There's no other agent to install and no
    registry.
  - Found on the re-run. Crew's account page offered signing keys, which always
    failed ("Couldn't load your signing keys"): a Crew engine keeps no plugin-key
    directory. The section is gone, and signing out asks "Sign out of Crew?".
- Not findings, on a second look:
  - `zeron/` worktree branches are Crew main's too (`crates/engine/src/repos.rs`).
  - The managed Claude catalog has Fable 5.2: upstream's `static_models()` lists all
    10 models. The picker showed only the rows it had mounted.

## Decisions for the owner

Open:

1. **Scaffold authority for test sign-in.**
   - Option one: staging Platform, for identity and Agent Auth only, with rooms on the local Edge.
     The end-to-end runs use this.
   - Option two: the old canary Scaffold. It answers, but nobody has checked whether its authority is still valid.
2. Deploying an Edge to production. (Staging is decided: below.)

Decided by the owner on 2026-11-20:

- **Agent Auth** stays on upstream's `RunEnvironment` hook.
- **Own logins:** managed by default, with Crew's development switch
  (`COMET_INFERENCE_MODE=local`).
- **Titles** stay on Agent Auth, on the cheapest model.
- **Model lists** come from the native harnesses, mapped to Agent Auth, with no
  hard-coded catalog.
- **Codex resume failure:** start a fresh conversation and say so. Upstream now does
  exactly this: "Couldn't restore the earlier Codex conversation …", shown as an error
  row while the turn goes on. It fails only for a dead process or a refusal.
- **Crew's wording:** an overlay file of Crew messages (built: see Status, slice 4).
- **The branch** is pushed to `origin`, and signed canaries are built from
  `crew-zgx-canary-*` tags (`.github/workflows/crew-zgx-canary.yml`).
- **Next:** finish the Agent Auth phase, OMP included.
- **Then (later that day):** slice 6, Scaffold sandboxes. Two staging-only deploys are
  approved: this branch's Edge as its own staging Worker (sharing nothing with Crew's
  Edges), and a scaffold-staging sandbox image whose Crew engine is built from this
  branch. Nothing in production.
- **Default agent:** new chats keep starting on Claude Code; OMP is a choice in the
  composer.

## Status

- [x] 0. Baseline
- [ ] 1. Isolation and flavour
- [x] 2. Crew sign-in: verified on upstream main, 2026-11-19, with slice 5 merged:
  - `cargo test -p zeron-engine --test scaffold_auth --test auth --test local_first`:
    24, 23 and 12 passed. `scaffold_auth` includes the exit gate
    `the_engine_signs_in_with_scaffold_and_reports_the_project_as_its_organization`.
    `local_first`'s two headless tests give an engine 5 s to serve IPC, so they can
    time out when the Mac is loaded and other tests run beside them; alone they pass.
  - `cargo test --lib -p zeron-sync -p zeron-cli -p zeron-launch -p zeron -- scaffold_auth crew`:
    15, 7, 2 and 5 passed. `crew_runtime`'s child test needed its output parsed
    wherever libtest prints it.
  - The headed sign-in, which needs a person, is the installed build's step 3
    (Development loop, "Testing an installed build").
  - Deferred:
    - Crew's rejection fence, `AccessToken { generation }` through `TokenSource::invalidate`.
      Until it lands, an Edge 401 doesn't send the engine back to Scaffold, and a revoked
      `sc_rc_` credential is only noticed at the next engine start. Crew's tests that drive
      the fence are left out.
    - Crew's `crew_guard`.
    - Sandbox (device) hosts: their auth and profile are ported, but not their runtime (owner
      registry, the `--device-bootstrap-file`, original-epoch evidence, the Scaffold client).
      That is slice 6.
- [ ] 3. Edge: built, and tested locally with unit, workerd and `wrangler dev`
  smoke runs. Its exit gate waits for slice 2's signed-in engine to join.
  - After the 2026-11-19 merge: unit 447/447, Crew's workerd 23/23, upstream's
    workerd 423/425. The two failures are throughput tests (6,100 relay frames,
    300 queued wakes) that time out only under heavy machine load; both pass
    alone (53/53). Upstream main and the pre-merge port pass the relay test in
    the same way.
  - Merged Edge served by `scripts/crew/local-backend`: `/auth/session` with no
    bearer or a bogus `sc_rc_` bearer, WorkOS's callback and `/install.sh` all
    answer 401 unauthenticated.
- [ ] 4. zgx Crew mode: tested on upstream main, 2026-11-19.
  - `cargo test --bin zeron-zgx` in `apps/zgx`: 760 passed, 0 failed, 33 ignored.
    That is every upstream test running in Crew mode, plus the `crew::`, `auth::`
    and `engine::manager` tests. The one Crew ignore is upstream's staging-default
    test, which is unreachable here.
  - The integration tests: `design_lint` 1, `leftovers` 3, `onboarding_continue_ink` 1
    and `reload` 16, all passing.
  - Open: the headed exit gate (above), which needs a person to sign in. It is the
    installed build's steps 3 and 4.
  - Since the merge: "Keep chats on this device" (upstream #1432) is refused in
    Crew mode and left out of Settings → General, since a Crew engine's scope
    comes from its Scaffold session.
  - The end-to-end findings fixed (run record above), at `8df181726`:
    - `cargo test --bin zeron-zgx`: 764 passed, 0 failed, 33 ignored. That includes
      the new `crew::` tests: Crew's welcome (`ui/crew_signin_fixture.luau`), setup's
      agents, Settings → Providers, the Crew heading, no star chip, and the account
      page's sign-out.
    - The integration tests `design_lint` 1, `leftovers` 3, `onboarding_continue_ink`
      1, `reload` 16 and `sidebar_row_ink` 1 pass at `ff2b87b68`. `design_lint` and
      `leftovers` pass again after the account page change.
    - `cargo test -p zeron-cli -p zeron-harness --lib -- crew`: 7 and 8 passed. That
      includes `the_app_offers_the_agents_crew_runs` and the packaging check.
    - Upstream's Luau UI tests: 1,470 tests, 0 failed. The command was
      `scripts/ci/test-zgx.sh ui` with the zgx CLI built at the pinned rev from
      cargo's checkout, using that checkout's `.cargo/config.toml`.
  - Crew's wording (2026-11-20):
    - `ui/crew/locales/<locale>.json` holds Crew's words for upstream's messages that
      name Zeron: 114 English and 113 Chinese. A Crew build merges them over
      `ui/locales` into its data folder and loads that (`src/crew_wording.rs`, one
      seam in `src/plugins/mod.rs` `with_catalog`). Upstream's catalog files stay as
      upstream wrote them.
    - `ui/crew/locales/kept.json` lists the 29 that stay Zeron, each with why: the
      "by Zeron" credits, the `.zeron-plugin` and `.zeron-template` formats,
      "Use Zeron v3", the commit credit (`zeronsh[bot]`), and Zeron Cloud, Members,
      invites, member cards and Profile, none of which Crew shows.
    - `crew_wording_tests` fail when an upstream message naming Zeron has neither
      Crew's words nor a reason, or when Crew's no longer match upstream's keys and
      arguments.
    - Still upstream's: Crew's own new messages (the `crew` block at the end of
      `en.json` and `zh-Hans.json`), and the host toast layer's accessible name.
- [ ] 5. Agent Auth: built and unit-tested on 2026-11-19; merged from
  `crew/zgx-thin-port-agent-auth`.
  - `cargo test -p zeron-harness --lib crew_inference`: 7 passed. These cover the
    route env, the strip (a real `env` child too), the Claude and Codex overrides and
    the offline catalogs.
  - `cargo test -p zeron-engine --lib crew_inference`: 36 passed. These cover Crew's
    relay tests that still apply, route reuse and retirement, and the managed
    environment: a spawn getting the relay env only; no route meaning an error; the
    other paths refusing; and an engine without a Scaffold authority unchanged.
  - Open: the exit gate, real turns from zgx through staging Platform's Agent Auth
    (decision 1).
  - Shared accounts (2026-11-20, section above):
    - Tests: `zeron-engine --lib` on `crew_inference crew_accounts agent_accounts
      rpc::tests`, 193 passed. The full `zeron-zgx` bin suite, 770 passed, 0 failed,
      33 ignored, with the new `crew::` tests for the section and the model picker.
      Luau UI 1,470 passed; `design_lint` and `leftovers` pass. The workspace `check
      --all-targets` is clean of new warnings.
    - The installed canary, against staging Agent Auth:
      - The section listed 6 Claude and 5 ChatGPT logins, each once, with plans and
        meters. Accounts with only a Weekly window show 0% for the session, and a
        full one says "Limit reached, resets Tue".
      - Claude's sign-in opened `claude.ai/oauth/authorize` (Claude Code's client, PKCE)
        and asked for the code; cancelled.
      - ChatGPT's opened `auth.openai.com/oauth/authorize` with Codex's loopback on
        1455. Cancelling stopped it.
      - The remove dialog showed "Remove this shared account?". It was opened through
        its state and cancelled, with nothing removed (11 logins before and after).
        Devtools can't press a row of the ⋯ menu in a backgrounded app, as with the
        model picker; the UI test covers the menu → dialog step.
    - Not done: completing a real sign-in or removing a real account (both change
      staging's shared pool).
- [x] 6. Scaffold: pieces 1 and 2 built and tested on this Mac, 2026-11-20 ("Scaffold
  sandboxes", "Built"); piece 3, the desktop, built and tested on this Mac against fakes,
  2026-11-21 ("Built: piece 3"); piece 4 deployed to staging and the exit gate passed there,
  2026-11-22 ("Deployed: piece 4, and the exit gate"); synced with Crew 0.2.155's sandbox
  model and re-verified on staging, 2026-11-24 ("Synced with Crew 0.2.155"). Open: OpenAI
  routes' `omp_inference_route_exhausted`, the reason for a launch refused before its
  runtime, and a live check of resume on send.
  - The sync, 2026-11-24 (Rust code as of `20402183f`):
    - `cargo test -p zeron-engine --lib -- plugins::service crew_ doc_host::remote_delivery`:
      117 passed, with `a_send_to_a_paused_sandbox_resumes_it` and
      `a_crew_sandbox_host_is_sent_no_plugin_call`.
    - `cargo test -p zeron --test crew_sandbox_host`: 3 passed. With the update checker's
      gate taken out, its main test fails on the agent CLIs the host ran.
    - zgx `cargo test --bin zeron-zgx -- crew`: 29 passed.
    - Luau UI (`scripts/ci/test-zgx.sh ui`, with the CLI built in the app's workspace):
      1,471 tests, 1 failed. The failure is `thousands archived: only the rows the page
      shows are built, to the very last`. It expects "2037 sessions", but this CLI
      carries the app's `i18n` feature, which writes "2,037"; with that text the test
      passes. CI's CLI has no `i18n`.
    - Edge: `src/crew/authority.test.ts` 49 passed; `npm run test:unit` 459 passed.
    - Platform, on the rebased task branch: the Worker's suite 635 passed (`master` alone:
      622 of 623); the Crew Edge, region and device tests 25 passed;
      `build-crew-zgx-session-image.test.mjs` 3 passed.
  - The failed-sandbox fixes, 2026-11-22: `cargo test -p zeron-engine --lib -- crew_`: 77
    passed; `-- doc_host::remote_delivery`: 4 passed. zgx `cargo test --bin zeron-zgx --
    crew`: 29 passed; `-- crew sessions chat`: 180 passed, 2 ignored. Luau UI
    (`scripts/ci/test-zgx.sh ui`): 1,470 tests, 0 failed. `check -p zeron-engine
    --all-targets` and `clippy -p zeron-engine --lib`: nothing in the files changed. Platform
    `build-crew-zgx-session-image.test.mjs`: 3 passed.
    - `crew_cloud::scaffold::tests::attach_starts_the_host_as_the_native_runner_takes_it`
      failed once in a full run while the UI suite loaded the machine ("the host answered
      another authority"). It passed 4 of 4 alone, and in the next full run.
  - Piece 3, 2026-11-21:
    - `cargo test -p zeron-engine --lib -- crew_cloud crew_sandbox crew_inference crew_accounts
      rpc::tests`: 98 passed, 1 ignored. `crew_cloud`'s 25: Crew main's mocked-Platform tests
      against a fake Platform and Edge (15), the ledger (1), and the adapter (9: create,
      publish and attach; no region; a lost create found by listing; a rejection settled;
      reconcile observing only; the Cloud RPCs; the row's epochs; the pre-filter; the
      `rpc.rs` seams).
    - `-p zeron-doc --lib registry::crew`: 1. `-p zeron --lib crew`: 11 (`sandboxRegion`
      among them).
    - zgx `cargo test --bin zeron-zgx -- crew sessions chat`: 179 passed, 2 ignored, with the
      new `crew::` tests for the Cloud row and the Settings pages. Luau UI
      (`scripts/ci/test-zgx.sh ui`): 1,470 tests, 0 failed.
    - Edge `npx vitest run`: 459 passed (42 files).
    - `check --locked --workspace --all-targets`: no warnings in the files changed.
  - `cargo test -p zeron --test crew_sandbox_host`: 3 passed. The real binary, started with
    the native runner's argv and variables against a fake Edge and authority:
    - it enrolls and removes the bootstrap, leaving the restart credential;
    - it runs as the grant's device, and the probe prints Crew's exact line;
    - it refuses what a host refuses;
    - SIGTERM ends it with 0 (in 55 ms here);
    - the runner's restart refreshes the credential and serves the same grant;
    - a refused grant ends it with the runner's words, the bootstrap kept.
  - `cargo test -p zeron --lib crew`: 11 passed. `-p zeron-cli --lib`: 28. `-p zeron --test
    cli`: 3.
  - `cargo test -p zeron-engine --lib -- crew_sandbox crew_inference crew_accounts rpc::tests`:
    73 passed. Among them are `crew_sandbox`'s 6: the guard, the probe, and a host's
    enrollment and device-token inference against a fake Scaffold. `--test scaffold_auth`: 24.
  - `cargo test -p zeron-harness --lib crew_omp`: 15 passed.
  - `node --test scripts/crew/check-native-crew-archive.test.mjs`: 6 passed. A stand-in archive
    passed Platform's own `install-native-crew.mjs` and `validate-comet-release-contract.mjs`.
  - `check --locked --workspace --all-targets` shows no new warnings. `actionlint` is clean.
  - Wider runs, with other agents' builds loading the Mac:
    - `-p zeron` (all targets): 26 passed.
    - `-p zeron-harness --lib`: 589 of 590. The miss,
      `process::unexpected_descendants_tests::a_child_that_is_no_configured_mcp_server_is_reported`,
      passes alone. Its orphaned `sleep`s held `local-cargo.py`'s build lock, which cargo's
      children inherit, until they were killed.
    - `-p zeron-engine --test local_first`: 12 of 12 once the load fell (earlier, under
      load, its two headless tests missed their 5 s IPC window).
    - `-p zeron-engine --lib`: 1247 passed, 2 failed, 10 ignored.
      `doc_host::publication_eviction_tests::failed_disk_writes_pin_edits_and_resume_admission_after_recovery`
      passes alone. `chat_persistence::tests::a_kill_mid_save_loses_and_doubles_nothing`
      fails alone too: it kills its re-run test binary after 40 ms, and that binary takes
      about 420 ms here to write its first file. Neither file is in this change.
- [ ] 7. Packaging
  - Local half: `scripts/crew/package-zgx-app` (ad-hoc signed, no updater) and
    `scripts/crew/local-backend`; see "Testing an installed build".
  - Signed builds (approved 2026-11-20): push a `crew-zgx-canary-*` tag.
    `.github/workflows/crew-zgx-canary.yml` builds in release on `macos-latest`. It
    needs macOS 26 and Xcode 26: Xcode 26's actool, which compiles the app icon, crashes
    on macOS 15. The package script signs with Crew's Developer ID and notarizes and
    staples through Crew's release secrets. The zipped app is the run's artifact, kept
    14 days.
  - First: `crew-zgx-canary-20261120-3` (run 44783173811, from `de41c5145`). A 195 MB
    app. Checked on a Mac: `spctl` "accepted, source=Notarized Developer ID", the
    staple valid, team 825LYXGJR6. To install, unzip it into /Applications.
  - With OMP: `crew-zgx-canary-20261120-4` (run 44793705345, from `e7d17b9cb`). 196 MB,
    notarized, the staple valid.
  - With shared accounts: `crew-zgx-canary-20261120-5` (run 44828398975, from
    `c0856a4f5`, 38 minutes). An 86 MB zip; `spctl` "accepted, source=Notarized
    Developer ID", the staple valid, `CrewSourceCommit` the tag's commit.
  - No updater yet. About now says so (a canary is updated by installing a newer
    build), and the app no longer asks its Edge's `/releases`.
  - Packaged 2026-11-19 from `9bca77d5c`: 361 MB debug bundle, `zeron` and
    `zeron-engine` 0.3.9, both the canary flavour.
  - Repackaged from `8df181726` (358 MB, without Parallax scenes) and driven again:
    - Crew's welcome, then "Sign in" through Scaffold staging.
    - Setup's agents, Settings → Providers, the Crew heading and the account page.
    - A Claude Code turn on Agent Auth ("PONG3", Opus 5.5, through the engine's relay).
  - Its engine, started headless in a scratch data dir, served IPC within 2 s.
    It answered `EngineInfo` with `zgx-interface-v1` among its capabilities, and
    `AuthStatus` as `signedOut`.
