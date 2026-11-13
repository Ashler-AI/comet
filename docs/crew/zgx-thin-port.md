# Crew on zgx: thin port

A thin port of Crew's essential behaviour onto upstream Zeron's current engine and
its new desktop interface (`apps/zgx`). It is a testing lane: it must never touch
the daily-driver Crew Staging install, its profile, or the shared staging backend.

## Pins

- Upstream: `zeronsh/zeron` `94ae5b10812abb854704bc014c2bcebdec64eac9` (v0.3.4, 2026-11-13).
- Crew: `Ashler-AI/comet` `3fc058ae23522b6eb09d7b5aac460593b5e6b3c6` (0.2.149, 2026-11-13).
- Branch: `crew/zgx-thin-port`, local, tracking no remote.
- Crew's last upstream import was about v0.2.130 (squash, PR #152), not v0.2.113.
  Compare each Crew file with its nearest upstream version, and audit dropped Crew
  lines with `scripts/audit-upstream-port.py` from the Crew repository.

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
- **Edge protocol** that v0.3.4 expects and Crew's edge lacks:
  - `/chat2/:id/checkpoint-part` and its staged commit;
  - the attachment blob route;
  - caller-device stamping;
  - registry `/chat-host`;
  - unauthenticated `/releases/*`, which the updater and harness-compatibility fetch use.

  Relay frames stay off until the authority checks cover them.

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
scripts/crew/build-engine        # zeron-engine without gpui -> ~/.crew-zgx-dev/bin
scripts/crew/dev-zgx             # zgx main window, own engine, signed out, local
scripts/crew/dev-zgx gallery     # zgx's component gallery: no engine at all
```

- **Zig:** zgx's terminals (Ghostty) need Zig 0.16. Use the official
  aarch64-macos tarball, checksum-verified, in `~/.crew-zgx-dev/zig`, not
  Homebrew's, which pulls in LLVM.
- **Build directories:** builds go through `scripts/local-cargo.py` and the shared
  target dir. zgx is its own cargo workspace and builds under `<target>/zgx`.
- **Engine binary:** it is copied out of the shared target, because another
  worktree's build can replace `target/debug/zeron-engine` there.
- **Disk:** only debug builds fit on the development Mac. Release packaging goes
  to CI, once approved.

## Decisions for the owner

1. **Scaffold authority for test sign-in.**
   - Option one: staging Platform, for identity and Agent Auth only, with rooms on the local Edge.
   - Option two: the old canary Scaffold. It answers, but nobody has checked whether its authority is still valid.
2. Agent Auth through upstream's `RunEnvironment` hook rather than `RunControls` route context.
3. **Codex resume failure.** Upstream silently starts a fresh conversation; Crew treats it as an error.
4. Approval to push this branch, run build-only CI and deploy any Edge.

## Status

- [x] 0. Baseline
- [ ] 1. Isolation and flavour
- [ ] 2. Crew sign-in
- [ ] 3. Edge
- [ ] 4. zgx Crew mode
- [ ] 5. Agent Auth
- [ ] 6. Scaffold
- [ ] 7. Packaging
