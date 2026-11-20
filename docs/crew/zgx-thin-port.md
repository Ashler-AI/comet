# Crew on zgx: thin port

A thin port of Crew's essential behaviour onto upstream Zeron's current engine and
its new desktop interface (`apps/zgx`). It is a testing lane: it must never touch
the daily-driver Crew Staging install, its profile, or the shared staging backend.

## Pins

- Upstream: `zeronsh/zeron` `e393c4a84` (main, 2026-11-19), merged in as `a3dad815f`.
  The port started from `94ae5b10812abb854704bc014c2bcebdec64eac9` (v0.3.4, 2026-11-13);
  the 241 commits since (v0.3.8, the GPUI app's removal in #1499) merged with 19
  conflicts, 18 hunks of them content.
- Crew: `Ashler-AI/comet` `1932df84d` (0.2.151, 2026-11-17).
- Branch: `crew/zgx-thin-port`, local, tracking no remote.
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
    `rpc.rs` (`ListModels`, `ActivateAgentAccount`, `prepare_voice_bridge`) and
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
  - Model lists (`ListModels`, plugins' `models`, titling's model choice) are the
    managed catalog: Claude Code's curated rows, and Crew's OpenAI rows for Codex. Each
    has an Account option: "Automatic · shared pool", then each healthy login from
    `/api/agent-accounts` (cached a minute, 5 s at most, deduplicated as Crew's settings
    do). No CLI is probed.
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

### Scaffold sandboxes

- Treat a sandbox host as an ordinary peer device (`comet-scaffold-{sandbox}-e{epoch}`)
  carrying the `cloud-session` capability. A Crew adapter sits behind upstream's Cloud
  RPCs, so zgx's existing Cloud row creates and attaches sessions without new Luau.
- Do not reuse upstream's Edge `SandboxProvider`, vault, runner or credits.
- **Main risk:** sandboxes run Platform's pinned `comet headless` runtime (`scaffold.comet-runtime.v2`).
  A v0.3.4 desktop needs a v0.3.4-based runtime archive that Platform admits.
- Create sends no idempotency key, so a lost response must be reconciled by session id and never re-created.

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
open -g -n --env ZGX_INSPECT_SOCKET=1 --env ZGX_RENDER_WHEN_COVERED=1 --env ZGX_CLIPBOARD=memory \
  --env ZERON_ZGX_NO_BROWSER=/tmp/signin-url.txt "/Applications/Crew zgx Canary.app" --args onboarding
```

- Drive it through its devtools socket: `$TMPDIR/zgx-inspect-<uid>/<pid>.sock`, one JSON
  request a line (zgx's `docs/agents/inspect.md`).
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

1. **Scaffold authority for test sign-in.**
   - Option one: staging Platform, for identity and Agent Auth only, with rooms on the local Edge.
   - Option two: the old canary Scaffold. It answers, but nobody has checked whether its authority is still valid.
2. Agent Auth through upstream's `RunEnvironment` hook rather than `RunControls` route context.
   Slice 5 is built this way.
3. **Codex resume failure.** Upstream silently starts a fresh conversation; Crew treats it as an error.
4. Approval to push this branch, run build-only CI and deploy any Edge.

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
  - Still Zeron's in Crew mode: 143 English messages that name Zeron (About's
    updates, the Plugins page and others), the menu's "Use Zeron v3", and the host
    toast layer's accessible name. That needs a copy pass, which is the owner's call
    because of upstream conflicts.
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
- [ ] 6. Scaffold
- [ ] 7. Packaging
  - Local half: `scripts/crew/package-zgx-app` (ad-hoc signed, no updater) and
    `scripts/crew/local-backend`; see "Testing an installed build". The signed
    build from CI still needs approval.
  - Packaged 2026-11-19 from `9bca77d5c`: 361 MB debug bundle, `zeron` and
    `zeron-engine` 0.3.9, both the canary flavour.
  - Repackaged from `8df181726` (358 MB, without Parallax scenes) and driven again:
    - Crew's welcome, then "Sign in" through Scaffold staging.
    - Setup's agents, Settings → Providers, the Crew heading and the account page.
    - A Claude Code turn on Agent Auth ("PONG3", Opus 5.5, through the engine's relay).
  - Its engine, started headless in a scratch data dir, served IPC within 2 s.
    It answered `EngineInfo` with `zgx-interface-v1` among its capabilities, and
    `AuthStatus` as `signedOut`.
